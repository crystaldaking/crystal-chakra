//! MCP server skeleton: typed tools over stdio (ADR-0003).
//!
//! Only `status` is exposed in this phase — enough to prove typed tool
//! exposure end to end. The remaining v0.1 tools are added with the indexer
//! so their results carry real data instead of stub payloads.

use std::sync::Arc;

use chakra_domain::envelope::QueryEnvelope;
use chakra_domain::query::{QueryError, QueryService, StatusData, StatusRequest};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Json;
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use thiserror::Error;

/// MCP server handle. Cloneable so transports can share one query service.
#[derive(Clone)]
pub struct ChakraMcpServer {
    service: Arc<dyn QueryService>,
    tool_router: ToolRouter<Self>,
}

/// Maps domain errors onto MCP protocol errors. Deliberately minimal in
/// this phase; richer mapping (tool-level vs protocol errors) arrives with
/// the real tools.
fn to_error_data(error: QueryError) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}

#[tool_router]
impl ChakraMcpServer {
    pub fn new(service: Arc<dyn QueryService>) -> Self {
        Self {
            service,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "status",
        description = "Chakra workspace status: identity, published revision, index counts, provider state"
    )]
    async fn status(&self) -> Result<Json<QueryEnvelope<StatusData>>, ErrorData> {
        self.service
            .status(StatusRequest)
            .map(Json)
            .map_err(to_error_data)
    }
}

#[tool_handler(
    name = "chakra",
    instructions = "Chakra code intelligence (v0.1 skeleton): only the `status` tool is exposed so far; symbol search, callers, and diff context tools arrive as indexing lands.",
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
