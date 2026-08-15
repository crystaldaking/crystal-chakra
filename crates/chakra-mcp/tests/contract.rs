//! MCP contract tests: the server must expose typed tools and answer a
//! real client over an in-process transport, without MCP types leaking
//! into the domain (the stub below implements the domain contract only).

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use chakra_domain::envelope::QueryEnvelope;
use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::query::{
    CallersRequest, ContextRequest, DiffContextRequest, IndexCounts, QueryError, QueryService,
    RepoMapRequest, SearchRequest, StatusData, StatusRequest, SymbolSearchRequest,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::{Freshness, ProviderState, WorkspaceStatus};
use chakra_engine::WorkspaceEngine;
use chakra_language_rust::index_repository;
use chakra_mcp::ChakraMcpServer;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use tempfile::TempDir;

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
    let mut tool_names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    tool_names.sort_unstable();
    assert_eq!(
        tool_names,
        ["repo_map", "search", "status", "symbol_search"]
    );
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

fn source_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("rust")
        .join("controller-service-provider")
}

fn copy_rust_tree(source: &Path, target: &Path) -> Result<(), Box<dyn Error + Send + Sync>> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            if entry.file_name() == "target" {
                continue;
            }
            fs::create_dir_all(&destination)?;
            copy_rust_tree(&entry.path(), &destination)?;
        } else if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "rs")
        {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

fn indexed_fixture_engine() -> Result<(TempDir, WorkspaceEngine), Box<dyn Error + Send + Sync>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    copy_rust_tree(&source_fixture_root(), repository.path())?;
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    Ok((repository, engine))
}

#[tokio::test]
async fn indexed_fixture_is_queryable_through_structured_mcp_tools()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let (_repository, engine) = indexed_fixture_engine()?;
    let server = ChakraMcpServer::new(Arc::new(engine));
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await?;

    let repo_map_args = serde_json::from_value(serde_json::json!({ "limit": 20 }))?;
    let repo_map = client
        .call_tool(CallToolRequestParams::new("repo_map").with_arguments(repo_map_args))
        .await?;
    assert_eq!(repo_map.is_error, Some(false));
    let repo_map = repo_map
        .structured_content
        .ok_or("repo_map must return structured content")?;
    assert_eq!(repo_map["freshness"], "fresh");
    assert_eq!(repo_map["data"]["files"].as_array().map(Vec::len), Some(7));
    assert_eq!(repo_map["data"]["files"][0]["provenance"], "git");
    assert_eq!(repo_map["data"]["files"][0]["precision"], "precise");

    let search_args = serde_json::from_value(serde_json::json!({
        "query": "amount must be positive",
        "case_sensitive": true,
        "limit": 5
    }))?;
    let search = client
        .call_tool(CallToolRequestParams::new("search").with_arguments(search_args))
        .await?;
    assert_eq!(search.is_error, Some(false));
    let search = search
        .structured_content
        .ok_or("search must return structured content")?;
    assert_eq!(search["data"]["matches"][0]["precision"], "textual");
    assert_eq!(search["data"]["matches"][0]["provenance"], "text_search");

    let symbol_args = serde_json::from_value(serde_json::json!({
        "query": "refund",
        "limit": 20
    }))?;
    let symbols = client
        .call_tool(CallToolRequestParams::new("symbol_search").with_arguments(symbol_args))
        .await?;
    assert_eq!(symbols.is_error, Some(false));
    let symbols = symbols
        .structured_content
        .ok_or("symbol_search must return structured content")?;
    let candidates = symbols["data"]["candidates"]
        .as_array()
        .ok_or("symbol candidates missing")?;
    assert!(candidates.len() >= 7);
    assert!(candidates.iter().all(|candidate| {
        candidate["precision"] == "syntax" && candidate["provenance"] == "tree_sitter"
    }));

    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;
    Ok(())
}
