//! MCP contract tests: the server must expose typed tools and answer a
//! real client over an in-process transport, without MCP types leaking
//! into the domain (the stub below implements the domain contract only).

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use chakra_domain::envelope::QueryEnvelope;
use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::operation::OperationContext;
use chakra_domain::query::{
    CallersRequest, ContextRequest, DiffContextRequest, IndexCounts, QueryError, QueryService,
    RepoMapRequest, SearchRequest, StatusData, StatusRequest, SymbolSearchRequest,
};
use chakra_domain::revision::Revision;
use chakra_domain::source::SourceMetadataCoverage;
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
            Vec::new(),
            StatusData {
                workspace: identity,
                counts: IndexCounts {
                    files: 1,
                    symbols: 2,
                    edges: 3,
                    call_sites: 0,
                    ambiguous_call_sites: 0,
                    unresolved_call_sites: 0,
                    call_sites_with_truncated_candidates: 0,
                },
                providers: vec![],
                provider_pool: None,
                query_execution: None,
                source_metadata: SourceMetadataCoverage {
                    total_files: 1,
                    cargo_metadata_files: 0,
                    composer_metadata_files: 0,
                    package_json_metadata_files: 0,
                    pyproject_metadata_files: 0,
                    maven_metadata_files: 0,
                    gradle_metadata_files: 0,
                    dotnet_project_metadata_files: 0,
                    shell_project_metadata_files: 0,
                    cpp_project_metadata_files: 0,
                    terraform_module_metadata_files: 0,
                    go_module_metadata_files: 0,
                    path_fallback_files: 1,
                },
                syntax_diagnostics: Default::default(),
            },
        ))
    }

    fn repo_map_with_context(
        &self,
        _request: RepoMapRequest,
        _operation: &OperationContext,
    ) -> Result<QueryEnvelope<chakra_domain::query::RepoMapData>, QueryError> {
        Err(QueryError::Unsupported("repo_map"))
    }

    fn search_with_context(
        &self,
        _request: SearchRequest,
        _operation: &OperationContext,
    ) -> Result<QueryEnvelope<chakra_domain::query::SearchData>, QueryError> {
        Err(QueryError::Unsupported("search"))
    }

    fn symbol_search_with_context(
        &self,
        _request: SymbolSearchRequest,
        _operation: &OperationContext,
    ) -> Result<QueryEnvelope<chakra_domain::query::SymbolSearchData>, QueryError> {
        Err(QueryError::Unsupported("symbol_search"))
    }

    fn context_with_context(
        &self,
        _request: ContextRequest,
        _operation: &OperationContext,
    ) -> Result<QueryEnvelope<chakra_domain::query::ContextData>, QueryError> {
        Err(QueryError::Unsupported("context"))
    }

    fn callers_with_context(
        &self,
        _request: CallersRequest,
        _operation: &OperationContext,
    ) -> Result<QueryEnvelope<chakra_domain::query::CallersData>, QueryError> {
        Err(QueryError::Unsupported("callers"))
    }

    fn diff_context_with_context(
        &self,
        _request: DiffContextRequest,
        _operation: &OperationContext,
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
    assert!(instructions.contains("multi-language code intelligence"));
    assert!(!instructions.contains("Rust and PHP"));

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
    let status_output_schema = status_tool
        .output_schema
        .as_deref()
        .ok_or("status output schema missing")?;
    let status_output_schema = serde_json::to_value(status_output_schema)?;
    assert!(status_output_schema.to_string().contains("schema_version"));
    assert!(status_output_schema.to_string().contains("truncation"));
    assert!(status_output_schema.to_string().contains("providers"));
    assert!(tools.iter().all(|tool| tool.output_schema.is_some()));
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
            description.contains("supported-language"),
            "{name} description: {description}"
        );
        assert!(!description.contains("Rust and PHP"));
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
    assert!(result.content.is_empty());
    let structured = result
        .structured_content
        .ok_or("status must return structured content")?;
    assert_eq!(
        structured["schema_version"],
        chakra_domain::envelope::SCHEMA_VERSION
    );
    assert_eq!(structured["truncation"], serde_json::json!([]));
    assert_eq!(structured["revision"], 7);
    assert_eq!(structured["freshness"], "fresh");
    assert_eq!(structured["status"], "ready");
    assert_eq!(
        structured["data"]["counts"]["call_sites_with_truncated_candidates"],
        0
    );
    assert_eq!(structured["provider_state"], "not_configured");
    assert_eq!(structured["indexing"]["budgets"]["max_files"], 100_000);
    assert!(structured["indexing"]["coverage"].is_object());
    assert!(structured["indexing"]["capabilities"].is_array());
    assert!(structured["indexing"]["degradations"].is_array());
    assert_eq!(structured["data"]["counts"]["symbols"], 2);
    assert_eq!(structured["data"]["query_execution"]["queued"], 0);
    assert_eq!(structured["data"]["query_execution"]["running"], 0);
    assert_eq!(
        structured["data"]["syntax_diagnostics"]["total_diagnostics"],
        0
    );

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

fn laravel_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("php")
        .join("laravel-relationships")
}

fn java_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("java")
        .join("controller-service-provider")
}

fn csharp_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("csharp")
        .join("controller-service-provider")
}

fn shell_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("shell")
        .join("controller-service-provider")
}

fn cpp_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("cpp")
        .join("controller-service-provider")
}

fn hcl_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("hcl")
        .join("controller-service-provider")
}

fn go_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("go")
        .join("controller-service-provider")
}

fn copy_fixture_tree(source: &Path, target: &Path) -> Result<(), Box<dyn Error + Send + Sync>> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&destination)?;
            copy_fixture_tree(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
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
    git(repository.path(), &["tag", "fixture-base"])?;
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

fn indexed_java_fixture_engine() -> Result<(TempDir, WorkspaceEngine), Box<dyn Error + Send + Sync>>
{
    let repository = TempDir::new()?;
    git(repository.path(), &["init", "--quiet"])?;
    git(
        repository.path(),
        &["config", "user.email", "tests@example.invalid"],
    )?;
    git(repository.path(), &["config", "user.name", "Chakra Tests"])?;
    copy_fixture_tree(&java_fixture_root(), repository.path())?;
    git(repository.path(), &["add", "pom.xml", "src"])?;
    git(repository.path(), &["commit", "--quiet", "-m", "base"])?;
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    engine.install_diff_provider(Arc::new(chakra_git::GitWorkspaceDiff))?;
    Ok((repository, engine))
}

fn indexed_csharp_fixture_engine()
-> Result<(TempDir, WorkspaceEngine), Box<dyn Error + Send + Sync>> {
    let repository = TempDir::new()?;
    git(repository.path(), &["init", "--quiet"])?;
    git(
        repository.path(),
        &["config", "user.email", "tests@example.invalid"],
    )?;
    git(repository.path(), &["config", "user.name", "Chakra Tests"])?;
    copy_fixture_tree(&csharp_fixture_root(), repository.path())?;
    git(repository.path(), &["add", "Payments.sln", "src", "tests"])?;
    git(repository.path(), &["commit", "--quiet", "-m", "base"])?;
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    engine.install_diff_provider(Arc::new(chakra_git::GitWorkspaceDiff))?;
    Ok((repository, engine))
}

fn indexed_shell_fixture_engine() -> Result<(TempDir, WorkspaceEngine), Box<dyn Error + Send + Sync>>
{
    let repository = TempDir::new()?;
    git(repository.path(), &["init", "--quiet"])?;
    git(
        repository.path(),
        &["config", "user.email", "tests@example.invalid"],
    )?;
    git(repository.path(), &["config", "user.name", "Chakra Tests"])?;
    copy_fixture_tree(&shell_fixture_root(), repository.path())?;
    git(
        repository.path(),
        &[
            "add",
            ".shellcheckrc",
            "src",
            "tests",
            "vendor",
            "generated",
        ],
    )?;
    git(repository.path(), &["commit", "--quiet", "-m", "base"])?;
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    Ok((repository, engine))
}

fn indexed_cpp_fixture_engine() -> Result<(TempDir, WorkspaceEngine), Box<dyn Error + Send + Sync>>
{
    let repository = TempDir::new()?;
    git(repository.path(), &["init", "--quiet"])?;
    git(
        repository.path(),
        &["config", "user.email", "tests@example.invalid"],
    )?;
    git(repository.path(), &["config", "user.name", "Chakra Tests"])?;
    copy_fixture_tree(&cpp_fixture_root(), repository.path())?;
    git(
        repository.path(),
        &[
            "add",
            "CMakeLists.txt",
            "include",
            "src",
            "tests",
            "vendor",
            "generated",
        ],
    )?;
    git(repository.path(), &["commit", "--quiet", "-m", "base"])?;
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    Ok((repository, engine))
}

fn indexed_hcl_fixture_engine() -> Result<(TempDir, WorkspaceEngine), Box<dyn Error + Send + Sync>>
{
    let repository = TempDir::new()?;
    git(repository.path(), &["init", "--quiet"])?;
    git(
        repository.path(),
        &["config", "user.email", "tests@example.invalid"],
    )?;
    git(repository.path(), &["config", "user.name", "Chakra Tests"])?;
    copy_fixture_tree(&hcl_fixture_root(), repository.path())?;
    git(
        repository.path(),
        &[
            "add",
            "generated",
            "outputs.tf",
            "resources.tf",
            "service.tf",
            "tests",
            "variables.tf",
            "vendor",
            "versions.tf",
        ],
    )?;
    git(repository.path(), &["commit", "--quiet", "-m", "base"])?;
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    Ok((repository, engine))
}

fn indexed_go_fixture_engine() -> Result<(TempDir, WorkspaceEngine), Box<dyn Error + Send + Sync>> {
    let repository = TempDir::new()?;
    git(repository.path(), &["init", "--quiet"])?;
    git(
        repository.path(),
        &["config", "user.email", "tests@example.invalid"],
    )?;
    git(repository.path(), &["config", "user.name", "Chakra Tests"])?;
    copy_fixture_tree(&go_fixture_root(), repository.path())?;
    git(
        repository.path(),
        &[
            "add",
            "go.mod",
            "generated",
            "src",
            "tests",
            "tools_linux.go",
            "vendor",
        ],
    )?;
    git(repository.path(), &["commit", "--quiet", "-m", "base"])?;
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    Ok((repository, engine))
}

#[tokio::test]
async fn java_fixture_is_queryable_through_structured_mcp_tools()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let (_repository, engine) = indexed_java_fixture_engine()?;
    let server = ChakraMcpServer::new(Arc::new(engine));
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await?;

    let repo_map = client
        .call_tool(
            CallToolRequestParams::new("repo_map").with_arguments(serde_json::from_value(
                serde_json::json!({ "include_languages": ["java"], "limit": 20 }),
            )?),
        )
        .await?
        .structured_content
        .ok_or("Java repo_map response missing")?;
    let files = repo_map["data"]["files"]
        .as_array()
        .ok_or("Java repo_map files missing")?;
    assert_eq!(files.len(), 8);
    assert!(files.iter().all(|file| file["language"] == "java"));
    assert!(files.iter().any(|file| {
        file["source_classification"] == "maven_metadata" && file["source_role"] == "test"
    }));

    let symbols = client
        .call_tool(CallToolRequestParams::new("symbol_search").with_arguments(
            serde_json::from_value(
                serde_json::json!({ "query": "sharedUniqueTarget", "limit": 5 }),
            )?,
        ))
        .await?
        .structured_content
        .ok_or("Java symbol_search response missing")?;
    let target = symbols["data"]["candidates"]
        .as_array()
        .and_then(|candidates| candidates.first())
        .ok_or("Java sharedUniqueTarget symbol missing")?;
    assert_eq!(target["language"], "java");
    assert_eq!(target["precision"], "syntax");
    assert_eq!(target["provenance"], "tree_sitter");

    let symbol_ref = serde_json::json!({
        "by_id": { "id": target["id"], "revision": symbols["revision"] }
    });
    let callers = client
        .call_tool(
            CallToolRequestParams::new("callers").with_arguments(serde_json::from_value(
                serde_json::json!({ "symbol": symbol_ref.clone(), "limit": 10 }),
            )?),
        )
        .await?
        .structured_content
        .ok_or("Java callers response missing")?;
    assert!(
        callers["data"]["callers"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert_ne!(callers["data"]["callers"][0]["precision"], "precise");

    let context =
        client
            .call_tool(CallToolRequestParams::new("context").with_arguments(
                serde_json::from_value(serde_json::json!({ "symbol": symbol_ref, "limit": 10 }))?,
            ))
            .await?
            .structured_content
            .ok_or("Java context response missing")?;
    assert_eq!(context["data"]["symbol"]["id"], target["id"]);

    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn csharp_fixture_is_queryable_through_structured_mcp_tools()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let (_repository, engine) = indexed_csharp_fixture_engine()?;
    let server = ChakraMcpServer::new(Arc::new(engine));
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await?;

    let repo_map = client
        .call_tool(
            CallToolRequestParams::new("repo_map").with_arguments(serde_json::from_value(
                serde_json::json!({ "include_languages": ["csharp"], "limit": 20 }),
            )?),
        )
        .await?
        .structured_content
        .ok_or("C# repo_map response missing")?;
    let files = repo_map["data"]["files"]
        .as_array()
        .ok_or("C# repo_map files missing")?;
    assert_eq!(files.len(), 6);
    assert!(files.iter().all(|file| file["language"] == "csharp"));
    assert!(files.iter().any(|file| {
        file["source_classification"] == "dotnet_project_metadata" && file["source_role"] == "test"
    }));

    let symbols = client
        .call_tool(CallToolRequestParams::new("symbol_search").with_arguments(
            serde_json::from_value(
                serde_json::json!({ "query": "SharedUniqueTarget", "limit": 5 }),
            )?,
        ))
        .await?
        .structured_content
        .ok_or("C# symbol_search response missing")?;
    let target = symbols["data"]["candidates"]
        .as_array()
        .and_then(|candidates| candidates.first())
        .ok_or("C# SharedUniqueTarget symbol missing")?;
    assert_eq!(target["language"], "csharp");
    assert_eq!(target["precision"], "syntax");
    assert_eq!(target["provenance"], "tree_sitter");

    let symbol_ref = serde_json::json!({
        "by_id": { "id": target["id"], "revision": symbols["revision"] }
    });
    let callers = client
        .call_tool(
            CallToolRequestParams::new("callers").with_arguments(serde_json::from_value(
                serde_json::json!({ "symbol": symbol_ref.clone(), "limit": 10 }),
            )?),
        )
        .await?
        .structured_content
        .ok_or("C# callers response missing")?;
    assert!(
        callers["data"]["callers"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert_ne!(callers["data"]["callers"][0]["precision"], "precise");

    let context =
        client
            .call_tool(CallToolRequestParams::new("context").with_arguments(
                serde_json::from_value(serde_json::json!({ "symbol": symbol_ref, "limit": 10 }))?,
            ))
            .await?
            .structured_content
            .ok_or("C# context response missing")?;
    assert_eq!(context["data"]["symbol"]["id"], target["id"]);

    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn shell_fixture_is_queryable_through_structured_mcp_tools()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let (_repository, engine) = indexed_shell_fixture_engine()?;
    let server = ChakraMcpServer::new(Arc::new(engine));
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await?;

    let repo_map = client
        .call_tool(
            CallToolRequestParams::new("repo_map").with_arguments(serde_json::from_value(
                serde_json::json!({ "include_languages": ["shell"], "limit": 20 }),
            )?),
        )
        .await?
        .structured_content
        .ok_or("Shell repo_map response missing")?;
    let files = repo_map["data"]["files"]
        .as_array()
        .ok_or("Shell repo_map files missing")?;
    assert_eq!(files.len(), 7);
    assert!(files.iter().all(|file| file["language"] == "shell"));
    assert!(files.iter().any(|file| {
        file["source_classification"] == "shell_project_metadata" && file["source_role"] == "test"
    }));

    let symbols = client
        .call_tool(CallToolRequestParams::new("symbol_search").with_arguments(
            serde_json::from_value(serde_json::json!({ "query": "refund_provider", "limit": 5 }))?,
        ))
        .await?
        .structured_content
        .ok_or("Shell symbol_search response missing")?;
    let target = symbols["data"]["candidates"]
        .as_array()
        .and_then(|candidates| candidates.first())
        .ok_or("Shell refund_provider symbol missing")?;
    assert_eq!(target["language"], "shell");
    assert_eq!(target["precision"], "syntax");
    assert_eq!(target["provenance"], "tree_sitter");

    let symbol_ref = serde_json::json!({
        "by_id": { "id": target["id"], "revision": symbols["revision"] }
    });
    let callers =
        client
            .call_tool(CallToolRequestParams::new("callers").with_arguments(
                serde_json::from_value(serde_json::json!({ "symbol": symbol_ref, "limit": 10 }))?,
            ))
            .await?
            .structured_content
            .ok_or("Shell callers response missing")?;
    assert_eq!(callers["data"]["callers"].as_array().map(Vec::len), Some(1));

    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn cpp_fixture_is_queryable_through_structured_mcp_tools()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let (_repository, engine) = indexed_cpp_fixture_engine()?;
    let server = ChakraMcpServer::new(Arc::new(engine));
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await?;

    let repo_map = client
        .call_tool(
            CallToolRequestParams::new("repo_map").with_arguments(serde_json::from_value(
                serde_json::json!({ "include_languages": ["cpp"], "limit": 20 }),
            )?),
        )
        .await?
        .structured_content
        .ok_or("C++ repo_map response missing")?;
    let files = repo_map["data"]["files"]
        .as_array()
        .ok_or("C++ repo_map files missing")?;
    assert_eq!(files.len(), 7);
    assert!(files.iter().all(|file| file["language"] == "cpp"));
    assert!(files.iter().any(|file| {
        file["source_classification"] == "cpp_project_metadata" && file["source_role"] == "test"
    }));

    let symbols = client
        .call_tool(CallToolRequestParams::new("symbol_search").with_arguments(
            serde_json::from_value(serde_json::json!({ "query": "provider_refund", "limit": 5 }))?,
        ))
        .await?
        .structured_content
        .ok_or("C++ symbol_search response missing")?;
    let target = symbols["data"]["candidates"]
        .as_array()
        .and_then(|candidates| candidates.first())
        .ok_or("C++ provider_refund symbol missing")?;
    assert_eq!(target["language"], "cpp");
    assert_eq!(target["precision"], "syntax");
    assert_eq!(target["provenance"], "tree_sitter");

    let symbol_ref = serde_json::json!({
        "by_id": { "id": target["id"], "revision": symbols["revision"] }
    });
    let callers =
        client
            .call_tool(CallToolRequestParams::new("callers").with_arguments(
                serde_json::from_value(serde_json::json!({ "symbol": symbol_ref, "limit": 10 }))?,
            ))
            .await?
            .structured_content
            .ok_or("C++ callers response missing")?;
    assert_eq!(callers["data"]["callers"].as_array().map(Vec::len), Some(1));

    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn hcl_fixture_is_queryable_through_structured_mcp_tools()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let (_repository, engine) = indexed_hcl_fixture_engine()?;
    let server = ChakraMcpServer::new(Arc::new(engine));
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await?;

    let repo_map = client
        .call_tool(
            CallToolRequestParams::new("repo_map").with_arguments(serde_json::from_value(
                serde_json::json!({ "include_languages": ["hcl"], "limit": 20 }),
            )?),
        )
        .await?
        .structured_content
        .ok_or("HCL repo_map response missing")?;
    let files = repo_map["data"]["files"]
        .as_array()
        .ok_or("HCL repo_map files missing")?;
    assert_eq!(files.len(), 8);
    assert!(files.iter().all(|file| file["language"] == "hcl"));
    assert!(files.iter().any(|file| {
        file["source_classification"] == "terraform_module_metadata"
            && file["source_role"] == "test"
    }));

    let symbols = client
        .call_tool(CallToolRequestParams::new("symbol_search").with_arguments(
            serde_json::from_value(
                serde_json::json!({ "query": "null_resource::provider", "limit": 10 }),
            )?,
        ))
        .await?
        .structured_content
        .ok_or("HCL symbol_search response missing")?;
    let target = symbols["data"]["candidates"]
        .as_array()
        .and_then(|candidates| {
            candidates.iter().find(|candidate| {
                candidate["qualified_name"] == "resource::null_resource::provider"
            })
        })
        .ok_or("HCL provider resource symbol missing")?;
    assert_eq!(target["language"], "hcl");
    assert_eq!(target["precision"], "syntax");
    assert_eq!(target["provenance"], "tree_sitter");

    let symbol_ref = serde_json::json!({
        "by_id": { "id": target["id"], "revision": symbols["revision"] }
    });
    let callers =
        client
            .call_tool(CallToolRequestParams::new("callers").with_arguments(
                serde_json::from_value(serde_json::json!({ "symbol": symbol_ref, "limit": 10 }))?,
            ))
            .await?
            .structured_content
            .ok_or("HCL callers response missing")?;
    assert_eq!(callers["data"]["callers"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        callers["data"]["callers"][0]["symbol"]["qualified_name"],
        "resource::null_resource::service"
    );

    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn go_fixture_is_queryable_through_structured_mcp_tools()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let (_repository, engine) = indexed_go_fixture_engine()?;
    let server = ChakraMcpServer::new(Arc::new(engine));
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await?;

    let repo_map = client
        .call_tool(
            CallToolRequestParams::new("repo_map").with_arguments(serde_json::from_value(
                serde_json::json!({ "include_languages": ["go"], "limit": 20 }),
            )?),
        )
        .await?
        .structured_content
        .ok_or("Go repo_map response missing")?;
    let files = repo_map["data"]["files"]
        .as_array()
        .ok_or("Go repo_map files missing")?;
    assert_eq!(files.len(), 7);
    assert!(files.iter().all(|file| file["language"] == "go"));
    assert!(files.iter().any(|file| {
        file["source_classification"] == "go_module_metadata" && file["source_role"] == "test"
    }));

    let symbols = client
        .call_tool(CallToolRequestParams::new("symbol_search").with_arguments(
            serde_json::from_value(serde_json::json!({ "query": "providerRefund", "limit": 10 }))?,
        ))
        .await?
        .structured_content
        .ok_or("Go symbol_search response missing")?;
    let target = symbols["data"]["candidates"]
        .as_array()
        .and_then(|candidates| {
            candidates
                .iter()
                .find(|candidate| candidate["qualified_name"] == "payments::providerRefund")
        })
        .ok_or("Go providerRefund symbol missing")?;
    assert_eq!(target["language"], "go");
    assert_eq!(target["precision"], "syntax");
    assert_eq!(target["provenance"], "tree_sitter");

    let symbol_ref = serde_json::json!({
        "by_id": { "id": target["id"], "revision": symbols["revision"] }
    });
    let callers =
        client
            .call_tool(CallToolRequestParams::new("callers").with_arguments(
                serde_json::from_value(serde_json::json!({ "symbol": symbol_ref, "limit": 10 }))?,
            ))
            .await?
            .structured_content
            .ok_or("Go callers response missing")?;
    assert_eq!(callers["data"]["callers"].as_array().map(Vec::len), Some(2));

    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;
    Ok(())
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
    // No precise provider is installed on this engine, so status must report
    // an empty provider list rather than a fabricated entry.
    assert_eq!(
        status["data"]["providers"].as_array().map(Vec::len),
        Some(0)
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
    assert_eq!(repo_map["data"]["files"][0]["language"], "rust");
    assert!(
        repo_map["data"]["overview"]
            .as_array()
            .is_some_and(|groups| !groups.is_empty())
    );

    let page_started = Instant::now();
    let first_page_args = serde_json::from_value(serde_json::json!({
        "include_languages": ["rust"],
        "limit": 2
    }))?;
    let first_page = client
        .call_tool(CallToolRequestParams::new("repo_map").with_arguments(first_page_args))
        .await?
        .structured_content
        .ok_or("first repo_map page missing")?;
    let cursor = first_page["data"]["next_cursor"].clone();
    assert!(cursor.is_object());
    assert_eq!(cursor["workspace_id"], first_page["workspace_id"]);
    let second_page_args = serde_json::from_value(serde_json::json!({
        "cursor": cursor,
        "limit": 2
    }))?;
    let second_page = client
        .call_tool(CallToolRequestParams::new("repo_map").with_arguments(second_page_args))
        .await?
        .structured_content
        .ok_or("second repo_map page missing")?;
    assert_eq!(second_page["revision"], first_page["revision"]);
    assert_eq!(
        second_page["data"]["overview"].as_array().map(Vec::len),
        Some(0)
    );
    let encoded_page_bytes = serde_json::to_vec(&first_page)?.len();
    assert!(encoded_page_bytes < 1024 * 1024);
    eprintln!(
        "repo_map_mcp_page: elapsed={:?}, bytes={encoded_page_bytes}",
        page_started.elapsed()
    );

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

    let filtered_args = serde_json::from_value(serde_json::json!({
        "query": "PaymentService",
        "include_languages": ["rust"],
        "include_kinds": ["struct"],
        "exclude_kinds": ["import"],
        "namespace_prefix": "service::payment_service::",
        "source": {
            "path_prefix": "src/service",
            "include_roles": ["production"]
        },
        "limit": 5
    }))?;
    let filtered = client
        .call_tool(CallToolRequestParams::new("symbol_search").with_arguments(filtered_args))
        .await?
        .structured_content
        .ok_or("filtered symbol_search must return structured content")?;
    let filtered = filtered["data"]["candidates"]
        .as_array()
        .ok_or("filtered candidates missing")?;
    assert_eq!(filtered.len(), 1);
    assert_eq!(
        filtered[0]["qualified_name"],
        "service::payment_service::PaymentService"
    );
    assert_eq!(filtered[0]["source_role"], "production");

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
    assert_eq!(diff["data"]["scope"]["requested"]["kind"], "worktree");
    assert!(
        diff["data"]["scope"]["base_commit"]
            .as_str()
            .is_some_and(|commit| commit.len() >= 40)
    );
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

    let base_ref_args = serde_json::from_value(serde_json::json!({
        "scope": {
            "kind": "base_ref",
            "reference": "fixture-base"
        },
        "limit": 20
    }))?;
    let base_ref_diff = client
        .call_tool(CallToolRequestParams::new("diff_context").with_arguments(base_ref_args))
        .await?
        .structured_content
        .ok_or("base-ref diff_context must return structured content")?;
    assert_eq!(
        base_ref_diff["data"]["scope"]["requested"],
        serde_json::json!({
            "kind": "base_ref",
            "reference": "fixture-base"
        })
    );
    assert_eq!(
        base_ref_diff["data"]["scope"]["base_commit"],
        diff["data"]["scope"]["base_commit"]
    );
    assert_eq!(
        base_ref_diff["data"]["changed_files"],
        diff["data"]["changed_files"]
    );

    let bounded_args = serde_json::from_value(serde_json::json!({ "limit": 1 }))?;
    let bounded = client
        .call_tool(CallToolRequestParams::new("diff_context").with_arguments(bounded_args))
        .await?
        .structured_content
        .ok_or("bounded diff_context must return structured content")?;
    assert_eq!(bounded["truncated"], true);
    assert!(
        bounded["truncation"]
            .as_array()
            .is_some_and(|details| !details.is_empty())
    );
    assert!(bounded["truncation"].as_array().is_some_and(|details| {
        details.iter().any(|detail| {
            detail["section"] == "diff_context_changed_symbols" && detail["cause"] == "item_limit"
        })
    }));
    for section in [
        "changed_files",
        "changed_symbols",
        "related_callers",
        "related_tests",
        "related_relations",
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
async fn laravel_relationships_are_queryable_through_mcp()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let repository = TempDir::new()?;
    git(repository.path(), &["init", "--quiet"])?;
    git(
        repository.path(),
        &["config", "user.email", "tests@example.invalid"],
    )?;
    git(repository.path(), &["config", "user.name", "Chakra Tests"])?;
    copy_fixture_tree(&laravel_fixture_root(), repository.path())?;
    git(
        repository.path(),
        &["add", "composer.json", "app", "routes"],
    )?;
    git(repository.path(), &["commit", "--quiet", "-m", "base"])?;
    let model_path = repository.path().join("app/Models/User.php");
    let model_source = fs::read_to_string(&model_path)?;
    fs::write(
        &model_path,
        format!("{model_source}\n// current policy edit\n"),
    )?;
    let report = index_repository(repository.path())?;
    assert!(report.metrics.laravel_detected);
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

    let model_context =
        client
            .call_tool(CallToolRequestParams::new("context").with_arguments(
                serde_json::from_value(serde_json::json!({
                    "symbol": { "by_name": "App::Models::User" },
                    "limit": 20
                }))?,
            ))
            .await?
            .structured_content
            .ok_or("Laravel model context missing")?;
    let policy = model_context["data"]["related_relations"]
        .as_array()
        .and_then(|relations| {
            relations.iter().find(|relation| {
                relation["direction"] == "outgoing"
                    && relation["relation"]["edge_kind"] == "AUTHORIZES_WITH"
            })
        })
        .ok_or("policy relationship missing")?;
    assert_eq!(
        policy["relation"]["symbol"]["qualified_name"],
        "App::Policies::UserPolicy"
    );
    assert_eq!(policy["relation"]["provenance"], "heuristic");
    assert_eq!(policy["relation"]["precision"], "heuristic");

    let controller_context =
        client
            .call_tool(CallToolRequestParams::new("context").with_arguments(
                serde_json::from_value(serde_json::json!({
                    "symbol": { "by_name": "App::Http::Controllers::UserController::show" },
                    "limit": 20
                }))?,
            ))
            .await?
            .structured_content
            .ok_or("Laravel controller context missing")?;
    assert!(
        controller_context["data"]["related_relations"]
            .as_array()
            .is_some_and(|relations| relations.iter().any(|relation| {
                relation["direction"] == "incoming"
                    && relation["relation"]["edge_kind"] == "ROUTES_TO"
                    && relation["relation"]["symbol"]["kind"] == "configuration"
            }))
    );

    let diff = client
        .call_tool(
            CallToolRequestParams::new("diff_context")
                .with_arguments(serde_json::from_value(serde_json::json!({ "limit": 20 }))?),
        )
        .await?
        .structured_content
        .ok_or("Laravel diff context missing")?;
    let changed_user = diff["data"]["changed_symbols"]
        .as_array()
        .and_then(|symbols| {
            symbols
                .iter()
                .find(|symbol| symbol["symbol"]["qualified_name"] == "App::Models::User")
        })
        .ok_or("changed Laravel model missing")?;
    let changed_user_id = &changed_user["symbol"]["id"];
    assert!(
        diff["data"]["related_relations"]
            .as_array()
            .is_some_and(|relations| relations.iter().any(|relation| {
                relation["changed_symbol_id"] == *changed_user_id
                    && relation["relation"]["direction"] == "outgoing"
                    && relation["relation"]["relation"]["edge_kind"] == "AUTHORIZES_WITH"
            }))
    );

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
use App\Service\PaymentService;
final class PaymentController {
    public function __construct(private PaymentService $service) {}
    public function refund(): void { $this->service->refund(100); }
}
"#,
    )?;
    fs::create_dir_all(repository.path().join("tests"))?;
    fs::write(
        repository.path().join("tests/PaymentServiceTest.php"),
        r#"<?php
namespace App\Tests;
use App\Service\PaymentService;
final class PaymentServiceTest {
    public function testRefundTwice(): void {
        $service = new PaymentService();
        $service->refund(100);
        $service->refund(200);
    }
    public function testDynamicRefund($service): void { $service->refund(300); }
}
"#,
    )?;
    git(repository.path(), &["add", "src", "tests"])?;
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
    let php_callers = context["data"]["callers"]
        .as_array()
        .ok_or("PHP callers missing")?;
    let controller_caller = php_callers
        .iter()
        .find(|caller| caller["symbol"]["qualified_name"] == "App::Api::PaymentController::refund")
        .ok_or("PHP controller caller missing")?;
    // Strict-tier receiver evidence (typed promoted property) is promoted to
    // the precise tier under Chakra's own resolver provenance (ADR-0030).
    assert_eq!(controller_caller["provenance"], "chakra_resolver");
    assert_eq!(controller_caller["precision"], "precise");
    assert_eq!(
        controller_caller["representative_call_sites"][0]["receiver_type"],
        "App::Service::PaymentService"
    );
    assert_eq!(
        controller_caller["representative_call_sites"][0]["receiver_type_source"],
        "promoted_property"
    );
    let php_tests = context["data"]["tests"]
        .as_array()
        .ok_or("PHP tests missing")?;
    assert_eq!(php_tests.len(), 1);
    assert_eq!(
        php_tests[0]["symbol"]["qualified_name"],
        "App::Tests::PaymentServiceTest::testRefundTwice"
    );
    assert_eq!(php_tests[0]["provenance"], "chakra_resolver");
    assert_eq!(php_tests[0]["precision"], "precise");
    assert_eq!(
        php_tests[0]["representative_call_sites"][0]["receiver_type"],
        "App::Service::PaymentService"
    );
    assert_eq!(
        php_tests[0]["representative_call_sites"][0]["receiver_type_source"],
        "local_new"
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
            .is_some_and(|items| items.is_empty())
    );
    assert_eq!(
        controller_context["data"]["callees"][0]["symbol"]["qualified_name"],
        "App::Service::PaymentService::refund"
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
    let related_tests = diff["data"]["related_tests"]
        .as_array()
        .ok_or("PHP diff related tests missing")?;
    assert_eq!(related_tests.len(), 1);
    assert_eq!(
        related_tests[0]["relation"]["symbol"]["qualified_name"],
        "App::Tests::PaymentServiceTest::testRefundTwice"
    );
    assert_eq!(
        related_tests[0]["relation"]["representative_call_sites"][0]["receiver_type_source"],
        "local_new"
    );

    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;
    Ok(())
}
