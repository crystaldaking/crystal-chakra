//! Query-service behavior over the Controller → Service → Provider scenario.

mod common;

use std::collections::BTreeSet;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use chakra_domain::diagnostic::{
    DiagnosticTruncationCause, SyntaxDiagnostic, SyntaxDiagnosticCause, SyntaxDiagnosticKind,
};
use chakra_domain::envelope::{TruncationCause, TruncationSection};
use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::operation::OperationContext;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    CallersRequest, ContextRequest, DiffContextRequest, ProviderFallbackCause, QueryError,
    QueryService, RepoMapRequest, SearchRequest, SourceFilter, StatusRequest, SymbolRef,
    SymbolSearchRequest,
};
use chakra_domain::revision::Revision;
use chakra_domain::source::{SourceClassification, SourceMetadata, SourcePackage, SourceRole};
use chakra_domain::state::{Freshness, FreshnessRequirement, ProviderState, WorkspaceStatus};
use chakra_domain::symbol::{
    CallForm, CallResolution, CallTargetKind, EdgeKind, Language, SymbolKey, SymbolKind,
};
use chakra_engine::{
    CallSiteInput, FreshnessBarrier, FreshnessBarrierError, PreciseProvider, PreciseQueryRequest,
    PreciseQueryResult, PreciseRelation, SymbolGraph, WorkspaceEngine,
};

use common::{scenario_engine, scenario_graph};

#[derive(Debug)]
struct FixedProvider {
    result: PreciseQueryResult,
    last_error: Option<&'static str>,
}

#[derive(Debug)]
struct CountingRustProvider {
    calls: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct RevisionAdvancingProvider {
    engine: Weak<WorkspaceEngine>,
    result: PreciseQueryResult,
}

#[derive(Debug)]
struct AdvanceAfterProviderBarrier {
    engine: Weak<WorkspaceEngine>,
    calls: AtomicUsize,
}

#[derive(Debug, Default)]
struct FailAfterProviderBarrier {
    calls: AtomicUsize,
}

impl FreshnessBarrier for AdvanceAfterProviderBarrier {
    fn require_fresh_with_context(
        &self,
        _operation: &OperationContext,
    ) -> Result<(), FreshnessBarrierError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            let engine = self
                .engine
                .upgrade()
                .ok_or_else(|| FreshnessBarrierError::new("test engine was dropped"))?;
            engine
                .publish(engine.begin_update())
                .map_err(|error| FreshnessBarrierError::new(error.to_string()))?;
        }
        Ok(())
    }
}

impl FreshnessBarrier for FailAfterProviderBarrier {
    fn require_fresh_with_context(
        &self,
        _operation: &OperationContext,
    ) -> Result<(), FreshnessBarrierError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            return Err(FreshnessBarrierError::new(
                "post-provider reconciliation failed",
            ));
        }
        Ok(())
    }
}

impl PreciseProvider for CountingRustProvider {
    fn name(&self) -> &'static str {
        "counting-rust-provider"
    }

    fn supports(&self, language: Language) -> bool {
        language == Language::Rust
    }

    fn state_for(&self, _revision: Revision) -> ProviderState {
        ProviderState::Ready
    }

    fn enrich_with_context(
        &self,
        request: PreciseQueryRequest,
        _operation: &OperationContext,
    ) -> PreciseQueryResult {
        self.calls.fetch_add(1, Ordering::Relaxed);
        PreciseQueryResult::unavailable(request.workspace.revision, ProviderState::Degraded)
    }
}

impl PreciseProvider for FixedProvider {
    fn name(&self) -> &'static str {
        "fixed-provider"
    }

    fn supports(&self, language: chakra_domain::symbol::Language) -> bool {
        language == chakra_domain::symbol::Language::Rust
    }

    fn state_for(&self, revision: Revision) -> ProviderState {
        if self.result.state == ProviderState::Ready && self.result.revision != revision {
            ProviderState::CatchingUp
        } else {
            self.result.state
        }
    }

    fn last_error(&self) -> Option<String> {
        self.last_error.map(str::to_owned)
    }

    fn enrich_with_context(
        &self,
        _request: PreciseQueryRequest,
        _operation: &OperationContext,
    ) -> PreciseQueryResult {
        self.result.clone()
    }
}

impl PreciseProvider for RevisionAdvancingProvider {
    fn name(&self) -> &'static str {
        "revision-advancing-provider"
    }

    fn supports(&self, language: Language) -> bool {
        language == Language::Rust
    }

    fn state_for(&self, _revision: Revision) -> ProviderState {
        ProviderState::Ready
    }

    fn enrich_with_context(
        &self,
        _request: PreciseQueryRequest,
        _operation: &OperationContext,
    ) -> PreciseQueryResult {
        if let Some(engine) = self.engine.upgrade() {
            let mut update = engine.begin_update();
            update.set_status(WorkspaceStatus::Stale);
            update.set_freshness(Freshness::Stale);
            let _ = engine.publish(update);
        }
        self.result.clone()
    }
}

fn add_search_symbol(
    graph: &mut SymbolGraph,
    path: &str,
    language: Language,
    qualified_name: &str,
    kind: SymbolKind,
    metadata: SourceMetadata,
) -> Result<(), Box<dyn Error>> {
    let path = RepoRelativePath::new(path)?;
    graph.add_file_with_metadata(path.clone(), "source\n", metadata)?;
    graph.add_symbol(
        SymbolKey {
            language,
            qualified_name: qualified_name.to_owned(),
            container: None,
            kind,
            path: path.clone(),
        },
        SourceRange::new(path, TextPosition::new(1, 1)?, TextPosition::new(1, 7)?)?,
        None,
        Provenance::TreeSitter,
        Precision::Syntax,
    )?;
    Ok(())
}

#[test]
fn status_reports_scenario_counts() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    let envelope = engine.status(StatusRequest)?;
    assert_eq!(
        envelope.schema_version,
        chakra_domain::envelope::SCHEMA_VERSION
    );
    assert_eq!(envelope.revision, Revision(1));
    assert_eq!(envelope.freshness, Freshness::Fresh);
    assert_eq!(envelope.status, WorkspaceStatus::Ready);
    assert_eq!(envelope.provider_state, ProviderState::NotConfigured);
    assert!(!envelope.truncated);
    assert_eq!(envelope.data.counts.symbols, 10);
    assert_eq!(envelope.data.counts.edges, 6);
    assert_eq!(envelope.data.counts.files, 3);
    assert_eq!(envelope.data.syntax_diagnostics.total_diagnostics, 0);
    assert!(!envelope.data.syntax_diagnostics.truncated);
    assert!(envelope.data.providers.is_empty());
    Ok(())
}

#[test]
fn status_reports_an_installed_provider_with_its_name_and_languages() -> Result<(), Box<dyn Error>>
{
    let (engine, _) = scenario_engine()?;
    engine.install_precise_provider(Arc::new(FixedProvider {
        result: PreciseQueryResult::unavailable(Revision(1), ProviderState::Ready),
        last_error: None,
    }))?;
    let envelope = engine.status(StatusRequest)?;
    assert_eq!(envelope.data.providers.len(), 1);
    let provider = &envelope.data.providers[0];
    assert_eq!(provider.name, "fixed-provider");
    assert_eq!(
        provider.languages,
        vec![chakra_domain::symbol::Language::Rust]
    );
    assert_eq!(provider.state, ProviderState::Ready);
    assert!(
        provider
            .capabilities
            .contains(&chakra_domain::query::ProviderCapability::IncomingCalls)
    );
    Ok(())
}

#[test]
fn context_aware_query_rejects_an_expired_operation() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    let operation = OperationContext::with_timeout(std::time::Duration::ZERO);
    let result = engine.repo_map_with_context(RepoMapRequest::default(), &operation);
    assert!(matches!(result, Err(QueryError::ExecutionDeadlineExceeded)));
    Ok(())
}

#[test]
fn status_reports_bounded_actionable_diagnostics_and_omission_causes() -> Result<(), Box<dyn Error>>
{
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = chakra_engine::WorkspaceEngine::new(identity);
    let mut graph = SymbolGraph::new();
    let rust_path = RepoRelativePath::new("src/overflow.rs")?;
    let rust_diagnostics = (0..64)
        .map(|column| {
            let start = TextPosition::new(1, column + 1)?;
            Ok(SyntaxDiagnostic {
                language: Language::Rust,
                range: SourceRange::new(rust_path.clone(), start, start)?,
                kind: SyntaxDiagnosticKind::Missing,
                provenance: Provenance::TreeSitter,
                precision: Precision::Syntax,
                cause: SyntaxDiagnosticCause::ParseRecovery,
                node_kind: ")".to_owned(),
            })
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    graph.add_file_with_metadata_and_diagnostics(
        rust_path,
        "broken\n",
        SourceMetadata::path_fallback(&RepoRelativePath::new("src/overflow.rs")?),
        rust_diagnostics,
        70,
    )?;
    for index in 0..40 {
        let path = RepoRelativePath::new(format!("php/Broken{index:02}.php"))?;
        let position = TextPosition::new(1, 1)?;
        graph.add_file_with_metadata_and_diagnostics(
            path.clone(),
            "<?php\n",
            SourceMetadata::path_fallback(&path),
            vec![SyntaxDiagnostic {
                language: Language::Php,
                range: SourceRange::new(path, position, position)?,
                kind: SyntaxDiagnosticKind::Error,
                provenance: Provenance::TreeSitter,
                precision: Precision::Syntax,
                cause: SyntaxDiagnosticCause::ParseRecovery,
                node_kind: "ERROR".to_owned(),
            }],
            1,
        )?;
    }
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let status = engine.status(StatusRequest)?;
    assert!(status.truncated);
    assert_eq!(status.data.syntax_diagnostics.files_with_diagnostics, 41);
    assert_eq!(status.data.syntax_diagnostics.total_diagnostics, 110);
    assert_eq!(status.data.syntax_diagnostics.diagnostics.len(), 100);
    assert_eq!(status.data.syntax_diagnostics.omitted_diagnostics, 10);
    assert_eq!(
        status.data.syntax_diagnostics.truncation_causes,
        [
            DiagnosticTruncationCause::PerFileLimit,
            DiagnosticTruncationCause::StatusLimit,
        ]
    );
    assert!(
        status
            .data
            .syntax_diagnostics
            .diagnostics
            .iter()
            .all(|item| { !item.node_kind.is_empty() && item.range.start().line() > 0 })
    );
    Ok(())
}

#[test]
fn repo_map_lists_files_sorted_with_counts() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    let envelope = engine.repo_map(RepoMapRequest::default())?;
    let files = &envelope.data.files;
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].path.as_str(), "src/api/controller.rs");
    assert_eq!(files[0].symbol_count, 2);
    assert_eq!(files[1].path.as_str(), "src/provider/mod.rs");
    assert_eq!(files[1].symbol_count, 4);
    assert_eq!(files[2].path.as_str(), "src/service/payment_service.rs");
    assert_eq!(files[2].symbol_count, 4);
    Ok(())
}

#[test]
fn repo_map_pages_every_large_repo_file_and_rejects_stale_cursors() -> Result<(), Box<dyn Error>> {
    const PHP_FILES: usize = 1_005;
    const RUST_FILES: usize = 1_005;
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = chakra_engine::WorkspaceEngine::new(identity);
    let mut graph = SymbolGraph::new();
    for index in 0..PHP_FILES {
        let path = RepoRelativePath::new(format!("app/Module{index:04}/Service.php"))?;
        graph.add_file_with_metadata(
            path,
            "<?php\n",
            SourceMetadata {
                role: SourceRole::Production,
                classification: SourceClassification::ComposerMetadata,
                package: Some(SourcePackage {
                    name: "vendor/app".to_owned(),
                    root: Some(RepoRelativePath::new("app")?),
                }),
            },
        )?;
    }
    for index in 0..RUST_FILES {
        let package_root = RepoRelativePath::new(format!("crates/package{index:04}"))?;
        graph.add_file_with_metadata(
            RepoRelativePath::new(format!("{package_root}/src/lib.rs"))?,
            "pub fn entry() {}\n",
            SourceMetadata {
                role: SourceRole::Production,
                classification: SourceClassification::CargoMetadata,
                package: Some(SourcePackage {
                    name: format!("package{index:04}"),
                    root: Some(package_root),
                }),
            },
        )?;
    }
    let mut update = engine.begin_update();
    update.replace_graph(graph.clone());
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let started = Instant::now();
    let first = engine.repo_map(RepoMapRequest {
        include_languages: vec![Language::Php],
        source: SourceFilter {
            package: Some(" vendor/app ".to_owned()),
            ..SourceFilter::default()
        },
        limit: Some(113),
        ..RepoMapRequest::default()
    })?;
    let first_again = engine.repo_map(RepoMapRequest {
        include_languages: vec![Language::Php],
        source: SourceFilter {
            package: Some("vendor/app".to_owned()),
            ..SourceFilter::default()
        },
        limit: Some(113),
        ..RepoMapRequest::default()
    })?;
    assert_eq!(first.data.files, first_again.data.files);
    assert_eq!(first.data.next_cursor, first_again.data.next_cursor);
    assert!(first.data.overview.iter().any(|group| {
        group.kind == chakra_domain::query::RepoMapGroupKind::TopLevelDirectory
            && group.root.as_ref().map(RepoRelativePath::as_str) == Some("app")
            && group.file_count == PHP_FILES as u64
    }));
    assert!(first.data.overview.iter().any(|group| {
        group.kind == chakra_domain::query::RepoMapGroupKind::ComposerPsr4
            && group.name == "vendor/app"
            && group.file_count == PHP_FILES as u64
    }));
    assert_eq!(
        first.data.source_metadata.composer_metadata_files,
        PHP_FILES as u64
    );
    let stale_cursor = first
        .data
        .next_cursor
        .clone()
        .ok_or("first page cursor missing")?;
    let conflicting_scope = engine.repo_map(RepoMapRequest {
        include_languages: vec![Language::Php],
        cursor: Some(stale_cursor.clone()),
        ..RepoMapRequest::default()
    });
    assert!(matches!(conflicting_scope, Err(QueryError::Invalid(_))));
    let other_identity =
        chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(&std::env::temp_dir())?;
    let other_engine = chakra_engine::WorkspaceEngine::new(other_identity);
    let mut other_update = other_engine.begin_update();
    other_update.replace_graph(SymbolGraph::new());
    other_update.set_status(WorkspaceStatus::Ready);
    other_update.set_freshness(Freshness::Fresh);
    other_engine.publish(other_update)?;
    let wrong_workspace = other_engine.repo_map(RepoMapRequest {
        cursor: Some(stale_cursor.clone()),
        ..RepoMapRequest::default()
    });
    assert!(matches!(
        wrong_workspace,
        Err(QueryError::CursorWorkspaceMismatch { .. })
    ));
    let mut all_paths: Vec<_> = first
        .data
        .files
        .into_iter()
        .map(|file| {
            assert_eq!(file.language, Language::Php);
            file.path
        })
        .collect();
    let mut cursor = first.data.next_cursor;
    while let Some(next) = cursor {
        let page = engine.repo_map(RepoMapRequest {
            cursor: Some(next),
            limit: Some(113),
            ..RepoMapRequest::default()
        })?;
        assert!(page.data.overview.is_empty());
        all_paths.extend(page.data.files.into_iter().map(|file| file.path));
        cursor = page.data.next_cursor;
    }
    assert_eq!(all_paths.len(), PHP_FILES);
    assert!(all_paths.windows(2).all(|paths| paths[0] < paths[1]));
    assert_eq!(
        all_paths.iter().cloned().collect::<BTreeSet<_>>().len(),
        PHP_FILES
    );

    let rust_first = engine.repo_map(RepoMapRequest {
        include_languages: vec![Language::Rust],
        limit: Some(127),
        ..RepoMapRequest::default()
    })?;
    assert!(rust_first.data.overview.iter().any(|group| {
        group.kind == chakra_domain::query::RepoMapGroupKind::TopLevelDirectory
            && group.root.as_ref().map(RepoRelativePath::as_str) == Some("crates")
            && group.file_count == RUST_FILES as u64
    }));
    assert!(rust_first.data.overview.iter().any(|group| {
        group.kind == chakra_domain::query::RepoMapGroupKind::CargoPackage
            && group.name == "package0000"
    }));
    let mut rust_count = rust_first.data.files.len();
    let mut rust_cursor = rust_first.data.next_cursor;
    while let Some(next) = rust_cursor {
        let page = engine.repo_map(RepoMapRequest {
            cursor: Some(next),
            limit: Some(127),
            ..RepoMapRequest::default()
        })?;
        rust_count += page.data.files.len();
        rust_cursor = page.data.next_cursor;
    }
    assert_eq!(rust_count, RUST_FILES);
    eprintln!(
        "repo_map_large_pagination: php_files={PHP_FILES}, rust_files={RUST_FILES}, elapsed={:?}",
        started.elapsed()
    );

    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    let stale = engine.repo_map(RepoMapRequest {
        cursor: Some(stale_cursor),
        limit: Some(113),
        ..RepoMapRequest::default()
    });
    assert!(matches!(stale, Err(QueryError::StaleCursor { .. })));
    Ok(())
}

#[test]
fn symbol_search_matches_names_and_respects_budgets() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;

    let found = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    // 4 refund methods + 2 test functions whose names contain "refund".
    assert_eq!(found.data.candidates.len(), 6);
    assert!(!found.truncated);
    assert!(
        found
            .data
            .candidates
            .iter()
            .all(|c| c.precision == Precision::Syntax)
    );

    let limited = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        limit: Some(2),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(limited.data.candidates.len(), 2);
    assert!(limited.truncated);
    assert_eq!(limited.truncation.len(), 1);
    assert_eq!(
        limited.truncation[0].section,
        TruncationSection::SymbolSearchCandidates
    );
    assert_eq!(limited.truncation[0].cause, TruncationCause::ItemLimit);

    let empty = engine.symbol_search(SymbolSearchRequest {
        query: "   ".to_owned(),
        ..SymbolSearchRequest::default()
    });
    assert!(matches!(empty, Err(QueryError::Invalid(_))));

    let oversized = engine.symbol_search(SymbolSearchRequest {
        query: "x".repeat(1_025),
        ..SymbolSearchRequest::default()
    });
    assert!(matches!(oversized, Err(QueryError::Invalid(_))));
    Ok(())
}

#[test]
fn exact_symbol_search_reaches_later_partition_before_bounded_scan() -> Result<(), Box<dyn Error>> {
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = chakra_engine::WorkspaceEngine::new(identity);
    let mut rust = SymbolGraph::new();
    for index in 0..1_025 {
        let path = format!("src/noise_{index:04}.rs");
        add_search_symbol(
            &mut rust,
            &path,
            Language::Rust,
            &format!("noise::item_{index:04}"),
            SymbolKind::Function,
            SourceMetadata::path_fallback(&RepoRelativePath::new(path.clone())?),
        )?;
    }
    let mut php = SymbolGraph::new();
    for index in 0..2 {
        let path = format!("app/Service{index}.php");
        add_search_symbol(
            &mut php,
            &path,
            Language::Php,
            &format!("App::Service{index}::run"),
            SymbolKind::Method,
            SourceMetadata::path_fallback(&RepoRelativePath::new(path.clone())?),
        )?;
    }
    let graph = SymbolGraph::merge([rust, php])?;
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let found = engine.symbol_search(SymbolSearchRequest {
        query: "RUN".to_owned(),
        limit: Some(20),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(found.data.candidates.len(), 2);
    assert!(
        found
            .data
            .candidates
            .iter()
            .all(|candidate| candidate.language == Language::Php)
    );
    assert!(found.truncation.iter().any(|detail| {
        detail.section == TruncationSection::SymbolSearchCandidates
            && detail.cause == TruncationCause::ExaminedWorkLimit
    }));
    Ok(())
}

#[test]
fn source_filters_scope_rust_and_php_without_hiding_default_results() -> Result<(), Box<dyn Error>>
{
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = chakra_engine::WorkspaceEngine::new(identity);
    let mut graph = SymbolGraph::new();
    let cases = [
        (
            "src/editor.rs",
            Language::Rust,
            "app::Editor",
            SourceMetadata {
                role: SourceRole::Production,
                classification: SourceClassification::CargoMetadata,
                package: Some(SourcePackage {
                    name: "app".to_owned(),
                    root: None,
                }),
            },
        ),
        (
            "tests/editor.rs",
            Language::Rust,
            "integration::Editor",
            SourceMetadata {
                role: SourceRole::Test,
                classification: SourceClassification::CargoMetadata,
                package: Some(SourcePackage {
                    name: "app".to_owned(),
                    root: None,
                }),
            },
        ),
        (
            "tests/fixtures/Editor.php",
            Language::Php,
            "Fixtures::Editor",
            SourceMetadata::path_fallback(&RepoRelativePath::new("tests/fixtures/Editor.php")?),
        ),
    ];
    for (path, language, qualified_name, metadata) in cases {
        let path = RepoRelativePath::new(path)?;
        graph.add_file_with_metadata(path.clone(), "source\n", metadata)?;
        graph.add_symbol(
            SymbolKey {
                language,
                qualified_name: qualified_name.to_owned(),
                container: None,
                kind: if language == Language::Php {
                    SymbolKind::Class
                } else {
                    SymbolKind::Struct
                },
                path: path.clone(),
            },
            SourceRange::new(path, TextPosition::new(1, 1)?, TextPosition::new(1, 7)?)?,
            None,
            Provenance::TreeSitter,
            Precision::Syntax,
        )?;
    }
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let all = engine.symbol_search(SymbolSearchRequest {
        query: "Editor".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(all.data.candidates.len(), 3);

    let production = engine.symbol_search(SymbolSearchRequest {
        query: "Editor".to_owned(),
        source: SourceFilter {
            package: Some("app".to_owned()),
            exclude_roles: vec![SourceRole::Test],
            ..SourceFilter::default()
        },
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(production.data.candidates.len(), 1);
    assert_eq!(production.data.candidates[0].qualified_name, "app::Editor");
    assert_eq!(
        production.data.candidates[0].source_role,
        SourceRole::Production
    );
    assert_eq!(
        production.data.candidates[0]
            .package
            .as_ref()
            .map(|package| package.name.as_str()),
        Some("app")
    );

    let fixture = engine.symbol_search(SymbolSearchRequest {
        query: "Editor".to_owned(),
        source: SourceFilter {
            include_roles: vec![SourceRole::Fixture],
            ..SourceFilter::default()
        },
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(fixture.data.candidates.len(), 1);
    assert_eq!(fixture.data.candidates[0].language, Language::Php);

    let test_map = engine.repo_map(RepoMapRequest {
        source: SourceFilter {
            path_prefix: Some("tests".to_owned()),
            include_roles: vec![SourceRole::Test],
            ..SourceFilter::default()
        },
        ..RepoMapRequest::default()
    })?;
    assert_eq!(test_map.data.files.len(), 1);
    assert_eq!(test_map.data.files[0].path.as_str(), "tests/editor.rs");
    assert_eq!(test_map.data.source_metadata.total_files, 3);
    Ok(())
}

#[test]
fn source_filters_reject_unbounded_or_invalid_input() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;

    let invalid_path = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        source: SourceFilter {
            path_prefix: Some("../outside".to_owned()),
            ..SourceFilter::default()
        },
        ..SymbolSearchRequest::default()
    });
    assert!(matches!(invalid_path, Err(QueryError::Invalid(_))));

    let empty_package = engine.repo_map(RepoMapRequest {
        source: SourceFilter {
            package: Some("  ".to_owned()),
            ..SourceFilter::default()
        },
        ..RepoMapRequest::default()
    });
    assert!(matches!(empty_package, Err(QueryError::Invalid(_))));

    let zero_page = engine.repo_map(RepoMapRequest {
        limit: Some(0),
        ..RepoMapRequest::default()
    });
    assert!(matches!(zero_page, Err(QueryError::Invalid(_))));

    let too_many_roles = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        source: SourceFilter {
            include_roles: vec![SourceRole::Production; 17],
            ..SourceFilter::default()
        },
        ..SymbolSearchRequest::default()
    });
    assert!(matches!(too_many_roles, Err(QueryError::Invalid(_))));

    let too_many_languages = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        include_languages: vec![Language::Rust; 17],
        ..SymbolSearchRequest::default()
    });
    assert!(matches!(too_many_languages, Err(QueryError::Invalid(_))));

    let too_many_kinds = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        include_kinds: vec![SymbolKind::Method; 17],
        ..SymbolSearchRequest::default()
    });
    assert!(matches!(too_many_kinds, Err(QueryError::Invalid(_))));

    let empty_namespace = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        namespace_prefix: Some("::".to_owned()),
        ..SymbolSearchRequest::default()
    });
    assert!(matches!(empty_namespace, Err(QueryError::Invalid(_))));

    let oversized_namespace = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        namespace_prefix: Some("n".repeat(1_025)),
        ..SymbolSearchRequest::default()
    });
    assert!(matches!(oversized_namespace, Err(QueryError::Invalid(_))));
    Ok(())
}

#[test]
fn symbol_search_ranks_declarations_and_applies_every_filter() -> Result<(), Box<dyn Error>> {
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = chakra_engine::WorkspaceEngine::new(identity);
    let mut graph = SymbolGraph::new();

    // Add noise first to prove a limit-one query still replaces it with the
    // best candidate discovered later in arena order.
    add_search_symbol(
        &mut graph,
        "app/Controller.php",
        Language::Php,
        "App::Controller::use App\\Service\\TransactionStatusService",
        SymbolKind::Import,
        SourceMetadata::path_fallback(&RepoRelativePath::new("app/Controller.php")?),
    )?;
    add_search_symbol(
        &mut graph,
        "crates/editor/tests/fixtures/editor.rs",
        Language::Rust,
        "snapshots::Editor",
        SymbolKind::Struct,
        SourceMetadata {
            role: SourceRole::Fixture,
            classification: SourceClassification::CargoMetadata,
            package: Some(SourcePackage {
                name: "editor".to_owned(),
                root: Some(RepoRelativePath::new("crates/editor")?),
            }),
        },
    )?;
    add_search_symbol(
        &mut graph,
        "crates/ui/src/import.rs",
        Language::Rust,
        "ui::use crate::core::Editor",
        SymbolKind::Import,
        SourceMetadata {
            role: SourceRole::Production,
            classification: SourceClassification::CargoMetadata,
            package: Some(SourcePackage {
                name: "ui".to_owned(),
                root: Some(RepoRelativePath::new("crates/ui")?),
            }),
        },
    )?;
    add_search_symbol(
        &mut graph,
        "tests/TransactionStatusService.php",
        Language::Php,
        "App::Tests::TransactionStatusService",
        SymbolKind::Class,
        SourceMetadata::path_fallback(&RepoRelativePath::new(
            "tests/TransactionStatusService.php",
        )?),
    )?;
    add_search_symbol(
        &mut graph,
        "app/Service/TransactionStatusService.php",
        Language::Php,
        "App::Service::TransactionStatusService",
        SymbolKind::Class,
        SourceMetadata::path_fallback(&RepoRelativePath::new(
            "app/Service/TransactionStatusService.php",
        )?),
    )?;
    add_search_symbol(
        &mut graph,
        "generated/EditorGenerated.rs",
        Language::Rust,
        "generated::EditorGenerated",
        SymbolKind::Struct,
        SourceMetadata::path_fallback(&RepoRelativePath::new("generated/EditorGenerated.rs")?),
    )?;
    add_search_symbol(
        &mut graph,
        "src/OtherEditor.php",
        Language::Php,
        "Other::Editor",
        SymbolKind::Class,
        SourceMetadata::path_fallback(&RepoRelativePath::new("src/OtherEditor.php")?),
    )?;
    add_search_symbol(
        &mut graph,
        "crates/editor/src/editor.rs",
        Language::Rust,
        "core::Editor",
        SymbolKind::Struct,
        SourceMetadata {
            role: SourceRole::Production,
            classification: SourceClassification::CargoMetadata,
            package: Some(SourcePackage {
                name: "editor".to_owned(),
                root: Some(RepoRelativePath::new("crates/editor")?),
            }),
        },
    )?;
    let mut update = engine.begin_update();
    update.replace_graph(graph.clone());
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let php = engine.symbol_search(SymbolSearchRequest {
        query: "TransactionStatusService".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(
        php.data.candidates[0].qualified_name,
        "App::Service::TransactionStatusService"
    );
    assert_eq!(php.data.candidates[0].kind, SymbolKind::Class);
    assert!(
        php.data
            .candidates
            .iter()
            .any(|candidate| candidate.kind == SymbolKind::Import)
    );

    let best_editor = engine.symbol_search(SymbolSearchRequest {
        query: "Editor".to_owned(),
        limit: Some(1),
        ..SymbolSearchRequest::default()
    })?;
    assert!(best_editor.truncated);
    assert_eq!(
        best_editor.data.candidates[0].qualified_name,
        "core::Editor"
    );

    let without_imports = engine.symbol_search(SymbolSearchRequest {
        query: "Editor".to_owned(),
        exclude_kinds: vec![SymbolKind::Import],
        ..SymbolSearchRequest::default()
    })?;
    assert!(
        without_imports
            .data
            .candidates
            .iter()
            .all(|candidate| candidate.kind != SymbolKind::Import)
    );

    let php_only = engine.symbol_search(SymbolSearchRequest {
        query: "Editor".to_owned(),
        include_languages: vec![Language::Php],
        ..SymbolSearchRequest::default()
    })?;
    assert!(
        php_only
            .data
            .candidates
            .iter()
            .all(|candidate| candidate.language == Language::Php)
    );

    let production_struct = engine.symbol_search(SymbolSearchRequest {
        query: "Editor".to_owned(),
        include_kinds: vec![SymbolKind::Struct],
        exclude_kinds: vec![SymbolKind::Import],
        namespace_prefix: Some("core::".to_owned()),
        source: SourceFilter {
            package: Some("editor".to_owned()),
            path_prefix: Some("crates/editor".to_owned()),
            exclude_roles: vec![SourceRole::Fixture, SourceRole::Generated],
            ..SourceFilter::default()
        },
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(production_struct.data.candidates.len(), 1);
    assert_eq!(
        production_struct.data.candidates[0].qualified_name,
        "core::Editor"
    );

    let partial_namespace = engine.symbol_search(SymbolSearchRequest {
        query: "Editor".to_owned(),
        namespace_prefix: Some("cor".to_owned()),
        ..SymbolSearchRequest::default()
    })?;
    assert!(partial_namespace.data.candidates.is_empty());

    let fixtures = engine.symbol_search(SymbolSearchRequest {
        query: "Editor".to_owned(),
        source: SourceFilter {
            include_roles: vec![SourceRole::Fixture],
            ..SourceFilter::default()
        },
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(fixtures.data.candidates.len(), 1);
    assert_eq!(
        fixtures.data.candidates[0].qualified_name,
        "snapshots::Editor"
    );

    let imports = engine.symbol_search(SymbolSearchRequest {
        query: "Editor".to_owned(),
        include_kinds: vec![SymbolKind::Import],
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(imports.data.candidates.len(), 1);
    assert_eq!(imports.data.candidates[0].kind, SymbolKind::Import);

    let ambiguous = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("Editor".to_owned())),
        ..ContextRequest::default()
    });
    assert!(matches!(ambiguous, Err(QueryError::AmbiguousSymbol { .. })));
    let resolved = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ById {
            id: best_editor.data.candidates[0].id,
            revision: best_editor.revision,
        }),
        ..ContextRequest::default()
    })?;
    assert_eq!(resolved.data.symbol.qualified_name, "core::Editor");

    let before_order: Vec<_> = engine
        .symbol_search(SymbolSearchRequest {
            query: "core::Editor".to_owned(),
            ..SymbolSearchRequest::default()
        })?
        .data
        .candidates
        .into_iter()
        .map(|candidate| (candidate.qualified_name, candidate.location))
        .collect();
    assert!(before_order.len() >= 2);
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    let after_order: Vec<_> = engine
        .symbol_search(SymbolSearchRequest {
            query: "core::Editor".to_owned(),
            ..SymbolSearchRequest::default()
        })?
        .data
        .candidates
        .into_iter()
        .map(|candidate| (candidate.qualified_name, candidate.location))
        .collect();
    assert_eq!(before_order, after_order);
    Ok(())
}

#[test]
fn bare_name_refund_is_ambiguous_not_guessed() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    let result = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName("refund".to_owned())),
        ..CallersRequest::default()
    });
    let expected = QueryError::AmbiguousSymbol {
        query: "refund".to_owned(),
        candidates: 4,
    };
    assert_eq!(result.err(), Some(expected));
    Ok(())
}

#[test]
fn qualified_name_resolves_unambiguously() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let envelope = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName(
            "service::payment_service::PaymentService::refund".to_owned(),
        )),
        ..CallersRequest::default()
    })?;
    assert_eq!(envelope.data.target.id, ids.service_refund);
    assert_eq!(envelope.data.callers.len(), 1);
    assert_eq!(envelope.data.callers[0].symbol.id, ids.controller_refund);
    Ok(())
}

#[test]
fn callers_of_provider_trait_method_is_the_service() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let revision = engine.snapshot().revision();
    let envelope = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision,
        }),
        ..CallersRequest::default()
    })?;
    assert_eq!(envelope.data.callers.len(), 1);
    let caller = &envelope.data.callers[0];
    assert_eq!(caller.symbol.id, ids.service_refund);
    // Syntax call candidates must never masquerade as precise (SPEC §7).
    assert_eq!(caller.precision, Precision::Syntax);
    assert_eq!(caller.provenance, Provenance::TreeSitter);
    assert_eq!(caller.occurrence_count, 1);
    assert_eq!(caller.representative_locations.len(), 1);
    Ok(())
}

#[test]
fn repeated_call_sites_are_aggregated_by_caller_and_target() -> Result<(), Box<dyn Error>> {
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = WorkspaceEngine::new(identity);
    let caller_path = RepoRelativePath::new("src/caller.rs")?;
    let test_path = RepoRelativePath::new("tests/caller.rs")?;
    let targets_path = RepoRelativePath::new("src/targets.rs")?;
    let mut graph = SymbolGraph::new();
    graph.add_file(caller_path.clone(), "pub fn invoke() {}\n")?;
    graph.add_file(test_path.clone(), "fn repeated_test() {}\n")?;
    graph.add_file(
        targets_path.clone(),
        "pub fn target() {}\npub fn unique() {}\n",
    )?;

    let add_symbol = |graph: &mut SymbolGraph,
                      path: &RepoRelativePath,
                      qualified_name: &str,
                      kind: SymbolKind,
                      line: u32|
     -> Result<_, Box<dyn Error>> {
        Ok(graph.add_symbol(
            SymbolKey {
                language: Language::Rust,
                qualified_name: qualified_name.to_owned(),
                container: None,
                kind,
                path: path.clone(),
            },
            SourceRange::new(
                path.clone(),
                TextPosition::new(line, 1)?,
                TextPosition::new(line, 12)?,
            )?,
            None,
            Provenance::TreeSitter,
            Precision::Syntax,
        )?)
    };
    let caller = add_symbol(
        &mut graph,
        &caller_path,
        "caller::invoke",
        SymbolKind::Function,
        1,
    )?;
    let test_caller = add_symbol(
        &mut graph,
        &test_path,
        "tests::repeated_test",
        SymbolKind::Test,
        1,
    )?;
    let target_a = add_symbol(
        &mut graph,
        &targets_path,
        "targets::a::target",
        SymbolKind::Function,
        1,
    )?;
    add_symbol(
        &mut graph,
        &targets_path,
        "targets::b::target",
        SymbolKind::Function,
        2,
    )?;
    let unique = add_symbol(
        &mut graph,
        &targets_path,
        "targets::unique",
        SymbolKind::Function,
        3,
    )?;

    let add_call = |graph: &mut SymbolGraph,
                    caller: chakra_domain::symbol::EntityId,
                    path: &RepoRelativePath,
                    line: u32,
                    name: &str|
     -> Result<CallResolution, Box<dyn Error>> {
        Ok(graph.add_call_site(CallSiteInput {
            caller,
            form: CallForm::Function,
            target_kind: CallTargetKind::Function,
            name: name.to_owned(),
            qualifier: None,
            receiver_type: None,
            receiver_type_source: None,
            receiver_hint: None,
            location: SourceRange::new(
                path.clone(),
                TextPosition::new(line, 1)?,
                TextPosition::new(line, 8)?,
            )?,
            provenance: Provenance::TreeSitter,
            precision: Precision::Syntax,
        })?)
    };
    for line in 1..=5 {
        assert_eq!(
            add_call(&mut graph, caller, &caller_path, line, "target")?,
            CallResolution::Ambiguous { candidates: 2 }
        );
        assert!(matches!(
            add_call(&mut graph, caller, &caller_path, line + 10, "unique")?,
            CallResolution::Resolved { target } if target == unique
        ));
    }
    for line in 1..=4 {
        assert!(matches!(
            add_call(&mut graph, test_caller, &test_path, line, "unique")?,
            CallResolution::Resolved { target } if target == unique
        ));
    }

    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    let revision = engine.publish(update)?.revision();

    let ambiguous = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: target_a,
            revision,
        }),
        ..CallersRequest::default()
    })?;
    assert_eq!(ambiguous.data.syntax_candidates.len(), 1);
    let candidate = &ambiguous.data.syntax_candidates[0];
    assert_eq!(candidate.caller.id, caller);
    assert_eq!(candidate.occurrence_count, 5);
    assert_eq!(candidate.representative_evidence.len(), 3);
    assert_eq!(candidate.evidence_omitted, 2);

    let exact = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ById {
            id: unique,
            revision,
        }),
        ..ContextRequest::default()
    })?;
    assert_eq!(exact.data.callers.len(), 2);
    let ordinary = exact
        .data
        .callers
        .iter()
        .find(|relation| relation.symbol.id == caller)
        .ok_or("ordinary caller missing")?;
    assert_eq!(ordinary.occurrence_count, 5);
    assert_eq!(ordinary.representative_locations.len(), 3);
    assert_eq!(ordinary.locations_omitted, 2);
    assert_eq!(exact.data.tests.len(), 1);
    assert_eq!(exact.data.tests[0].symbol.id, test_caller);
    assert_eq!(exact.data.tests[0].occurrence_count, 4);
    assert_eq!(exact.data.tests[0].representative_locations.len(), 3);
    assert_eq!(exact.data.tests[0].locations_omitted, 1);
    Ok(())
}

#[test]
fn response_sections_enforce_exact_byte_budgets_for_multibyte_paths() -> Result<(), Box<dyn Error>>
{
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = WorkspaceEngine::new(identity);
    let mut graph = SymbolGraph::new();
    let long_component = "界".repeat(400);
    for index in 0..500 {
        graph.add_file(
            RepoRelativePath::new(format!("src/{index:03}-{long_component}.rs"))?,
            "",
        )?;
    }
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let result = engine.repo_map(RepoMapRequest {
        limit: Some(500),
        ..RepoMapRequest::default()
    })?;
    assert!(result.data.files.len() < 500);
    let encoded = serde_json::to_vec(&result.data.files)?;
    assert!(encoded.len() <= 256 * 1024);
    assert!(result.truncation.iter().any(|detail| {
        detail.section == TruncationSection::RepoMapFiles
            && detail.cause == TruncationCause::ResponseByteLimit
            && detail.limit == 256 * 1024
            && detail.omitted.is_some_and(|bytes| bytes > 0)
    }));
    Ok(())
}

#[test]
fn context_source_byte_budget_handles_multibyte_snippets() -> Result<(), Box<dyn Error>> {
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = WorkspaceEngine::new(identity);
    let path = RepoRelativePath::new("src/multibyte.rs")?;
    let source = format!("pub fn multibyte() {{ /* {} */ }}", "🦀".repeat(5_000));
    let end_column = u32::try_from(source.chars().count() + 1)?;
    let mut graph = SymbolGraph::new();
    graph.add_file(path.clone(), source)?;
    graph.add_symbol(
        SymbolKey {
            language: Language::Rust,
            qualified_name: "multibyte::multibyte".to_owned(),
            container: None,
            kind: SymbolKind::Function,
            path: path.clone(),
        },
        SourceRange::new(
            path,
            TextPosition::new(1, 1)?,
            TextPosition::new(1, end_column)?,
        )?,
        Some("pub fn multibyte()".to_owned()),
        Provenance::TreeSitter,
        Precision::Syntax,
    )?;
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let result = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("multibyte::multibyte".to_owned())),
        ..ContextRequest::default()
    })?;
    let source = result.data.source.as_ref().ok_or("source missing")?;
    assert!(source.truncated);
    assert!(serde_json::to_vec(source)?.len() <= 16 * 1024);
    assert!(result.truncation.iter().any(|detail| {
        detail.section == TruncationSection::ContextSource
            && detail.cause == TruncationCause::ResponseByteLimit
            && detail.limit == 16 * 1024
            && detail.omitted.is_some_and(|bytes| bytes > 0)
    }));
    Ok(())
}

#[test]
fn noisy_caller_section_cannot_starve_tests_or_declaration() -> Result<(), Box<dyn Error>> {
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = WorkspaceEngine::new(identity);
    let target_path = RepoRelativePath::new("src/target.rs")?;
    let mut graph = SymbolGraph::new();
    graph.add_file(target_path.clone(), "pub fn target() {}\n")?;
    let target = graph.add_symbol(
        SymbolKey {
            language: Language::Rust,
            qualified_name: "target::target".to_owned(),
            container: None,
            kind: SymbolKind::Function,
            path: target_path.clone(),
        },
        SourceRange::new(
            target_path,
            TextPosition::new(1, 1)?,
            TextPosition::new(1, 19)?,
        )?,
        Some("pub fn target()".to_owned()),
        Provenance::TreeSitter,
        Precision::Syntax,
    )?;

    let long_component = "caller".repeat(100);
    for index in 0..200 {
        let path = RepoRelativePath::new(format!("src/callers/{index:03}-{long_component}.rs"))?;
        let caller = graph.add_symbol(
            SymbolKey {
                language: Language::Rust,
                qualified_name: format!("callers::{index:03}::{long_component}"),
                container: None,
                kind: SymbolKind::Function,
                path: path.clone(),
            },
            SourceRange::new(
                path.clone(),
                TextPosition::new(1, 1)?,
                TextPosition::new(1, 8)?,
            )?,
            None,
            Provenance::TreeSitter,
            Precision::Syntax,
        )?;
        graph.add_edge(
            chakra_domain::symbol::EdgeKind::Calls,
            caller,
            target,
            Provenance::TreeSitter,
            Precision::Syntax,
            Some(SourceRange::new(
                path,
                TextPosition::new(1, 1)?,
                TextPosition::new(1, 8)?,
            )?),
        )?;
    }

    let test_path = RepoRelativePath::new("tests/target.rs")?;
    let test = graph.add_symbol(
        SymbolKey {
            language: Language::Rust,
            qualified_name: "tests::target_is_called".to_owned(),
            container: None,
            kind: SymbolKind::Test,
            path: test_path.clone(),
        },
        SourceRange::new(
            test_path,
            TextPosition::new(1, 1)?,
            TextPosition::new(1, 20)?,
        )?,
        None,
        Provenance::TreeSitter,
        Precision::Syntax,
    )?;
    graph.add_edge(
        chakra_domain::symbol::EdgeKind::Tests,
        test,
        target,
        Provenance::Heuristic,
        Precision::Heuristic,
        None,
    )?;

    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    let revision = engine.publish(update)?.revision();
    let result = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ById {
            id: target,
            revision,
        }),
        limit: Some(500),
        ..ContextRequest::default()
    })?;

    assert_eq!(result.data.symbol.id, target);
    assert_eq!(result.data.tests.len(), 1);
    assert_eq!(result.data.tests[0].symbol.id, test);
    assert!(result.data.callers.len() < 200);
    assert!(result.truncation.iter().any(|detail| {
        detail.section == TruncationSection::ContextCallers
            && detail.cause == TruncationCause::ResponseByteLimit
    }));
    assert!(!result.truncation.iter().any(|detail| {
        detail.section == TruncationSection::ContextTests
            && detail.cause == TruncationCause::ResponseByteLimit
    }));
    Ok(())
}

#[test]
fn current_precise_callers_replace_matching_syntax_candidates() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let revision = engine.snapshot().revision();
    let caller = engine
        .snapshot()
        .graph()
        .symbol(ids.controller_refund)
        .ok_or("controller symbol missing")?
        .clone();
    engine.install_precise_provider(Arc::new(FixedProvider {
        result: PreciseQueryResult {
            revision,
            state: ProviderState::Ready,
            fallback_cause: None,
            incoming: vec![
                PreciseRelation {
                    name: caller.name().to_owned(),
                    declaration: caller.location.clone(),
                    occurrence_count: 3,
                    call_sites: vec![caller.location.clone(), caller.location.clone()],
                    provenance: Provenance::RustAnalyzer,
                },
                PreciseRelation {
                    name: caller.name().to_owned(),
                    declaration: caller.location.clone(),
                    occurrence_count: 2,
                    call_sites: vec![caller.location],
                    provenance: Provenance::RustAnalyzer,
                },
            ],
            outgoing: Vec::new(),
            incoming_truncated: true,
            outgoing_truncated: false,
        },
        last_error: None,
    }))?;

    let envelope = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.service_refund,
            revision,
        }),
        ..CallersRequest::default()
    })?;
    assert_eq!(envelope.provider_state, ProviderState::Ready);
    assert_eq!(envelope.data.callers.len(), 1);
    assert_eq!(envelope.data.callers[0].symbol.id, ids.controller_refund);
    assert_eq!(envelope.data.callers[0].precision, Precision::Precise);
    assert_eq!(
        envelope.data.callers[0].provenance,
        Provenance::RustAnalyzer
    );
    assert_eq!(envelope.data.callers[0].occurrence_count, 5);
    assert_eq!(envelope.data.callers[0].representative_locations.len(), 3);
    assert_eq!(envelope.data.callers[0].locations_omitted, 2);
    assert!(envelope.truncated);
    assert!(envelope.truncation.iter().any(|detail| {
        detail.section == TruncationSection::CallersCallers
            && detail.cause == TruncationCause::ProviderLimit
    }));
    Ok(())
}

#[test]
fn older_precise_result_is_never_labeled_current_after_revision_change()
-> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let old_revision = engine.snapshot().revision();
    let caller = engine
        .snapshot()
        .graph()
        .symbol(ids.controller_refund)
        .ok_or("controller symbol missing")?
        .clone();
    engine.install_precise_provider(Arc::new(FixedProvider {
        result: PreciseQueryResult {
            revision: old_revision,
            state: ProviderState::Ready,
            fallback_cause: None,
            incoming: vec![PreciseRelation {
                name: caller.name().to_owned(),
                declaration: caller.location,
                occurrence_count: 1,
                call_sites: Vec::new(),
                provenance: Provenance::RustAnalyzer,
            }],
            outgoing: Vec::new(),
            incoming_truncated: false,
            outgoing_truncated: false,
        },
        last_error: None,
    }))?;
    let mut update = engine.begin_update();
    update.graph_mut().add_file(
        RepoRelativePath::new("src/api/controller.rs")?,
        "// edited source captured in the new syntax revision\n",
    )?;
    update.set_freshness(Freshness::Fresh);
    let next = engine.publish(update)?;

    let envelope = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName(
            "service::payment_service::PaymentService::refund".to_owned(),
        )),
        ..CallersRequest::default()
    })?;
    assert_eq!(envelope.revision, next.revision());
    assert_eq!(envelope.provider_state, ProviderState::CatchingUp);
    assert_eq!(envelope.data.callers.len(), 1);
    assert_eq!(envelope.data.callers[0].precision, Precision::Syntax);
    assert_eq!(envelope.data.callers[0].provenance, Provenance::TreeSitter);
    Ok(())
}

#[test]
fn degraded_provider_preserves_useful_syntax_callers() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let revision = engine.snapshot().revision();
    engine.install_precise_provider(Arc::new(FixedProvider {
        result: PreciseQueryResult::unavailable_because(
            revision,
            ProviderState::Degraded,
            ProviderFallbackCause::ActivationFailed,
        ),
        last_error: Some("provider process stopped"),
    }))?;
    let envelope = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision,
        }),
        ..CallersRequest::default()
    })?;
    assert_eq!(envelope.provider_state, ProviderState::Degraded);
    assert_eq!(envelope.data.callers.len(), 1);
    assert_eq!(envelope.data.callers[0].symbol.id, ids.service_refund);
    assert_eq!(envelope.data.callers[0].precision, Precision::Syntax);
    assert_eq!(
        envelope
            .data
            .provider
            .as_ref()
            .and_then(|provider| provider.fallback_cause),
        Some(ProviderFallbackCause::ActivationFailed)
    );
    let status = engine.status(StatusRequest)?;
    assert_eq!(
        status.data.providers[0].last_error.as_deref(),
        Some("provider process stopped")
    );
    Ok(())
}

#[test]
fn precise_result_is_discarded_if_workspace_advances_during_provider_query()
-> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let engine = Arc::new(engine);
    let revision = engine.snapshot().revision();
    let caller = engine
        .snapshot()
        .graph()
        .symbol(ids.service_refund)
        .ok_or("service symbol missing")?
        .clone();
    engine.install_precise_provider(Arc::new(RevisionAdvancingProvider {
        engine: Arc::downgrade(&engine),
        result: PreciseQueryResult {
            revision,
            state: ProviderState::Ready,
            fallback_cause: None,
            incoming: vec![PreciseRelation {
                name: caller.name().to_owned(),
                declaration: caller.location,
                occurrence_count: 1,
                call_sites: Vec::new(),
                provenance: Provenance::RustAnalyzer,
            }],
            outgoing: Vec::new(),
            incoming_truncated: false,
            outgoing_truncated: false,
        },
    }))?;

    let envelope = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision,
        }),
        ..CallersRequest::default()
    })?;
    assert_eq!(envelope.provider_state, ProviderState::CatchingUp);
    assert_eq!(engine.snapshot().revision(), revision.next());
    assert_eq!(envelope.data.callers.len(), 1);
    assert_eq!(envelope.data.callers[0].precision, Precision::Syntax);
    assert!(
        envelope
            .data
            .provider
            .as_ref()
            .is_some_and(|provider| provider.fallback_used && provider.fallback_reason.is_some())
    );
    Ok(())
}

#[test]
fn precise_result_is_discarded_if_post_provider_freshness_advances() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let engine = Arc::new(engine);
    let revision = engine.snapshot().revision();
    let caller = engine
        .snapshot()
        .graph()
        .symbol(ids.service_refund)
        .ok_or("service symbol missing")?
        .clone();
    engine.install_precise_provider(Arc::new(FixedProvider {
        result: PreciseQueryResult {
            revision,
            state: ProviderState::Ready,
            fallback_cause: None,
            incoming: vec![PreciseRelation {
                name: caller.name().to_owned(),
                declaration: caller.location,
                occurrence_count: 1,
                call_sites: Vec::new(),
                provenance: Provenance::RustAnalyzer,
            }],
            outgoing: Vec::new(),
            incoming_truncated: false,
            outgoing_truncated: false,
        },
        last_error: None,
    }))?;
    engine.install_freshness_barrier(Arc::new(AdvanceAfterProviderBarrier {
        engine: Arc::downgrade(&engine),
        calls: AtomicUsize::new(0),
    }))?;

    let envelope = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision,
        }),
        ..CallersRequest::default()
    })?;
    assert_eq!(engine.snapshot().revision(), revision.next());
    assert_eq!(envelope.revision, revision);
    assert_eq!(envelope.provider_state, ProviderState::CatchingUp);
    assert_eq!(envelope.data.callers.len(), 1);
    assert_eq!(envelope.data.callers[0].precision, Precision::Syntax);
    Ok(())
}

#[test]
fn post_provider_freshness_failure_keeps_syntax_fallback() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let revision = engine.snapshot().revision();
    let caller = engine
        .snapshot()
        .graph()
        .symbol(ids.service_refund)
        .ok_or("service symbol missing")?
        .clone();
    engine.install_precise_provider(Arc::new(FixedProvider {
        result: PreciseQueryResult {
            revision,
            state: ProviderState::Ready,
            fallback_cause: None,
            incoming: vec![PreciseRelation {
                name: caller.name().to_owned(),
                declaration: caller.location,
                occurrence_count: 1,
                call_sites: Vec::new(),
                provenance: Provenance::RustAnalyzer,
            }],
            outgoing: Vec::new(),
            incoming_truncated: false,
            outgoing_truncated: false,
        },
        last_error: None,
    }))?;
    engine.install_freshness_barrier(Arc::new(FailAfterProviderBarrier::default()))?;

    let envelope = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision,
        }),
        ..CallersRequest::default()
    })?;
    assert_eq!(envelope.revision, revision);
    assert_eq!(envelope.provider_state, ProviderState::CatchingUp);
    assert_eq!(envelope.data.callers.len(), 1);
    assert_eq!(envelope.data.callers[0].precision, Precision::Syntax);
    Ok(())
}

#[test]
fn allow_stale_uses_syntax_without_waiting_for_precise_provider() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let calls = Arc::new(AtomicUsize::new(0));
    engine.install_precise_provider(Arc::new(CountingRustProvider {
        calls: calls.clone(),
    }))?;

    let envelope = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision: engine.snapshot().revision(),
        }),
        freshness: FreshnessRequirement::AllowStale,
        ..CallersRequest::default()
    })?;
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(envelope.provider_state, ProviderState::CatchingUp);
    assert!(
        envelope
            .data
            .provider
            .as_ref()
            .and_then(|provider| provider.fallback_reason.as_deref())
            .is_some_and(|reason| reason.contains("allow_stale"))
    );
    assert_eq!(envelope.data.callers.len(), 1);
    assert_eq!(envelope.data.callers[0].precision, Precision::Syntax);
    Ok(())
}

#[test]
fn rust_provider_is_not_invoked_for_php_symbols() -> Result<(), Box<dyn Error>> {
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = chakra_engine::WorkspaceEngine::new(identity);
    let path = RepoRelativePath::new("src/PaymentService.php")?;
    let position = TextPosition::new(1, 1)?;
    let mut graph = SymbolGraph::new();
    graph.add_file(path.clone(), "<?php function refund(): void {}\n")?;
    graph.add_symbol(
        SymbolKey {
            language: Language::Php,
            qualified_name: "refund".to_owned(),
            container: None,
            kind: SymbolKind::Function,
            path: path.clone(),
        },
        SourceRange::new(path, position, TextPosition::new(1, 31)?)?,
        Some("function refund(): void".to_owned()),
        Provenance::TreeSitter,
        Precision::Syntax,
    )?;
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let calls = Arc::new(AtomicUsize::new(0));
    engine.install_precise_provider(Arc::new(CountingRustProvider {
        calls: calls.clone(),
    }))?;
    let context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("refund".to_owned())),
        ..ContextRequest::default()
    })?;
    assert_eq!(context.data.symbol.language, Language::Php);
    assert_eq!(context.data.symbol.precision, Precision::Syntax);
    assert_eq!(context.provider_state, ProviderState::NotConfigured);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    Ok(())
}

#[test]
fn context_combines_bounded_relations() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let revision = engine.snapshot().revision();
    let envelope = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.service_refund,
            revision,
        }),
        ..ContextRequest::default()
    })?;
    let data = &envelope.data;
    assert_eq!(data.symbol.id, ids.service_refund);
    assert_eq!(data.symbol.kind, SymbolKind::Method);
    assert!(data.symbol.signature.is_some());
    assert_eq!(data.callers.len(), 1);
    assert_eq!(data.callers[0].symbol.id, ids.controller_refund);
    assert_eq!(data.callees.len(), 1);
    assert_eq!(data.callees[0].symbol.id, ids.provider_refund);
    assert_eq!(data.tests.len(), 2);
    let test_ids: Vec<_> = data.tests.iter().map(|t| t.symbol.id).collect();
    assert!(test_ids.contains(&ids.test_delegates));
    assert!(test_ids.contains(&ids.test_rejects_zero));
    assert!(data.implementations.is_empty());
    let files: Vec<&str> = data.related_files.iter().map(|f| f.as_str()).collect();
    assert_eq!(
        files,
        vec![
            "src/api/controller.rs",
            "src/provider/mod.rs",
            "src/service/payment_service.rs"
        ]
    );
    assert!(!envelope.truncated);
    Ok(())
}

#[test]
fn context_preserves_direction_and_provenance_for_framework_relations() -> Result<(), Box<dyn Error>>
{
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = chakra_engine::WorkspaceEngine::new(identity);
    let path = RepoRelativePath::new("app/Policy.php")?;
    let mut graph = SymbolGraph::new();
    graph.add_file(path.clone(), "<?php\n")?;
    let location = SourceRange::new(
        path.clone(),
        TextPosition::new(1, 1)?,
        TextPosition::new(1, 6)?,
    )?;
    let model = graph.add_symbol(
        SymbolKey {
            language: Language::Php,
            qualified_name: "App::Models::User".to_owned(),
            container: Some("App::Models".to_owned()),
            kind: SymbolKind::Class,
            path: path.clone(),
        },
        location.clone(),
        None,
        Provenance::TreeSitter,
        Precision::Syntax,
    )?;
    let policy = graph.add_symbol(
        SymbolKey {
            language: Language::Php,
            qualified_name: "App::Policies::UserPolicy".to_owned(),
            container: Some("App::Policies".to_owned()),
            kind: SymbolKind::Class,
            path,
        },
        location.clone(),
        None,
        Provenance::TreeSitter,
        Precision::Syntax,
    )?;
    graph.add_edge(
        EdgeKind::AuthorizesWith,
        model,
        policy,
        Provenance::Heuristic,
        Precision::Heuristic,
        Some(location),
    )?;
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let model_context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("App::Models::User".to_owned())),
        ..ContextRequest::default()
    })?;
    assert_eq!(model_context.data.related_relations.len(), 1);
    let relation = &model_context.data.related_relations[0];
    assert_eq!(
        relation.direction,
        chakra_domain::query::RelationDirection::Outgoing
    );
    assert_eq!(relation.relation.edge_kind, EdgeKind::AuthorizesWith);
    assert_eq!(relation.relation.symbol.id, policy);
    assert_eq!(relation.relation.provenance, Provenance::Heuristic);
    assert_eq!(relation.relation.precision, Precision::Heuristic);

    let policy_context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("App::Policies::UserPolicy".to_owned())),
        ..ContextRequest::default()
    })?;
    assert_eq!(
        policy_context.data.related_relations[0].direction,
        chakra_domain::query::RelationDirection::Incoming
    );
    assert_eq!(
        policy_context.data.related_relations[0].relation.symbol.id,
        model
    );
    Ok(())
}

#[test]
fn trait_method_context_shows_implementations() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let revision = engine.snapshot().revision();
    let envelope = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision,
        }),
        ..ContextRequest::default()
    })?;
    assert_eq!(envelope.data.implementations.len(), 1);
    assert_eq!(
        envelope.data.implementations[0].symbol.id,
        ids.stripe_refund
    );
    Ok(())
}

#[test]
fn struct_and_trait_kinds_are_exposed() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let kinds_of = |query: &str| -> Result<
        Vec<(chakra_domain::symbol::EntityId, SymbolKind)>,
        Box<dyn Error>,
    > {
        let found = engine.symbol_search(SymbolSearchRequest {
            query: query.to_owned(),
            ..SymbolSearchRequest::default()
        })?;
        Ok(found
            .data
            .candidates
            .iter()
            .map(|c| (c.id, c.kind))
            .collect())
    };
    let payments = kinds_of("payment")?;
    assert!(payments.contains(&(ids.controller_struct, SymbolKind::Struct)));
    assert!(payments.contains(&(ids.service_struct, SymbolKind::Struct)));
    let providers = kinds_of("provider")?;
    assert!(providers.contains(&(ids.provider_trait, SymbolKind::Trait)));
    assert!(providers.contains(&(ids.stripe_struct, SymbolKind::Struct)));
    Ok(())
}

#[test]
fn text_search_is_empty_without_captured_source_and_diff_requires_an_adapter()
-> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    let search = engine.search(SearchRequest {
        query: "refund".to_owned(),
        ..SearchRequest::default()
    })?;
    assert!(search.data.matches.is_empty());
    assert!(!search.truncated);
    let diff = engine.diff_context(DiffContextRequest::default());
    assert!(matches!(diff, Err(QueryError::DiffUnavailable(_))));
    Ok(())
}

#[test]
fn missing_and_unknown_symbol_refs_are_typed_errors() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    let revision = engine.snapshot().revision();
    let missing = engine.context(ContextRequest::default());
    assert!(matches!(missing, Err(QueryError::MissingSymbolRef)));

    let unknown = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: chakra_domain::symbol::EntityId(9999),
            revision,
        }),
        ..CallersRequest::default()
    });
    assert!(matches!(unknown, Err(QueryError::SymbolNotFound(_))));

    let absent = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName("does_not_exist".to_owned())),
        ..CallersRequest::default()
    });
    assert!(matches!(absent, Err(QueryError::SymbolNotFound(_))));
    Ok(())
}

#[test]
fn entity_ids_are_scoped_to_their_revision() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let stale_revision = engine.snapshot().revision();

    // Any newer publication makes old ids unresolvable by value.
    engine.publish(engine.begin_update())?;

    let result = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision: stale_revision,
        }),
        ..CallersRequest::default()
    });
    let expected = QueryError::StaleSymbolRef {
        reference_revision: stale_revision,
        current_revision: Revision(2),
    };
    assert_eq!(result.err(), Some(expected));

    // Publish a graph whose arena order differs, so the old numeric index now
    // denotes a DIFFERENT symbol — the exact hazard revision scoping exists
    // for. The scenario graph is republished in reverse declaration order.
    let (scenario, _) = scenario_graph()?;
    let count = scenario.symbol_count();
    let mut reversed = SymbolGraph::new();
    for symbol in scenario.symbols().iter().rev() {
        reversed.add_symbol(
            symbol.key.clone(),
            symbol.location.clone(),
            symbol.signature.clone(),
            symbol.provenance,
            symbol.precision,
        )?;
    }
    let remap =
        |id: chakra_domain::symbol::EntityId| chakra_domain::symbol::EntityId(count - 1 - id.0);
    for symbol in scenario.symbols() {
        for edge in scenario.outgoing_edges(symbol.id) {
            reversed.add_edge(
                edge.kind,
                remap(edge.from),
                remap(edge.to),
                edge.provenance,
                edge.precision,
                edge.location.clone(),
            )?;
        }
    }
    let mut update = engine.begin_update();
    update.replace_graph(reversed);
    // Replacing the graph revoked freshness; this update stands in for a
    // completed reconciliation, so it re-claims `Fresh` explicitly.
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    let snapshot = engine.snapshot();
    let current_revision = snapshot.revision();
    let graph = snapshot.graph();

    // The stale index now resolves to another symbol entirely…
    let hijacked = graph
        .symbol(ids.provider_refund)
        .ok_or("remapped graph lost the old index")?;
    assert_ne!(
        hijacked.key.qualified_name,
        "provider::PaymentProvider::refund"
    );

    // …so the client re-resolves by name against the current revision and
    // gets a fresh, correct id.
    let matches = graph.resolve_name("provider::PaymentProvider::refund");
    let fresh_id = *matches
        .first()
        .ok_or("provider::PaymentProvider::refund missing after remap")?;
    assert_ne!(fresh_id, ids.provider_refund);

    let resolved = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: fresh_id,
            revision: current_revision,
        }),
        ..CallersRequest::default()
    })?;
    assert_eq!(
        resolved.data.target.qualified_name,
        "provider::PaymentProvider::refund"
    );
    assert_eq!(resolved.data.callers.len(), 1);
    Ok(())
}

#[test]
fn unindexed_engine_reports_initializing_and_not_fresh() -> Result<(), Box<dyn Error>> {
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = chakra_engine::WorkspaceEngine::new(identity);
    let envelope = engine.status(StatusRequest)?;
    assert_eq!(envelope.status, WorkspaceStatus::Initializing);
    assert_eq!(envelope.freshness, Freshness::Stale);
    assert_eq!(envelope.data.counts.symbols, 0);
    Ok(())
}
