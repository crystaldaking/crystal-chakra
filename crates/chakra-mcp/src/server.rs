//! MCP server: typed tools over stdio (ADR-0003).

use std::borrow::Cow;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chakra_domain::envelope::QueryEnvelope;
use chakra_domain::identity::{WorkspaceId, WorkspaceIdentity};
use chakra_domain::operation::OperationContext;
use chakra_domain::query::{
    CallersData, CallersRequest, ContextData, ContextRequest, DiffContextData, DiffContextRequest,
    QueryError, QueryExecutionMetrics, QueryService, RepoMapData, RepoMapRequest, SearchData,
    SearchRequest, StatusData, StatusRequest, SymbolSearchData, SymbolSearchRequest,
    WorkspaceCatalogData, WorkspaceQueryRouter,
};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResponse, CallToolResult};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;

const MAX_CONCURRENT_QUERIES: usize = 2;
const MAX_MCP_ENVELOPE_BYTES: usize = 1024 * 1024;
const QUERY_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_MESSAGE_CHARS: usize = 1_024;

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct WorkspaceTargetRequest {
    /// Required when more than one worktree is registered. Omit for a
    /// single-worktree service.
    #[serde(default)]
    workspace_id: Option<WorkspaceId>,
}

macro_rules! routed_request {
    ($name:ident, $request:ty) => {
        #[derive(Debug, Deserialize, JsonSchema)]
        struct $name {
            /// Required when more than one worktree is registered.
            #[serde(default)]
            workspace_id: Option<WorkspaceId>,
            #[serde(flatten)]
            request: $request,
        }
    };
}

routed_request!(RoutedRepoMapRequest, RepoMapRequest);
routed_request!(RoutedSearchRequest, SearchRequest);
routed_request!(RoutedSymbolSearchRequest, SymbolSearchRequest);
routed_request!(RoutedContextRequest, ContextRequest);
routed_request!(RoutedCallersRequest, CallersRequest);
routed_request!(RoutedDiffContextRequest, DiffContextRequest);

#[derive(Debug, Default)]
struct QueryMetricsState {
    queued: AtomicU64,
    running: AtomicU64,
    started: AtomicU64,
    cancelled: AtomicU64,
    queue_timed_out: AtomicU64,
    execution_timed_out: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    permit_hold_micros_total: AtomicU64,
    permit_hold_micros_max: AtomicU64,
}

impl QueryMetricsState {
    fn snapshot(&self) -> QueryExecutionMetrics {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        QueryExecutionMetrics {
            queued: load(&self.queued),
            running: load(&self.running),
            started: load(&self.started),
            cancelled: load(&self.cancelled),
            queue_timed_out: load(&self.queue_timed_out),
            execution_timed_out: load(&self.execution_timed_out),
            completed: load(&self.completed),
            failed: load(&self.failed),
            permit_hold_micros_total: load(&self.permit_hold_micros_total),
            permit_hold_micros_max: load(&self.permit_hold_micros_max),
        }
    }

    fn record_permit_hold(&self, elapsed: Duration) {
        let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        self.permit_hold_micros_total
            .fetch_add(micros, Ordering::Relaxed);
        let mut maximum = self.permit_hold_micros_max.load(Ordering::Relaxed);
        while micros > maximum {
            match self.permit_hold_micros_max.compare_exchange_weak(
                maximum,
                micros,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => maximum = observed,
            }
        }
    }
}

struct ClientCancellationGuard {
    operation: OperationContext,
    metrics: Arc<QueryMetricsState>,
    armed: bool,
}

impl ClientCancellationGuard {
    fn new(operation: OperationContext, metrics: Arc<QueryMetricsState>) -> Self {
        Self {
            operation,
            metrics,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ClientCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.operation.cancel();
            self.metrics.cancelled.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct CounterGauge {
    counter: Arc<QueryMetricsState>,
    queued: bool,
}

impl CounterGauge {
    fn queued(counter: Arc<QueryMetricsState>) -> Self {
        counter.queued.fetch_add(1, Ordering::Relaxed);
        Self {
            counter,
            queued: true,
        }
    }

    fn running(counter: Arc<QueryMetricsState>) -> Self {
        counter.running.fetch_add(1, Ordering::Relaxed);
        Self {
            counter,
            queued: false,
        }
    }
}

impl Drop for CounterGauge {
    fn drop(&mut self) {
        let gauge = if self.queued {
            &self.counter.queued
        } else {
            &self.counter.running
        };
        gauge.fetch_sub(1, Ordering::Relaxed);
    }
}

struct PermitHoldGuard {
    metrics: Arc<QueryMetricsState>,
    started: Instant,
    _running: CounterGauge,
    finished: bool,
}

impl PermitHoldGuard {
    fn new(metrics: Arc<QueryMetricsState>, started: Instant) -> Self {
        metrics.started.fetch_add(1, Ordering::Relaxed);
        Self {
            _running: CounterGauge::running(metrics.clone()),
            metrics,
            started,
            finished: false,
        }
    }

    fn finish<T>(&mut self, result: &Result<T, QueryError>) {
        match result {
            Ok(_) => &self.metrics.completed,
            Err(QueryError::ExecutionDeadlineExceeded) => &self.metrics.execution_timed_out,
            Err(QueryError::Cancelled) => {
                self.finished = true;
                return;
            }
            Err(_) => &self.metrics.failed,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.finished = true;
    }
}

impl Drop for PermitHoldGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.metrics.failed.fetch_add(1, Ordering::Relaxed);
        }
        self.metrics.record_permit_hold(self.started.elapsed());
    }
}

/// MCP server handle. Cloneable so transports can share one query service.
#[derive(Clone)]
pub struct ChakraMcpServer {
    router: Arc<dyn WorkspaceQueryRouter>,
    query_slots: Arc<Semaphore>,
    query_metrics: Arc<QueryMetricsState>,
    queue_timeout: Duration,
    execution_timeout: Duration,
    tool_router: ToolRouter<Self>,
}

struct SingleWorkspaceRouter {
    service: Arc<dyn QueryService>,
}

impl WorkspaceQueryRouter for SingleWorkspaceRouter {
    fn workspaces(&self) -> Result<Vec<WorkspaceIdentity>, QueryError> {
        Ok(vec![self.service.status(StatusRequest)?.data.workspace])
    }

    fn route(&self, requested: Option<&WorkspaceId>) -> Result<Arc<dyn QueryService>, QueryError> {
        if let Some(requested) = requested {
            let current = self.service.status(StatusRequest)?.workspace_id;
            if requested != &current {
                return Err(QueryError::WorkspaceNotFound(requested.clone()));
            }
        }
        Ok(self.service.clone())
    }
}

fn to_error_data(error: QueryError) -> ErrorData {
    match error {
        QueryError::Invalid(_)
        | QueryError::MissingSymbolRef
        | QueryError::StaleSymbolRef { .. }
        | QueryError::StaleCursor { .. }
        | QueryError::CursorWorkspaceMismatch { .. }
        | QueryError::SymbolNotFound(_)
        | QueryError::AmbiguousSymbol { .. }
        | QueryError::FreshnessNotMet { .. }
        | QueryError::WorkspaceNotFound(_)
        | QueryError::WorkspaceSelectionRequired { .. } => {
            ErrorData::invalid_params(error.to_string(), None)
        }
        QueryError::Unsupported(_)
        | QueryError::FreshnessUnavailable(_)
        | QueryError::DiffUnavailable(_)
        | QueryError::ResponseConstruction(_)
        | QueryError::NoWorkspacesRegistered => ErrorData::internal_error(error.to_string(), None),
        QueryError::Cancelled => execution_error("client_cancelled", error.to_string()),
        QueryError::ExecutionDeadlineExceeded => {
            execution_error("execution_deadline", error.to_string())
        }
    }
}

fn execution_error(kind: &'static str, message: impl Into<String>) -> ErrorData {
    let message: String = message
        .into()
        .chars()
        .take(MAX_ERROR_MESSAGE_CHARS)
        .collect();
    ErrorData::internal_error(message, Some(serde_json::json!({ "kind": kind })))
}

/// Structured response already converted to the protocol's JSON value.
/// Unlike rmcp's `Json<T>`, this wrapper lets Chakra validate the exact wire
/// size without serializing the typed envelope into a counting writer first.
struct BudgetedJson<T> {
    value: serde_json::Value,
    marker: PhantomData<T>,
}

impl<T: JsonSchema> JsonSchema for BudgetedJson<T> {
    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        T::json_schema(generator)
    }
}

impl<T: JsonSchema + 'static> IntoCallToolResult for BudgetedJson<T> {
    fn into_call_tool_result(self) -> Result<CallToolResponse, ErrorData> {
        let mut result = CallToolResult::default();
        result.structured_content = Some(self.value);
        result.is_error = Some(false);
        Ok(result.into())
    }
}

fn encoded_string_len(value: &str) -> usize {
    value.chars().fold(2_usize, |bytes, character| {
        bytes.saturating_add(match character {
            '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
            '\u{00}'..='\u{1f}' => 6,
            other => other.len_utf8(),
        })
    })
}

/// Exact compact JSON length for a protocol value, without serializing it a
/// second time or allocating a full encoded response buffer.
fn encoded_json_len(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(true) => 4,
        serde_json::Value::Bool(false) => 5,
        serde_json::Value::Number(number) => number.to_string().len(),
        serde_json::Value::String(string) => encoded_string_len(string),
        serde_json::Value::Array(items) => items
            .iter()
            .fold(2_usize, |bytes, item| {
                bytes.saturating_add(encoded_json_len(item))
            })
            .saturating_add(items.len().saturating_sub(1)),
        serde_json::Value::Object(entries) => entries
            .iter()
            .fold(2_usize, |bytes, (key, value)| {
                bytes
                    .saturating_add(encoded_string_len(key))
                    .saturating_add(1)
                    .saturating_add(encoded_json_len(value))
            })
            .saturating_add(entries.len().saturating_sub(1)),
    }
}

impl<T> BudgetedJson<T>
where
    T: Serialize + JsonSchema + 'static,
{
    fn new(envelope: T) -> Result<Self, ErrorData> {
        let serialization_started = Instant::now();
        let value = serde_json::to_value(envelope).map_err(|error| {
            ErrorData::internal_error(
                format!("failed to serialize structured response: {error}"),
                None,
            )
        })?;
        let serialization_micros =
            u64::try_from(serialization_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let budget_started = Instant::now();
        let response_bytes = encoded_json_len(&value);
        let budget_check_micros =
            u64::try_from(budget_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        tracing::debug!(
            response_bytes,
            serialization_micros,
            budget_check_micros,
            transport_serialization = "rmcp_owned",
            "MCP structured response prepared"
        );
        if response_bytes > MAX_MCP_ENVELOPE_BYTES {
            return Err(ErrorData::internal_error(
                format!(
                    "query envelope exceeds the {MAX_MCP_ENVELOPE_BYTES}-byte MCP budget; lower the requested limit"
                ),
                None,
            ));
        }
        Ok(Self {
            value,
            marker: PhantomData,
        })
    }
}

#[tool_router]
impl ChakraMcpServer {
    pub fn new(service: Arc<dyn QueryService>) -> Self {
        Self::with_workspace_router(Arc::new(SingleWorkspaceRouter { service }))
    }

    pub fn with_workspace_router(router: Arc<dyn WorkspaceQueryRouter>) -> Self {
        Self {
            router,
            query_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_QUERIES)),
            query_metrics: Arc::new(QueryMetricsState::default()),
            queue_timeout: QUERY_QUEUE_TIMEOUT,
            execution_timeout: QUERY_EXECUTION_TIMEOUT,
            tool_router: Self::tool_router(),
        }
    }

    #[cfg(test)]
    fn with_timeouts(
        service: Arc<dyn QueryService>,
        queue_timeout: Duration,
        execution_timeout: Duration,
    ) -> Self {
        Self {
            router: Arc::new(SingleWorkspaceRouter { service }),
            query_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_QUERIES)),
            query_metrics: Arc::new(QueryMetricsState::default()),
            queue_timeout,
            execution_timeout,
            tool_router: Self::tool_router(),
        }
    }

    async fn execute_query<T, F>(
        &self,
        workspace_id: Option<WorkspaceId>,
        query: F,
    ) -> Result<BudgetedJson<QueryEnvelope<T>>, ErrorData>
    where
        T: Send + Serialize + JsonSchema + 'static,
        F: FnOnce(&dyn QueryService, &OperationContext) -> Result<QueryEnvelope<T>, QueryError>
            + Send
            + 'static,
    {
        let service = self
            .router
            .route(workspace_id.as_ref())
            .map_err(to_error_data)?;
        let operation = OperationContext::with_timeout(self.execution_timeout);
        let mut cancellation =
            ClientCancellationGuard::new(operation.clone(), self.query_metrics.clone());
        let queued = CounterGauge::queued(self.query_metrics.clone());
        let permit = match tokio::time::timeout(
            self.queue_timeout,
            self.query_slots.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                drop(queued);
                cancellation.disarm();
                return Err(ErrorData::internal_error(
                    "query executor is shutting down",
                    None,
                ));
            }
            Err(_) => {
                drop(queued);
                self.query_metrics
                    .queue_timed_out
                    .fetch_add(1, Ordering::Relaxed);
                cancellation.disarm();
                return Err(execution_error(
                    "queue_timeout",
                    format!(
                        "query did not acquire an execution slot within {} seconds",
                        self.queue_timeout.as_secs_f64()
                    ),
                ));
            }
        };
        drop(queued);
        let metrics = self.query_metrics.clone();
        let blocking_operation = operation.clone();
        let hold = PermitHoldGuard::new(metrics, Instant::now());
        let joined = tokio::task::spawn_blocking(move || -> Result<_, ErrorData> {
            let _permit = permit;
            let mut hold = hold;
            let result = query(service.as_ref(), &blocking_operation);
            hold.finish(&result);
            let envelope = result.map_err(to_error_data)?;
            BudgetedJson::new(envelope)
        })
        .await
        .map_err(|error| ErrorData::internal_error(format!("query worker failed: {error}"), None));
        cancellation.disarm();
        joined?
    }

    #[tool(
        name = "workspaces",
        description = "List the independently owned materialized Git worktrees available to Chakra",
        output_schema = rmcp::handler::server::tool::schema_for_output::<WorkspaceCatalogData>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workspaces(&self) -> Result<BudgetedJson<WorkspaceCatalogData>, ErrorData> {
        let workspaces = self.router.workspaces().map_err(to_error_data)?;
        BudgetedJson::new(WorkspaceCatalogData { workspaces })
    }

    #[tool(
        name = "status",
        description = "Chakra worktree status: identity, published revision, index counts, provider state, live indexing diagnostics; pass workspace_id when several worktrees are registered",
        output_schema = rmcp::handler::server::tool::schema_for_output::<QueryEnvelope<StatusData>>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn status(
        &self,
        Parameters(target): Parameters<WorkspaceTargetRequest>,
    ) -> Result<BudgetedJson<QueryEnvelope<StatusData>>, ErrorData> {
        let service = self
            .router
            .route(target.workspace_id.as_ref())
            .map_err(to_error_data)?;
        let mut envelope = service.status(StatusRequest).map_err(to_error_data)?;
        envelope.data.query_execution = Some(self.query_metrics.snapshot());
        BudgetedJson::new(envelope)
    }

    #[tool(
        name = "repo_map",
        description = "Browse indexed supported-language structure with a bounded overview, filters, and revision-scoped cursor pages",
        output_schema = rmcp::handler::server::tool::schema_for_output::<QueryEnvelope<RepoMapData>>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn repo_map(
        &self,
        Parameters(request): Parameters<RoutedRepoMapRequest>,
    ) -> Result<BudgetedJson<QueryEnvelope<RepoMapData>>, ErrorData> {
        self.execute_query(request.workspace_id, move |service, operation| {
            service.repo_map_with_context(request.request, operation)
        })
        .await
    }

    #[tool(
        name = "search",
        description = "Search the atomically indexed source snapshot using literal or regex text matching",
        output_schema = rmcp::handler::server::tool::schema_for_output::<QueryEnvelope<SearchData>>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn search(
        &self,
        Parameters(request): Parameters<RoutedSearchRequest>,
    ) -> Result<BudgetedJson<QueryEnvelope<SearchData>>, ErrorData> {
        self.execute_query(request.workspace_id, move |service, operation| {
            service.search_with_context(request.request, operation)
        })
        .await
    }

    #[tool(
        name = "symbol_search",
        description = "Find bounded supported-language syntax symbol candidates by simple or qualified name; match_mode=exact restricts matching to the exact-name index",
        output_schema = rmcp::handler::server::tool::schema_for_output::<QueryEnvelope<SymbolSearchData>>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn symbol_search(
        &self,
        Parameters(request): Parameters<RoutedSymbolSearchRequest>,
    ) -> Result<BudgetedJson<QueryEnvelope<SymbolSearchData>>, ErrorData> {
        self.execute_query(request.workspace_id, move |service, operation| {
            service.symbol_search_with_context(request.request, operation)
        })
        .await
    }

    #[tool(
        name = "context",
        description = "Get bounded syntax context for one resolved supported-language symbol, with optional current precise enrichment when supported",
        output_schema = rmcp::handler::server::tool::schema_for_output::<QueryEnvelope<ContextData>>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn context(
        &self,
        Parameters(request): Parameters<RoutedContextRequest>,
    ) -> Result<BudgetedJson<QueryEnvelope<ContextData>>, ErrorData> {
        self.execute_query(request.workspace_id, move |service, operation| {
            service.context_with_context(request.request, operation)
        })
        .await
    }

    #[tool(
        name = "callers",
        description = "Get bounded callers for one resolved supported-language symbol, preferring current provider precision when supported and retaining honest syntax fallback",
        output_schema = rmcp::handler::server::tool::schema_for_output::<QueryEnvelope<CallersData>>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn callers(
        &self,
        Parameters(request): Parameters<RoutedCallersRequest>,
    ) -> Result<BudgetedJson<QueryEnvelope<CallersData>>, ErrorData> {
        self.execute_query(request.workspace_id, move |service, operation| {
            service.callers_with_context(request.request, operation)
        })
        .await
    }

    #[tool(
        name = "diff_context",
        description = "Summarize bounded supported-language changes from HEAD, a base ref, or a merge base to the materialized worktree, with changed symbols and related callers/tests",
        output_schema = rmcp::handler::server::tool::schema_for_output::<QueryEnvelope<DiffContextData>>(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn diff_context(
        &self,
        Parameters(request): Parameters<RoutedDiffContextRequest>,
    ) -> Result<BudgetedJson<QueryEnvelope<DiffContextData>>, ErrorData> {
        self.execute_query(request.workspace_id, move |service, operation| {
            service.diff_context_with_context(request.request, operation)
        })
        .await
    }
}

#[tool_handler(
    name = "chakra",
    instructions = "Chakra multi-language code intelligence: inspect workspaces, status, and repo_map; search indexed source; resolve ambiguous names through symbol_search; request context or callers for one entity; and use diff_context for current worktree or branch-relative changes. When workspaces lists more than one worktree, pass its workspace_id to every workspace query. Results are bounded and carry language, revision, freshness, provider state and capabilities, provenance, and precision.",
    router = self.tool_router
)]
impl ServerHandler for ChakraMcpServer {}

/// Why the stdio server stopped with an error.
#[derive(Debug, Error)]
pub enum ServeError {
    #[error("failed to initialize the MCP stdio server: {0}")]
    Init(String),
    #[error("MCP stdio server failed while running: {0}")]
    Runtime(String),
}

/// Serves MCP over stdin/stdout until the client disconnects.
///
/// Writes nothing to stdout itself: the transport owns the stream. Logging
/// goes to stderr (configured by the CLI).
pub async fn serve_stdio(service: Arc<dyn QueryService>) -> Result<(), ServeError> {
    let server = ChakraMcpServer::new(service);
    serve_server(server).await
}

/// Serves a multi-worktree router over stdin/stdout until the client
/// disconnects. Each tool request is resolved to one worktree before it
/// enters the ordinary query executor.
pub async fn serve_stdio_router(router: Arc<dyn WorkspaceQueryRouter>) -> Result<(), ServeError> {
    let server = ChakraMcpServer::with_workspace_router(router);
    serve_server(server).await
}

async fn serve_server(server: ChakraMcpServer) -> Result<(), ServeError> {
    let running = server
        .serve(stdio())
        .await
        .map_err(|error| ServeError::Init(error.to_string()))?;
    running
        .waiting()
        .await
        .map_err(|error| ServeError::Runtime(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;

    use chakra_domain::identity::WorkspaceIdentity;
    use chakra_domain::query::StatusData;
    use chakra_engine::WorkspaceEngine;

    use super::*;

    static SERIALIZE_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct TestWorkspaceRouter {
        entries: Vec<(WorkspaceIdentity, Arc<WorkspaceEngine>)>,
    }

    impl WorkspaceQueryRouter for TestWorkspaceRouter {
        fn workspaces(&self) -> Result<Vec<WorkspaceIdentity>, QueryError> {
            Ok(self
                .entries
                .iter()
                .map(|(identity, _)| identity.clone())
                .collect())
        }

        fn route(
            &self,
            requested: Option<&WorkspaceId>,
        ) -> Result<Arc<dyn QueryService>, QueryError> {
            let requested = requested.ok_or_else(|| QueryError::WorkspaceSelectionRequired {
                available: self
                    .entries
                    .iter()
                    .map(|(identity, _)| identity.workspace.clone())
                    .collect(),
            })?;
            self.entries
                .iter()
                .find(|(identity, _)| &identity.workspace == requested)
                .map(|(_, engine)| engine.clone() as Arc<dyn QueryService>)
                .ok_or_else(|| QueryError::WorkspaceNotFound(requested.clone()))
        }
    }

    struct CountedPayload;

    impl Serialize for CountedPayload {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            SERIALIZE_CALLS.fetch_add(1, Ordering::SeqCst);
            serializer.serialize_str("counted")
        }
    }

    impl JsonSchema for CountedPayload {
        fn schema_name() -> Cow<'static, str> {
            String::schema_name()
        }

        fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
            String::json_schema(generator)
        }
    }

    #[tokio::test]
    async fn workspace_catalog_requires_explicit_routing_when_multiple_are_ready()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let temp = tempfile::tempdir()?;
        let first_root = temp.path().join("first");
        let second_root = temp.path().join("second");
        fs::create_dir_all(&first_root)?;
        fs::create_dir_all(&second_root)?;
        let first = WorkspaceIdentity::for_primary_worktree(&first_root)?;
        let second = WorkspaceIdentity::for_primary_worktree(&second_root)?;
        let server = ChakraMcpServer::with_workspace_router(Arc::new(TestWorkspaceRouter {
            entries: vec![
                (first.clone(), Arc::new(WorkspaceEngine::new(first.clone()))),
                (
                    second.clone(),
                    Arc::new(WorkspaceEngine::new(second.clone())),
                ),
            ],
        }));

        let catalog = server.workspaces().await?;
        assert_eq!(
            catalog.value["workspaces"].as_array().map(Vec::len),
            Some(2)
        );

        let missing = server
            .status(Parameters(WorkspaceTargetRequest::default()))
            .await;
        let error = match missing {
            Ok(_) => return Err("unscoped status unexpectedly succeeded".into()),
            Err(error) => error,
        };
        assert!(error.message.contains("specify workspace_id"));

        let selected = server
            .status(Parameters(WorkspaceTargetRequest {
                workspace_id: Some(second.workspace.clone()),
            }))
            .await?;
        assert_eq!(
            selected.value["workspace_id"],
            serde_json::json!(second.workspace)
        );
        Ok(())
    }

    #[test]
    fn routed_tool_requests_keep_the_existing_flat_json_shape()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let request: RoutedSearchRequest = serde_json::from_value(serde_json::json!({
            "workspace_id": "workspace-a",
            "query": "needle",
            "limit": 7
        }))?;
        assert_eq!(
            request.workspace_id.map(|id| id.to_string()).as_deref(),
            Some("workspace-a")
        );
        assert_eq!(request.request.query, "needle");
        assert_eq!(request.request.limit, Some(7));
        Ok(())
    }

    #[test]
    fn exact_value_size_matches_serde_json_for_escaped_and_multibyte_content()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let controls: String = (0_u8..=31).map(char::from).collect();
        let values = [
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(-123.5),
            serde_json::json!(format!(
                "quote=\" slash=\\ {controls} 🦀 界 \u{2028}\u{2029}"
            )),
            serde_json::json!({
                "multibyte-界": ["🦀", "line\nfeed", {"nested": false}],
            }),
        ];
        for value in values {
            assert_eq!(encoded_json_len(&value), serde_json::to_vec(&value)?.len());
        }
        Ok(())
    }

    #[test]
    fn budget_boundary_serializes_the_typed_payload_once()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        SERIALIZE_CALLS.store(0, Ordering::SeqCst);
        let response = BudgetedJson::<CountedPayload>::new(CountedPayload)?;
        assert_eq!(response.value, serde_json::json!("counted"));
        assert_eq!(SERIALIZE_CALLS.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_a_queued_query_prevents_dispatch()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let identity = WorkspaceIdentity::for_primary_worktree(std::path::Path::new("."))?;
        let server = ChakraMcpServer::new(Arc::new(WorkspaceEngine::new(identity)));
        let first = server.query_slots.clone().acquire_owned().await?;
        let second = server.query_slots.clone().acquire_owned().await?;
        let called = Arc::new(AtomicBool::new(false));
        let task_called = called.clone();
        let task_server = server.clone();
        let task = tokio::spawn(async move {
            task_server
                .execute_query::<StatusData, _>(None, move |_, _| {
                    task_called.store(true, Ordering::Release);
                    Err(QueryError::Unsupported("cancelled test query"))
                })
                .await
        });

        tokio::task::yield_now().await;
        task.abort();
        let cancellation = task.await;
        assert!(cancellation.is_err_and(|error| error.is_cancelled()));
        drop((first, second));
        tokio::task::yield_now().await;
        assert!(!called.load(Ordering::Acquire));
        let metrics = server.query_metrics.snapshot();
        assert_eq!(metrics.queued, 0);
        assert_eq!(metrics.running, 0);
        assert_eq!(metrics.cancelled, 1);
        assert_eq!(metrics.started, 0);
        Ok(())
    }

    #[tokio::test]
    async fn oversized_serialized_response_is_rejected()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let identity = WorkspaceIdentity::for_primary_worktree(std::path::Path::new("."))?;
        let workspace_id = identity.workspace.clone();
        let server = ChakraMcpServer::new(Arc::new(WorkspaceEngine::new(identity)));
        let result = server
            .execute_query::<String, _>(None, move |_, _| {
                Ok(QueryEnvelope::new(
                    workspace_id,
                    chakra_domain::revision::Revision(1),
                    chakra_domain::state::Freshness::Fresh,
                    chakra_domain::state::WorkspaceStatus::Ready,
                    chakra_domain::state::ProviderState::NotConfigured,
                    Vec::new(),
                    "x".repeat(MAX_MCP_ENVELOPE_BYTES),
                ))
            })
            .await;
        let error = match result {
            Ok(_) => return Err("oversized response unexpectedly succeeded".into()),
            Err(error) => error,
        };
        assert!(error.message.contains("MCP budget"));
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_two_running_queries_releases_both_permits()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let identity = WorkspaceIdentity::for_primary_worktree(std::path::Path::new("."))?;
        let workspace_id = identity.workspace.clone();
        let server = ChakraMcpServer::new(Arc::new(WorkspaceEngine::new(identity)));
        let (started, observed) = mpsc::channel();

        let first_server = server.clone();
        let first_started = started.clone();
        let first = tokio::spawn(async move {
            first_server
                .execute_query::<String, _>(None, move |_, operation| {
                    first_started.send(1).map_err(|_| QueryError::Cancelled)?;
                    loop {
                        match operation.check() {
                            Ok(()) => std::thread::park_timeout(Duration::from_millis(1)),
                            Err(error) => return Err(error.into()),
                        }
                    }
                })
                .await
        });
        let second_server = server.clone();
        let second_started = started.clone();
        let second = tokio::spawn(async move {
            second_server
                .execute_query::<String, _>(None, move |_, operation| {
                    second_started.send(2).map_err(|_| QueryError::Cancelled)?;
                    loop {
                        match operation.check() {
                            Ok(()) => std::thread::park_timeout(Duration::from_millis(1)),
                            Err(error) => return Err(error.into()),
                        }
                    }
                })
                .await
        });

        tokio::task::yield_now().await;
        let mut running = [observed.recv()?, observed.recv()?];
        running.sort_unstable();
        assert_eq!(running, [1, 2]);

        let third_server = server.clone();
        let third = tokio::spawn(async move {
            third_server
                .execute_query::<String, _>(None, move |_, operation| {
                    operation.check()?;
                    Ok(QueryEnvelope::new(
                        workspace_id,
                        chakra_domain::revision::Revision(1),
                        chakra_domain::state::Freshness::Fresh,
                        chakra_domain::state::WorkspaceStatus::Ready,
                        chakra_domain::state::ProviderState::NotConfigured,
                        Vec::new(),
                        "started after cancellation".to_owned(),
                    ))
                })
                .await
        });

        first.abort();
        second.abort();
        assert!(first.await.is_err_and(|error| error.is_cancelled()));
        assert!(second.await.is_err_and(|error| error.is_cancelled()));
        let joined = tokio::time::timeout(Duration::from_secs(1), third)
            .await
            .map_err(|_| "replacement query did not acquire a released permit")?;
        let response = joined.map_err(|error| format!("replacement query task failed: {error}"))?;
        let third = response?;
        assert_eq!(third.value["data"], "started after cancellation");

        // Acquiring the whole executor proves both cancelled blocking workers
        // have observed cancellation and released their permits. The aborted
        // Tokio wrappers finish before `spawn_blocking` workers, so inspecting
        // the gauges immediately after joining the wrappers is racy.
        let released = tokio::time::timeout(
            Duration::from_secs(1),
            server
                .query_slots
                .clone()
                .acquire_many_owned(MAX_CONCURRENT_QUERIES as u32),
        )
        .await
        .map_err(|_| "cancelled query workers did not release both permits")??;
        drop(released);

        let metrics = server.query_metrics.snapshot();
        assert_eq!(metrics.queued, 0);
        assert_eq!(metrics.running, 0);
        assert_eq!(metrics.cancelled, 2);
        assert_eq!(metrics.started, 3);
        assert_eq!(metrics.completed, 1);
        assert_eq!(metrics.execution_timed_out, 0);
        assert!(metrics.permit_hold_micros_total >= metrics.permit_hold_micros_max);
        Ok(())
    }

    #[tokio::test]
    async fn queue_and_execution_deadlines_have_distinct_errors_and_metrics()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let identity = WorkspaceIdentity::for_primary_worktree(std::path::Path::new("."))?;
        let service: Arc<dyn QueryService> = Arc::new(WorkspaceEngine::new(identity));
        let queued_server = ChakraMcpServer::with_timeouts(
            service.clone(),
            Duration::from_millis(10),
            Duration::from_secs(1),
        );
        let first = queued_server.query_slots.clone().acquire_owned().await?;
        let second = queued_server.query_slots.clone().acquire_owned().await?;
        let called = Arc::new(AtomicBool::new(false));
        let observed = called.clone();
        let queue_error = match queued_server
            .execute_query::<String, _>(None, move |_, _| {
                observed.store(true, Ordering::Release);
                Err(QueryError::Unsupported("queue deadline test"))
            })
            .await
        {
            Ok(_) => return Err("queued query unexpectedly started".into()),
            Err(error) => error,
        };
        assert_eq!(
            queue_error.data,
            Some(serde_json::json!({ "kind": "queue_timeout" }))
        );
        assert!(!called.load(Ordering::Acquire));
        assert_eq!(queued_server.query_metrics.snapshot().queue_timed_out, 1);
        drop((first, second));

        let deadline_server = ChakraMcpServer::with_timeouts(
            service,
            Duration::from_secs(1),
            Duration::from_millis(10),
        );
        let execution_error = match deadline_server
            .execute_query::<String, _>(None, move |_, operation| {
                loop {
                    match operation.check() {
                        Ok(()) => std::thread::park_timeout(Duration::from_millis(1)),
                        Err(error) => return Err(error.into()),
                    }
                }
            })
            .await
        {
            Ok(_) => return Err("deadline-bound query unexpectedly succeeded".into()),
            Err(error) => error,
        };
        assert_eq!(
            execution_error.data,
            Some(serde_json::json!({ "kind": "execution_deadline" }))
        );
        let metrics = deadline_server.query_metrics.snapshot();
        assert_eq!(metrics.execution_timed_out, 1);
        assert_eq!(metrics.cancelled, 0);
        assert_eq!(metrics.running, 0);
        Ok(())
    }
}
