//! MCP server: typed tools over stdio (ADR-0003).

use std::io::{self, Write};
use std::sync::Arc;

use chakra_domain::envelope::QueryEnvelope;
use chakra_domain::query::{
    CallersData, CallersRequest, ContextData, ContextRequest, DiffContextData, DiffContextRequest,
    QueryError, QueryService, RepoMapData, RepoMapRequest, SearchData, SearchRequest, StatusData,
    StatusRequest, SymbolSearchData, SymbolSearchRequest,
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

/// MCP server handle. Cloneable so transports can share one query service.
#[derive(Clone)]
pub struct ChakraMcpServer {
    service: Arc<dyn QueryService>,
    query_slots: Arc<Semaphore>,
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
    }
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
        Err(_) if writer.exceeded => Err(ErrorData::internal_error(
            format!(
                "query response exceeds the {MAX_MCP_RESPONSE_BYTES}-byte MCP budget; lower the requested limit"
            ),
            None,
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
            tool_router: Self::tool_router(),
        }
    }

    async fn execute_query<T, F>(&self, query: F) -> Result<Json<QueryEnvelope<T>>, ErrorData>
    where
        T: Send + Serialize + 'static,
        F: FnOnce(&dyn QueryService) -> Result<QueryEnvelope<T>, QueryError> + Send + 'static,
    {
        let permit = self
            .query_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ErrorData::internal_error("query executor is shutting down", None))?;
        let service = self.service.clone();
        let envelope = tokio::task::spawn_blocking(move || -> Result<_, ErrorData> {
            let _permit = permit;
            let envelope = query(service.as_ref()).map_err(to_error_data)?;
            enforce_response_budget(&envelope)?;
            Ok(envelope)
        })
        .await
        .map_err(|error| {
            ErrorData::internal_error(format!("query worker failed: {error}"), None)
        })??;
        Ok(Json(envelope))
    }

    #[tool(
        name = "status",
        description = "Chakra workspace status: identity, published revision, index counts, provider state"
    )]
    async fn status(&self) -> Result<Json<QueryEnvelope<StatusData>>, ErrorData> {
        let envelope = self.service.status(StatusRequest).map_err(to_error_data)?;
        enforce_response_budget(&envelope)?;
        Ok(Json(envelope))
    }

    #[tool(
        name = "repo_map",
        description = "List indexed Rust files with bounded syntax-symbol counts"
    )]
    async fn repo_map(
        &self,
        Parameters(request): Parameters<RepoMapRequest>,
    ) -> Result<Json<QueryEnvelope<RepoMapData>>, ErrorData> {
        self.execute_query(move |service| service.repo_map(request))
            .await
    }

    #[tool(
        name = "search",
        description = "Search the atomically indexed source snapshot using literal or regex text matching"
    )]
    async fn search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<Json<QueryEnvelope<SearchData>>, ErrorData> {
        self.execute_query(move |service| service.search(request))
            .await
    }

    #[tool(
        name = "symbol_search",
        description = "Find bounded Rust syntax symbol candidates by simple or qualified name"
    )]
    async fn symbol_search(
        &self,
        Parameters(request): Parameters<SymbolSearchRequest>,
    ) -> Result<Json<QueryEnvelope<SymbolSearchData>>, ErrorData> {
        self.execute_query(move |service| service.symbol_search(request))
            .await
    }

    #[tool(
        name = "context",
        description = "Get bounded syntax context for one resolved symbol, with optional current rust-analyzer callers and callees"
    )]
    async fn context(
        &self,
        Parameters(request): Parameters<ContextRequest>,
    ) -> Result<Json<QueryEnvelope<ContextData>>, ErrorData> {
        self.execute_query(move |service| service.context(request))
            .await
    }

    #[tool(
        name = "callers",
        description = "Get bounded callers for one resolved symbol, preferring current rust-analyzer precision and retaining honest syntax fallback"
    )]
    async fn callers(
        &self,
        Parameters(request): Parameters<CallersRequest>,
    ) -> Result<Json<QueryEnvelope<CallersData>>, ErrorData> {
        self.execute_query(move |service| service.callers(request))
            .await
    }

    #[tool(
        name = "diff_context",
        description = "Summarize bounded Rust changes from HEAD to the materialized worktree, with changed symbols and related callers/tests"
    )]
    async fn diff_context(
        &self,
        Parameters(request): Parameters<DiffContextRequest>,
    ) -> Result<Json<QueryEnvelope<DiffContextData>>, ErrorData> {
        self.execute_query(move |service| service.diff_context(request))
            .await
    }
}

#[tool_handler(
    name = "chakra",
    instructions = "Chakra Rust code intelligence: inspect status and repo_map, search indexed source, resolve ambiguous names through symbol_search, request context or callers for one entity, and use diff_context for current worktree changes. Results are bounded and carry revision, freshness, provider state, provenance, and precision.",
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
                .execute_query::<StatusData, _>(move |_| {
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
        Ok(())
    }

    #[tokio::test]
    async fn oversized_serialized_response_is_rejected()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let identity = WorkspaceIdentity::for_primary_worktree(std::path::Path::new("."))?;
        let workspace_id = identity.workspace.clone();
        let server = ChakraMcpServer::new(Arc::new(WorkspaceEngine::new(identity)));
        let result = server
            .execute_query::<String, _>(move |_| {
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
}
