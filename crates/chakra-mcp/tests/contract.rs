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
use chakra_language::index_repository;
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
                    call_sites: 0,
                    ambiguous_call_sites: 0,
                    unresolved_call_sites: 0,
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
    let instructions = server_info
        .instructions
        .as_deref()
        .ok_or("server instructions missing")?;
    assert!(instructions.contains("Rust and PHP code intelligence"));

    let tools = client.list_all_tools().await?;
    let mut tool_names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    tool_names.sort_unstable();
    assert_eq!(
        tool_names,
        [
            "callers",
            "context",
            "diff_context",
            "repo_map",
            "search",
            "status",
            "symbol_search"
        ]
    );
    let status_tool = tools
        .iter()
        .find(|tool| tool.name == "status")
        .ok_or("status tool not listed")?;
    assert!(status_tool.description.is_some());
    for name in [
        "repo_map",
        "symbol_search",
        "context",
        "callers",
        "diff_context",
    ] {
        let description = tools
            .iter()
            .find(|tool| tool.name == name)
            .and_then(|tool| tool.description.as_deref())
            .ok_or("tool description missing")?;
        assert!(
            description.contains("PHP"),
            "{name} description: {description}"
        );
    }
    assert!(tools.iter().all(|tool| {
        tool.annotations.as_ref().is_some_and(|annotations| {
            annotations.read_only_hint == Some(true)
                && annotations.destructive_hint == Some(false)
                && annotations.idempotent_hint == Some(true)
                && annotations.open_world_hint == Some(false)
        })
    }));

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

fn git(root: &Path, args: &[&str]) -> Result<(), Box<dyn Error + Send + Sync>> {
    let status = Command::new("git").current_dir(root).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("git {} failed", args.join(" ")).into())
    }
}

fn indexed_fixture_engine() -> Result<(TempDir, WorkspaceEngine), Box<dyn Error + Send + Sync>> {
    let repository = TempDir::new()?;
    git(repository.path(), &["init", "--quiet"])?;
    git(
        repository.path(),
        &["config", "user.email", "tests@example.invalid"],
    )?;
    git(repository.path(), &["config", "user.name", "Chakra Tests"])?;
    copy_rust_tree(&source_fixture_root(), repository.path())?;
    git(repository.path(), &["add", "src", "tests"])?;
    git(repository.path(), &["commit", "--quiet", "-m", "base"])?;
    let service_path = repository.path().join("src/service/payment_service.rs");
    let service_source = fs::read_to_string(&service_path)?;
    if !service_source.contains("amount_cents == 0") {
        return Err("fixture refund guard missing".into());
    }
    fs::write(
        &service_path,
        service_source.replacen("amount_cents == 0", "amount_cents <= 0", 1),
    )?;
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    engine.install_diff_provider(Arc::new(chakra_git::GitWorkspaceDiff))?;
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

    let status = client
        .call_tool(CallToolRequestParams::new("status"))
        .await?
        .structured_content
        .ok_or("indexed status must return structured content")?;
    assert_eq!(status["data"]["providers"][0]["name"], "rust-analyzer");
    assert_eq!(status["data"]["providers"][0]["languages"][0], "rust");
    assert!(
        status["data"]["providers"][0]["capabilities"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item == "incoming_calls"))
    );

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
        candidate["language"] == "rust"
            && candidate["precision"] == "syntax"
            && candidate["provenance"] == "tree_sitter"
    }));
    let service_refund = candidates
        .iter()
        .find(|candidate| {
            candidate["qualified_name"] == "service::payment_service::PaymentService::refund"
        })
        .ok_or("service refund candidate missing")?;
    let callers_args = serde_json::from_value(serde_json::json!({
        "symbol": {
            "by_id": {
                "id": service_refund["id"],
                "revision": symbols["revision"]
            }
        },
        "limit": 20
    }))?;
    let callers = client
        .call_tool(CallToolRequestParams::new("callers").with_arguments(callers_args))
        .await?;
    assert_eq!(callers.is_error, Some(false));
    let callers = callers
        .structured_content
        .ok_or("callers must return structured content")?;
    assert_eq!(callers["provider_state"], "not_configured");
    assert_ne!(callers["data"]["callers"][0]["precision"], "precise");
    assert_ne!(callers["data"]["callers"][0]["provenance"], "rust_analyzer");

    let context_args = serde_json::from_value(serde_json::json!({
        "symbol": {
            "by_id": {
                "id": service_refund["id"],
                "revision": symbols["revision"]
            }
        },
        "limit": 20
    }))?;
    let context = client
        .call_tool(CallToolRequestParams::new("context").with_arguments(context_args))
        .await?;
    assert_eq!(context.is_error, Some(false));
    let context = context
        .structured_content
        .ok_or("context must return structured content")?;
    assert_eq!(context["data"]["symbol"]["id"], service_refund["id"]);
    assert!(
        context["data"]["syntax_call_candidates"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );

    let ambiguous_args = serde_json::from_value(serde_json::json!({
        "symbol": { "by_name": "refund" },
        "limit": 20
    }))?;
    let ambiguous = client
        .call_tool(CallToolRequestParams::new("context").with_arguments(ambiguous_args))
        .await;
    let ambiguity = match ambiguous {
        Ok(_) => return Err("ambiguous context unexpectedly succeeded".into()),
        Err(error) => error,
    };
    assert!(ambiguity.to_string().contains("ambiguous symbol reference"));

    let diff_args = serde_json::from_value(serde_json::json!({ "limit": 20 }))?;
    let diff = client
        .call_tool(CallToolRequestParams::new("diff_context").with_arguments(diff_args))
        .await?;
    assert_eq!(diff.is_error, Some(false));
    let diff = diff
        .structured_content
        .ok_or("diff_context must return structured content")?;
    assert_eq!(diff["freshness"], "fresh");
    assert_eq!(
        diff["data"]["changed_files"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(
        diff["data"]["changed_files"][0]["path"],
        "src/service/payment_service.rs"
    );
    assert_eq!(diff["data"]["changed_files"][0]["change"], "modified");
    assert_eq!(diff["data"]["changed_files"][0]["provenance"], "git");
    assert_eq!(diff["data"]["changed_files"][0]["precision"], "precise");
    let changed_symbols = diff["data"]["changed_symbols"]
        .as_array()
        .ok_or("changed symbols missing")?;
    let changed_refund = changed_symbols
        .iter()
        .find(|symbol| {
            symbol["symbol"]["qualified_name"] == "service::payment_service::PaymentService::refund"
                && symbol["basis"] == "declared_in_changed_file"
                && symbol["precision"] == "heuristic"
        })
        .ok_or("changed refund symbol missing")?;
    let changed_refund_id = &changed_refund["symbol"]["id"];
    assert!(
        diff["data"]["related_callers"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| {
                item["changed_symbol_id"] != *changed_refund_id
                    || item["relation"]["symbol"]["qualified_name"]
                        != "api::controller::PaymentController::refund"
            }))
    );
    assert!(
        diff["data"]["related_tests"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| {
                item["changed_symbol_id"] != *changed_refund_id
                    || item["relation"]["edge_kind"] != "TESTS"
            }))
    );

    let bounded_args = serde_json::from_value(serde_json::json!({ "limit": 1 }))?;
    let bounded = client
        .call_tool(CallToolRequestParams::new("diff_context").with_arguments(bounded_args))
        .await?
        .structured_content
        .ok_or("bounded diff_context must return structured content")?;
    assert_eq!(bounded["truncated"], true);
    for section in [
        "changed_files",
        "changed_symbols",
        "related_callers",
        "related_tests",
        "related_call_candidates",
    ] {
        assert!(
            bounded["data"][section]
                .as_array()
                .is_some_and(|items| items.len() <= 1)
        );
    }

    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn php_symbols_context_and_diff_are_queryable_through_mcp()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let repository = TempDir::new()?;
    git(repository.path(), &["init", "--quiet"])?;
    git(
        repository.path(),
        &["config", "user.email", "tests@example.invalid"],
    )?;
    git(repository.path(), &["config", "user.name", "Chakra Tests"])?;
    fs::create_dir_all(repository.path().join("src"))?;
    let service_path = repository.path().join("src/PaymentService.php");
    fs::write(
        &service_path,
        r#"<?php
namespace App\Service;
final class PaymentService {
    public function refund(int $amount): void {}
}
"#,
    )?;
    fs::write(
        repository.path().join("src/PaymentController.php"),
        r#"<?php
namespace App\Api;
final class PaymentController {
    public function refund(): void { $this->service->refund(100); }
}
"#,
    )?;
    git(repository.path(), &["add", "src"])?;
    git(repository.path(), &["commit", "--quiet", "-m", "base"])?;
    let source = fs::read_to_string(&service_path)?;
    fs::write(
        &service_path,
        source.replace(
            "public function refund",
            "// current worktree edit\n    public function refund",
        ),
    )?;

    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    engine.install_diff_provider(Arc::new(chakra_git::GitWorkspaceDiff))?;

    let server = ChakraMcpServer::new(Arc::new(engine));
    let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await?;

    let symbols = client
        .call_tool(CallToolRequestParams::new("symbol_search").with_arguments(
            serde_json::from_value(serde_json::json!({ "query": "refund", "limit": 20 }))?,
        ))
        .await?
        .structured_content
        .ok_or("PHP symbol_search must return structured content")?;
    let candidates = symbols["data"]["candidates"]
        .as_array()
        .ok_or("PHP candidates missing")?;
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate["language"] == "php")
    );
    let service_refund = candidates
        .iter()
        .find(|candidate| candidate["qualified_name"] == "App::Service::PaymentService::refund")
        .ok_or("PHP service refund missing")?;
    let controller_refund = candidates
        .iter()
        .find(|candidate| candidate["qualified_name"] == "App::Api::PaymentController::refund")
        .ok_or("PHP controller refund missing")?;

    let context =
        client
            .call_tool(CallToolRequestParams::new("context").with_arguments(
                serde_json::from_value(serde_json::json!({
                    "symbol": { "by_id": {
                        "id": service_refund["id"],
                        "revision": symbols["revision"]
                    }},
                    "limit": 20
                }))?,
            ))
            .await?
            .structured_content
            .ok_or("PHP context must return structured content")?;
    assert_eq!(context["data"]["symbol"]["language"], "php");
    assert!(
        context["data"]["callers"]
            .as_array()
            .is_some_and(|items| items.is_empty())
    );
    let controller_context =
        client
            .call_tool(CallToolRequestParams::new("context").with_arguments(
                serde_json::from_value(serde_json::json!({
                    "symbol": { "by_id": {
                        "id": controller_refund["id"],
                        "revision": symbols["revision"]
                    }},
                    "limit": 20
                }))?,
            ))
            .await?
            .structured_content
            .ok_or("PHP controller context must return structured content")?;
    assert!(
        controller_context["data"]["syntax_call_candidates"]
            .as_array()
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item["caller"]["qualified_name"] == "App::Api::PaymentController::refund"
                        && item["name"] == "refund"
                        && item["candidate_target"].is_null()
                        && item["resolution"] == "unresolved"
                        && item["precision"] == "syntax"
                })
            })
    );

    let diff = client
        .call_tool(
            CallToolRequestParams::new("diff_context")
                .with_arguments(serde_json::from_value(serde_json::json!({ "limit": 20 }))?),
        )
        .await?
        .structured_content
        .ok_or("PHP diff_context must return structured content")?;
    assert_eq!(
        diff["data"]["changed_files"][0]["path"],
        "src/PaymentService.php"
    );
    assert!(
        diff["data"]["changed_symbols"]
            .as_array()
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item["symbol"]["qualified_name"] == "App::Service::PaymentService::refund"
                        && item["symbol"]["language"] == "php"
                })
            })
    );

    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;
    Ok(())
}
