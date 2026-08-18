//! MCP server: typed tools over stdio (ADR-0003).

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chakra_domain::envelope::QueryEnvelope;
use chakra_domain::operation::OperationContext;
use chakra_domain::query::{
    CallersData, CallersRequest, ContextData, ContextRequest, DiffContextData, DiffContextRequest,
    QueryError, QueryExecutionMetrics, QueryService, RepoMapData, RepoMapRequest, SearchData,
    SearchRequest, StatusData, StatusRequest, SymbolSearchData, SymbolSearchRequest,
};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::Semaphore;

const MAX_CONCURRENT_QUERIES: usize = 2;
const MAX_MCP_RESPONSE_BYTES: usize = 1024 * 1024;
const QUERY_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_MESSAGE_CHARS: usize = 1_024;

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
    service: Arc<dyn QueryService>,
    query_slots: Arc<Semaphore>,
    query_metrics: Arc<QueryMetricsState>,
    queue_timeout: Duration,
    execution_timeout: Duration,
    tool_router: ToolRouter<Self>,
}

fn to_error_data(error: QueryError) -> ErrorData {
    match error {
        QueryError::Invalid(_)
        | QueryError::MissingSymbolRef
        | QueryError::StaleSymbolRef { .. }
        | QueryError::SymbolNotFound(_)
        | QueryError::AmbiguousSymbol { .. }
        | QueryError::FreshnessNotMet { .. } => ErrorData::invalid_params(error.to_string(), None),
        QueryError::Unsupported(_)
        | QueryError::FreshnessUnavailable(_)
        | QueryError::DiffUnavailable(_) => ErrorData::internal_error(error.to_string(), None),
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

struct ResponseBudgetWriter {
    remaining: usize,
    exceeded: bool,
}

impl Write for ResponseBudgetWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            self.exceeded = true;
            return Err(io::Error::other("MCP response budget exceeded"));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn enforce_response_budget<T: Serialize>(value: &T) -> Result<(), ErrorData> {
    let mut writer = ResponseBudgetWriter {
        remaining: MAX_MCP_RESPONSE_BYTES,
        exceeded: false,
    };
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(()),
        Err(_) if writer.exceeded => Err(execution_error(
            "resource_budget",
            format!(
                "query response exceeds the {MAX_MCP_RESPONSE_BYTES}-byte MCP budget; lower the requested limit"
            ),
        )),
        Err(error) => Err(ErrorData::internal_error(
            format!("failed to size query response: {error}"),
            None,
        )),
    }
}

#[tool_router]
impl ChakraMcpServer {
    pub fn new(service: Arc<dyn QueryService>) -> Self {
        Self {
            service,
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
            service,
            query_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_QUERIES)),
            query_metrics: Arc::new(QueryMetricsState::default()),
            queue_timeout,
            execution_timeout,
            tool_router: Self::tool_router(),
        }
    }

    async fn execute_query<T, F>(&self, query: F) -> Result<Json<QueryEnvelope<T>>, ErrorData>
    where
        T: Send + Serialize + 'static,
        F: FnOnce(&dyn QueryService, &OperationContext) -> Result<QueryEnvelope<T>, QueryError>
            + Send
            + 'static,
    {
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
        let service = self.service.clone();
        let metrics = self.query_metrics.clone();
        let blocking_operation = operation.clone();
        let hold = PermitHoldGuard::new(metrics, Instant::now());
        let joined = tokio::task::spawn_blocking(move || -> Result<_, ErrorData> {
            let _permit = permit;
            let mut hold = hold;
            let result = query(service.as_ref(), &blocking_operation);
            hold.finish(&result);
            let envelope = result.map_err(to_error_data)?;
            enforce_response_budget(&envelope)?;
            Ok(envelope)
        })
        .await
        .map_err(|error| ErrorData::internal_error(format!("query worker failed: {error}"), None));
        cancellation.disarm();
        let envelope = joined??;
        Ok(Json(envelope))
    }

    #[tool(
        name = "status",
        description = "Chakra workspace status: identity, published revision, index counts, provider state",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn status(&self) -> Result<Json<QueryEnvelope<StatusData>>, ErrorData> {
        let mut envelope = self.service.status(StatusRequest).map_err(to_error_data)?;
        envelope.data.query_execution = Some(self.query_metrics.snapshot());
        enforce_response_budget(&envelope)?;
        Ok(Json(envelope))
    }

    #[tool(
        name = "repo_map",
        description = "List indexed Rust and PHP files with bounded syntax-symbol counts",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn repo_map(
        &self,
        Parameters(request): Parameters<RepoMapRequest>,
    ) -> Result<Json<QueryEnvelope<RepoMapData>>, ErrorData> {
        self.execute_query(move |service, operation| {
            service.repo_map_with_context(request, operation)
        })
        .await
    }

    #[tool(
        name = "search",
        description = "Search the atomically indexed source snapshot using literal or regex text matching",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<Json<QueryEnvelope<SearchData>>, ErrorData> {
        self.execute_query(move |service, operation| {
            service.search_with_context(request, operation)
        })
        .await
    }

    #[tool(
        name = "symbol_search",
        description = "Find bounded Rust and PHP syntax symbol candidates by simple or qualified name",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn symbol_search(
        &self,
        Parameters(request): Parameters<SymbolSearchRequest>,
    ) -> Result<Json<QueryEnvelope<SymbolSearchData>>, ErrorData> {
        self.execute_query(move |service, operation| {
            service.symbol_search_with_context(request, operation)
        })
        .await
    }

    #[tool(
        name = "context",
        description = "Get bounded syntax context for one resolved Rust or PHP symbol, with optional current precise enrichment when supported",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn context(
        &self,
        Parameters(request): Parameters<ContextRequest>,
    ) -> Result<Json<QueryEnvelope<ContextData>>, ErrorData> {
        self.execute_query(move |service, operation| {
            service.context_with_context(request, operation)
        })
        .await
    }

    #[tool(
        name = "callers",
        description = "Get bounded callers for one resolved Rust or PHP symbol, preferring current provider precision when supported and retaining honest syntax fallback",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn callers(
        &self,
        Parameters(request): Parameters<CallersRequest>,
    ) -> Result<Json<QueryEnvelope<CallersData>>, ErrorData> {
        self.execute_query(move |service, operation| {
            service.callers_with_context(request, operation)
        })
        .await
    }

    #[tool(
        name = "diff_context",
        description = "Summarize bounded Rust and PHP changes from HEAD to the materialized worktree, with changed symbols and related callers/tests",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn diff_context(
        &self,
        Parameters(request): Parameters<DiffContextRequest>,
    ) -> Result<Json<QueryEnvelope<DiffContextData>>, ErrorData> {
        self.execute_query(move |service, operation| {
            service.diff_context_with_context(request, operation)
        })
        .await
    }
}

#[tool_handler(
    name = "chakra",
    instructions = "Chakra Rust and PHP code intelligence: inspect status and repo_map, search indexed source, resolve ambiguous names through symbol_search, request context or callers for one entity, and use diff_context for current worktree changes. Results are bounded and carry language, revision, freshness, provider state and capabilities, provenance, and precision.",
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    use chakra_domain::identity::WorkspaceIdentity;
    use chakra_domain::query::StatusData;
    use chakra_engine::WorkspaceEngine;

    use super::*;

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
                .execute_query::<StatusData, _>(move |_, _| {
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
            .execute_query::<String, _>(move |_, _| {
                Ok(QueryEnvelope::new(
                    workspace_id,
                    chakra_domain::revision::Revision(1),
                    chakra_domain::state::Freshness::Fresh,
                    chakra_domain::state::WorkspaceStatus::Ready,
                    chakra_domain::state::ProviderState::NotConfigured,
                    false,
                    "x".repeat(MAX_MCP_RESPONSE_BYTES),
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
                .execute_query::<String, _>(move |_, operation| {
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
                .execute_query::<String, _>(move |_, operation| {
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
                .execute_query::<String, _>(move |_, operation| {
                    operation.check()?;
                    Ok(QueryEnvelope::new(
                        workspace_id,
                        chakra_domain::revision::Revision(1),
                        chakra_domain::state::Freshness::Fresh,
                        chakra_domain::state::WorkspaceStatus::Ready,
                        chakra_domain::state::ProviderState::NotConfigured,
                        false,
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
        assert_eq!(third.0.data, "started after cancellation");

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
            .execute_query::<String, _>(move |_, _| {
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
            .execute_query::<String, _>(move |_, operation| {
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
