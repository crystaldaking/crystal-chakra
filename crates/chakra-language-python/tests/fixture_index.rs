//! End-to-end syntax index coverage over the project Python fixture.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chakra_domain::diagnostic::SyntaxDiagnosticCause;
use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    CallersRequest, ContextRequest, QueryError, QueryService, StatusRequest, SymbolRef,
    SymbolSearchRequest,
};
use chakra_domain::source::{SourceClassification, SourceRole};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_domain::symbol::{CallResolution, EdgeKind, Language, SymbolKind};
use chakra_engine::WorkspaceEngine;
use chakra_language_python::{IndexMetrics, index_repository};
use tempfile::TempDir;

fn source_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("python")
        .join("controller-service-provider")
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            if entry.file_name() == "__pycache__" {
                continue;
            }
            fs::create_dir_all(&destination)?;
            copy_tree(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

fn fixture_repository() -> Result<TempDir, Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    copy_tree(&source_fixture_root(), repository.path())?;
    Ok(repository)
}

fn indexed_engine() -> Result<(TempDir, WorkspaceEngine, IndexMetrics), Box<dyn Error>> {
    let repository = fixture_repository()?;
    let report = index_repository(repository.path())?;
    let metrics = report.metrics;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    engine.install_diff_provider(std::sync::Arc::new(chakra_git::GitWorkspaceDiff))?;
    Ok((repository, engine, metrics))
}

#[test]
fn fixture_extracts_required_python_syntax_facts() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, metrics) = indexed_engine()?;
    let snapshot = engine.snapshot();
    let graph = snapshot.graph();

    assert_eq!(metrics.discovered_files, 7);
    assert_eq!(metrics.parsed_files, 7);
    assert_eq!(metrics.syntax_error_files, 0);
    assert_eq!(graph.file_count(), 7);
    graph.validate_consistency()?;

    let required = [
        ("shared::shared_unique_target", SymbolKind::Function),
        ("shared::record_event", SymbolKind::Function),
        ("provider::provider::PaymentProvider", SymbolKind::Class),
        (
            "provider::provider::PaymentProvider::refund",
            SymbolKind::Method,
        ),
        (
            "provider::provider::PaymentProvider::label",
            SymbolKind::Property,
        ),
        ("provider::provider::AmountCents", SymbolKind::Constant),
        ("provider::provider::PaymentStatus", SymbolKind::Class),
        (
            "provider::provider::PaymentStatus::Open",
            SymbolKind::Property,
        ),
        (
            "provider::stripe_provider::StripeProvider",
            SymbolKind::Class,
        ),
        (
            "provider::stripe_provider::StripeProvider::__init__",
            SymbolKind::Method,
        ),
        (
            "provider::stripe_provider::StripeProvider::refund",
            SymbolKind::Method,
        ),
        (
            "provider::stripe_provider::StripeProvider::label",
            SymbolKind::Property,
        ),
        (
            "service::payment_service::PaymentService",
            SymbolKind::Class,
        ),
        (
            "service::payment_service::build_payment_service",
            SymbolKind::Function,
        ),
        ("api::controller::PaymentController", SymbolKind::Class),
        (
            "api::controller::PaymentController::refund",
            SymbolKind::Method,
        ),
        ("view::panel::Panel", SymbolKind::Function),
        (
            "tests::test_payment_flow::test_refund_delegates_to_provider",
            SymbolKind::Test,
        ),
    ];
    for (qualified_name, kind) in required {
        assert!(
            graph.symbols().iter().any(|symbol| {
                symbol.key.qualified_name == qualified_name && symbol.key.kind == kind
            }),
            "missing {kind:?} {qualified_name}"
        );
    }
    assert!(graph.symbols().iter().all(|symbol| {
        symbol.key.language == Language::Python
            && symbol.provenance == Provenance::TreeSitter
            && symbol.precision == Precision::Syntax
    }));

    // Import facts, including the `record_event as audit_event` alias.
    let imports: Vec<_> = graph
        .symbols()
        .iter()
        .filter(|symbol| symbol.key.kind == SymbolKind::Import)
        .collect();
    assert!(imports.len() >= 6);
    assert!(
        imports
            .iter()
            .any(|symbol| symbol.key.qualified_name.contains("audit_event")),
        "aliased import fact missing: {imports:?}"
    );

    // Byte-accurate ranges: `shared_unique_target` spans lines 4-6 and its
    // declaration starts at column 1.
    let target = graph
        .symbols()
        .iter()
        .find(|symbol| symbol.key.qualified_name == "shared::shared_unique_target")
        .ok_or("shared target missing")?;
    assert_eq!(target.location.start().line(), 4);
    assert_eq!(target.location.start().column(), 1);
    assert_eq!(target.location.end().line(), 6);

    // Base-class relation resolved through the named import.
    let stripe = graph.resolve_name("provider::stripe_provider::StripeProvider");
    let provider = graph.resolve_name("provider::provider::PaymentProvider");
    assert_eq!(stripe.len(), 1);
    assert_eq!(provider.len(), 1);
    assert!(graph.outgoing_edges(stripe[0]).iter().any(|edge| {
        edge.kind == EdgeKind::Extends
            && edge.to == provider[0]
            && edge.provenance == Provenance::TreeSitter
            && edge.precision == Precision::Heuristic
    }));

    // Unique-name and import-alias calls resolve; the untyped instance
    // receiver call stays honestly ambiguous or unresolved, never guessed.
    let call_sites: Vec<_> = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.call_sites_from(symbol.id))
        .collect();
    let aliased = call_sites
        .iter()
        .find(|call| {
            call.name == "record_event" && call.receiver_hint.as_deref() == Some("audit_event")
        })
        .ok_or("aliased call missing")?;
    assert!(matches!(
        aliased.resolution,
        CallResolution::Resolved { .. }
    ));
    let constructor = call_sites
        .iter()
        .find(|call| call.name == "__init__" && call.qualifier.as_deref() == Some("StripeProvider"))
        .ok_or("constructor call missing")?;
    assert!(matches!(
        constructor.resolution,
        CallResolution::Resolved { .. }
    ));
    let refund_calls: Vec<_> = call_sites
        .iter()
        .filter(|call| call.name == "refund")
        .collect();
    assert!(!refund_calls.is_empty());
    assert!(
        refund_calls
            .iter()
            .all(|call| !matches!(call.resolution, CallResolution::Resolved { .. })),
        "same-named methods must not resolve through an untyped receiver"
    );
    Ok(())
}

#[test]
fn fixture_roles_and_package_scopes_come_from_pyproject_toml() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, _metrics) = indexed_engine()?;
    let graph = engine.snapshot().graph().clone();
    let coverage = graph.source_metadata_coverage();
    assert_eq!(coverage.total_files, 7);
    assert_eq!(coverage.pyproject_metadata_files, 7);
    let test_file = graph
        .file_metadata(&chakra_domain::location::RepoRelativePath::new(
            "tests/test_payment_flow.py",
        )?)
        .ok_or("test file metadata missing")?;
    assert_eq!(test_file.role, SourceRole::Test);
    assert_eq!(
        test_file.classification,
        SourceClassification::PyprojectMetadata
    );
    assert_eq!(
        test_file
            .package
            .as_ref()
            .map(|package| package.name.as_str()),
        Some("chakra-python-fixture")
    );
    let panel = graph
        .file_metadata(&chakra_domain::location::RepoRelativePath::new(
            "src/view/panel.py",
        )?)
        .ok_or("panel metadata missing")?;
    assert_eq!(panel.role, SourceRole::Production);
    Ok(())
}

#[test]
fn broken_python_keeps_valid_symbols_and_reports_actionable_diagnostics()
-> Result<(), Box<dyn Error>> {
    let repository = fixture_repository()?;
    fs::write(
        repository.path().join("src/view/panel.py"),
        "def Panel(props):\n    return \"<section>\"\n\n\ndef retained_marker():\n    pass\n\n\ndef broken(:\n",
    )?;

    let report = index_repository(repository.path())?;
    assert_eq!(report.metrics.syntax_error_files, 1);
    let diagnostics = report.graph.syntax_diagnostics(10);
    assert_eq!(diagnostics.files_with_diagnostics, 1);
    assert!(!diagnostics.diagnostics.is_empty());
    assert!(diagnostics.diagnostics.iter().all(|diagnostic| {
        diagnostic.language == Language::Python
            && diagnostic.range.file().as_str() == "src/view/panel.py"
            && diagnostic.cause == SyntaxDiagnosticCause::ParseRecovery
    }));
    assert!(
        report
            .graph
            .symbols()
            .iter()
            .any(|symbol| symbol.key.qualified_name == "view::panel::retained_marker")
    );
    Ok(())
}

#[test]
fn ambiguous_names_never_resolve_silently() -> Result<(), Box<dyn Error>> {
    let repository = fixture_repository()?;
    fs::write(
        repository.path().join("src/a.py"),
        "def colliding_helper():\n    return \"a\"\n",
    )?;
    fs::write(
        repository.path().join("src/b.py"),
        "def colliding_helper():\n    return \"b\"\n",
    )?;

    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let search = engine.symbol_search(SymbolSearchRequest {
        query: "colliding_helper".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    let mut found: Vec<&str> = search
        .data
        .candidates
        .iter()
        .map(|candidate| candidate.qualified_name.as_str())
        .collect();
    found.sort_unstable();
    assert_eq!(found, ["a::colliding_helper", "b::colliding_helper"]);
    let ambiguous = engine.callers(CallersRequest {
        source: Default::default(),
        symbol: Some(SymbolRef::ByName("colliding_helper".to_owned())),
        ..CallersRequest::default()
    });
    assert!(matches!(
        ambiguous,
        Err(QueryError::AmbiguousSymbol { candidates: 2, .. })
    ));
    Ok(())
}

#[test]
fn callers_and_context_answer_at_syntax_precision() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, _metrics) = indexed_engine()?;

    let callers = engine.callers(CallersRequest {
        source: Default::default(),
        symbol: Some(SymbolRef::ByName("shared::shared_unique_target".to_owned())),
        ..CallersRequest::default()
    })?;
    assert_eq!(callers.data.callers.len(), 1);
    let caller = &callers.data.callers[0];
    assert_eq!(
        caller.symbol.qualified_name,
        "service::payment_service::PaymentService::refund"
    );
    assert_eq!(caller.provenance, Provenance::TreeSitter);
    assert_eq!(caller.precision, Precision::Heuristic);

    let context = engine.context(ContextRequest {
        source: Default::default(),
        symbol: Some(SymbolRef::ByName(
            "service::payment_service::PaymentService::refund".to_owned(),
        )),
        ..ContextRequest::default()
    })?;
    assert!(
        context
            .data
            .callees
            .iter()
            .any(|callee| callee.symbol.qualified_name == "shared::shared_unique_target")
    );
    // The test constructs the controller: the test function is a test-kind
    // caller of the constructor, surfaced as a test relation in context.
    let constructor_context = engine.context(ContextRequest {
        source: Default::default(),
        symbol: Some(SymbolRef::ByName(
            "api::controller::PaymentController::__init__".to_owned(),
        )),
        ..ContextRequest::default()
    })?;
    assert!(
        constructor_context
            .data
            .tests
            .iter()
            .any(|test| test.symbol.qualified_name
                == "tests::test_payment_flow::test_refund_delegates_to_provider"),
        "test relation missing: {:?}",
        constructor_context.data.tests
    );

    let status = engine.status(StatusRequest)?;
    assert!(status.data.providers.is_empty());
    Ok(())
}

#[test]
fn reconcile_reparses_only_the_edited_file() -> Result<(), Box<dyn Error>> {
    let repository = fixture_repository()?;
    let report = index_repository(repository.path())?;
    let controller = repository.path().join("src/api/controller.py");
    let source = fs::read_to_string(&controller)?;
    fs::write(
        &controller,
        source.replace(
            "self.service.refund(amount_cents)",
            "self.service.refund(amount_cents + 0)",
        ),
    )?;

    let sources = chakra_language_python::scan_repository_sources(repository.path())?;
    let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
    assert_eq!(reconciled.metrics.reparsed_files, 1);
    assert_eq!(reconciled.metrics.modified_files, 1);
    let graph = reconciled.graph.ok_or("reconciled graph missing")?;
    graph.validate_consistency()?;
    Ok(())
}
