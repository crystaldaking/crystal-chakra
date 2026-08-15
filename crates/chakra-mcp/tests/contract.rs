//! MCP contract tests: the server must expose typed tools and answer a
//! real client over an in-process transport, without MCP types leaking
//! into the domain (the stub below implements the domain contract only).

use std::error::Error;
use std::sync::Arc;

use chakra_domain::envelope::QueryEnvelope;
use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::query::{
    CallersRequest, ContextRequest, DiffContextRequest, IndexCounts, QueryError, QueryService,
    RepoMapRequest, SearchRequest, StatusData, StatusRequest, SymbolSearchRequest,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::{Freshness, ProviderState, WorkspaceStatus};
use chakra_mcp::ChakraMcpServer;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;

/// Fixed-response domain service: the adapter must not care whether the
/// engine or a stub answers.
struct StubService;

impl StubService {
    fn identity() -> Result<WorkspaceIdentity, Box<dyn Error>> {
        Ok(WorkspaceIdentity::for_primary_worktree(
            std::path::Path::new("."),
        )?)
    }
}

impl QueryService for StubService {
    fn status(&self, _request: StatusRequest) -> Result<QueryEnvelope<StatusData>, QueryError> {
        let identity = Self::identity().map_err(|_| QueryError::Invalid("identity".to_owned()))?;
        Ok(QueryEnvelope::new(
            identity.workspace.clone(),
            Revision(7),
            Freshness::Fresh,
            WorkspaceStatus::Ready,
            ProviderState::NotConfigured,
            false,
            StatusData {
                workspace: identity,
                counts: IndexCounts {
                    files: 1,
                    symbols: 2,
                    edges: 3,
                },
                providers: vec![],
            },
        ))
    }

    fn repo_map(
        &self,
        _request: RepoMapRequest,
    ) -> Result<QueryEnvelope<chakra_domain::query::RepoMapData>, QueryError> {
        Err(QueryError::Unsupported("repo_map"))
    }

    fn search(
        &self,
        _request: SearchRequest,
    ) -> Result<QueryEnvelope<chakra_domain::query::SearchData>, QueryError> {
        Err(QueryError::Unsupported("search"))
    }

    fn symbol_search(
        &self,
        _request: SymbolSearchRequest,
    ) -> Result<QueryEnvelope<chakra_domain::query::SymbolSearchData>, QueryError> {
        Err(QueryError::Unsupported("symbol_search"))
    }

    fn context(
        &self,
        _request: ContextRequest,
    ) -> Result<QueryEnvelope<chakra_domain::query::ContextData>, QueryError> {
        Err(QueryError::Unsupported("context"))
    }

    fn callers(
        &self,
        _request: CallersRequest,
    ) -> Result<QueryEnvelope<chakra_domain::query::CallersData>, QueryError> {
        Err(QueryError::Unsupported("callers"))
    }

    fn diff_context(
        &self,
        _request: DiffContextRequest,
    ) -> Result<QueryEnvelope<chakra_domain::query::DiffContextData>, QueryError> {
        Err(QueryError::Unsupported("diff_context"))
    }
}

#[tokio::test]
async fn status_tool_is_listed_and_callable() -> Result<(), Box<dyn Error + Send + Sync>> {
    let server = ChakraMcpServer::new(Arc::new(StubService));

    let (server_transport, client_transport) = tokio::io::duplex(8192);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });

    let client = ().serve(client_transport).await?;

    let server_info = client
        .peer_info()
        .ok_or("server info missing after initialize")?;
    let implementation = server_info
        .server_info
        .as_ref()
        .ok_or("server implementation missing")?;
    assert_eq!(implementation.name, "chakra");

    let tools = client.list_all_tools().await?;
    let status_tool = tools
        .iter()
        .find(|tool| tool.name == "status")
        .ok_or("status tool not listed")?;
    assert!(status_tool.description.is_some());

    let result = client
        .call_tool(CallToolRequestParams::new("status"))
        .await?;
    assert_eq!(result.is_error, Some(false));
    let structured = result
        .structured_content
        .ok_or("status must return structured content")?;
    assert_eq!(structured["schema_version"], 1);
    assert_eq!(structured["revision"], 7);
    assert_eq!(structured["freshness"], "fresh");
    assert_eq!(structured["status"], "ready");
    assert_eq!(structured["provider_state"], "not_configured");
    assert_eq!(structured["data"]["counts"]["symbols"], 2);

    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;
    Ok(())
}
