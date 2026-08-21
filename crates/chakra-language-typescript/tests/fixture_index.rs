//! End-to-end syntax index coverage over the project TypeScript fixture.

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
use chakra_language_typescript::{IndexMetrics, index_repository};
use tempfile::TempDir;

fn source_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("typescript")
        .join("controller-service-provider")
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            if entry.file_name() == "node_modules" {
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
fn fixture_extracts_required_typescript_syntax_facts() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, metrics) = indexed_engine()?;
    let snapshot = engine.snapshot();
    let graph = snapshot.graph();

    assert_eq!(metrics.discovered_files, 7);
    assert_eq!(metrics.parsed_files, 7);
    assert_eq!(metrics.syntax_error_files, 0);
    assert_eq!(graph.file_count(), 7);
    graph.validate_consistency()?;

    let required = [
        ("shared::sharedUniqueTarget", SymbolKind::Function),
        ("shared::recordEvent", SymbolKind::Function),
        ("provider::provider::PaymentProvider", SymbolKind::Interface),
        (
            "provider::provider::PaymentProvider::refund",
            SymbolKind::Method,
        ),
        (
            "provider::provider::PaymentProvider::label",
            SymbolKind::Property,
        ),
        ("provider::provider::AmountCents", SymbolKind::TypeAlias),
        ("provider::provider::PaymentStatus", SymbolKind::Enum),
        (
            "provider::provider::PaymentStatus::Open",
            SymbolKind::Constant,
        ),
        (
            "provider::stripeProvider::StripeProvider",
            SymbolKind::Class,
        ),
        (
            "provider::stripeProvider::StripeProvider::refund",
            SymbolKind::Method,
        ),
        (
            "provider::stripeProvider::StripeProvider::apiKey",
            SymbolKind::Property,
        ),
        ("service::paymentService::PaymentService", SymbolKind::Class),
        (
            "service::paymentService::buildPaymentService",
            SymbolKind::Function,
        ),
        ("api::controller::PaymentController", SymbolKind::Class),
        (
            "api::controller::PaymentController::refund",
            SymbolKind::Method,
        ),
        ("view::panel::Panel", SymbolKind::Function),
        (
            "tests::paymentFlow::refund delegates to provider",
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
        symbol.key.language == Language::TypeScript
            && symbol.provenance == Provenance::TreeSitter
            && symbol.precision == Precision::Syntax
    }));

    // Import facts, including the `recordEvent as auditEvent` alias.
    let imports: Vec<_> = graph
        .symbols()
        .iter()
        .filter(|symbol| symbol.key.kind == SymbolKind::Import)
        .collect();
    assert!(imports.len() >= 6);
    assert!(
        imports
            .iter()
            .any(|symbol| symbol.key.qualified_name.contains("auditEvent")),
        "aliased import fact missing: {imports:?}"
    );

    // Byte-accurate ranges: `sharedUniqueTarget` spans lines 3-5 and its
    // declaration node starts at column 8 (after `export `).
    let target = graph
        .symbols()
        .iter()
        .find(|symbol| symbol.key.qualified_name == "shared::sharedUniqueTarget")
        .ok_or("shared target missing")?;
    assert_eq!(target.location.start().line(), 3);
    assert_eq!(target.location.start().column(), 8);
    assert_eq!(target.location.end().line(), 5);

    // Implements relation resolved through the named import.
    let stripe = graph.resolve_name("provider::stripeProvider::StripeProvider");
    let provider = graph.resolve_name("provider::provider::PaymentProvider");
    assert_eq!(stripe.len(), 1);
    assert_eq!(provider.len(), 1);
    assert!(graph.outgoing_edges(stripe[0]).iter().any(|edge| {
        edge.kind == EdgeKind::Implements
            && edge.to == provider[0]
            && edge.provenance == Provenance::TreeSitter
            && edge.precision == Precision::Heuristic
    }));

    // Unique-name and import-alias calls resolve; the interface-typed
    // receiver call stays honestly ambiguous or unresolved, never guessed.
    let call_sites: Vec<_> = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.call_sites_from(symbol.id))
        .collect();
    let aliased = call_sites
        .iter()
        .find(|call| {
            call.name == "recordEvent" && call.receiver_hint.as_deref() == Some("auditEvent")
        })
        .ok_or("aliased call missing")?;
    assert!(matches!(
        aliased.resolution,
        CallResolution::Resolved { .. }
    ));
    let constructor = call_sites
        .iter()
        .find(|call| {
            call.name == "constructor" && call.qualifier.as_deref() == Some("StripeProvider")
        })
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
fn fixture_roles_and_package_scopes_come_from_package_json() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, _metrics) = indexed_engine()?;
    let graph = engine.snapshot().graph().clone();
    let coverage = graph.source_metadata_coverage();
    assert_eq!(coverage.total_files, 7);
    assert_eq!(coverage.package_json_metadata_files, 7);
    let test_file = graph
        .file_metadata(&chakra_domain::location::RepoRelativePath::new(
            "tests/paymentFlow.test.ts",
        )?)
        .ok_or("test file metadata missing")?;
    assert_eq!(test_file.role, SourceRole::Test);
    assert_eq!(
        test_file.classification,
        SourceClassification::PackageJsonMetadata
    );
    assert_eq!(
        test_file
            .package
            .as_ref()
            .map(|package| package.name.as_str()),
        Some("chakra/ts-fixture")
    );
    let panel = graph
        .file_metadata(&chakra_domain::location::RepoRelativePath::new(
            "src/view/panel.tsx",
        )?)
        .ok_or("panel metadata missing")?;
    assert_eq!(panel.role, SourceRole::Production);
    Ok(())
}

#[test]
fn broken_tsx_keeps_valid_symbols_and_reports_actionable_diagnostics() -> Result<(), Box<dyn Error>>
{
    let repository = fixture_repository()?;
    fs::write(
        repository.path().join("src/view/panel.tsx"),
        "export function Panel() { return <section>; }\nexport function retainedMarker(): void {}\nconst broken = <div>;\n",
    )?;

    let report = index_repository(repository.path())?;
    assert_eq!(report.metrics.syntax_error_files, 1);
    let diagnostics = report.graph.syntax_diagnostics(10);
    assert_eq!(diagnostics.files_with_diagnostics, 1);
    assert!(!diagnostics.diagnostics.is_empty());
    assert!(diagnostics.diagnostics.iter().all(|diagnostic| {
        diagnostic.language == Language::TypeScript
            && diagnostic.range.file().as_str() == "src/view/panel.tsx"
            && diagnostic.cause == SyntaxDiagnosticCause::ParseRecovery
    }));
    assert!(
        report
            .graph
            .symbols()
            .iter()
            .any(|symbol| symbol.key.qualified_name == "view::panel::retainedMarker")
    );
    Ok(())
}

#[test]
fn ambiguous_names_never_resolve_silently() -> Result<(), Box<dyn Error>> {
    let repository = fixture_repository()?;
    fs::write(
        repository.path().join("src/a.ts"),
        "export function collidingHelper(): string { return \"a\"; }\n",
    )?;
    fs::write(
        repository.path().join("src/b.ts"),
        "export function collidingHelper(): string { return \"b\"; }\n",
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
        query: "collidingHelper".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    let mut found: Vec<&str> = search
        .data
        .candidates
        .iter()
        .map(|candidate| candidate.qualified_name.as_str())
        .collect();
    found.sort_unstable();
    assert_eq!(found, ["a::collidingHelper", "b::collidingHelper"]);
    let ambiguous = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName("collidingHelper".to_owned())),
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
        symbol: Some(SymbolRef::ByName("shared::sharedUniqueTarget".to_owned())),
        ..CallersRequest::default()
    })?;
    assert_eq!(callers.data.callers.len(), 1);
    let caller = &callers.data.callers[0];
    assert_eq!(
        caller.symbol.qualified_name,
        "service::paymentService::PaymentService::refund"
    );
    assert_eq!(caller.provenance, Provenance::TreeSitter);
    assert_eq!(caller.precision, Precision::Heuristic);

    let context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName(
            "service::paymentService::PaymentService::refund".to_owned(),
        )),
        ..ContextRequest::default()
    })?;
    assert!(
        context
            .data
            .callees
            .iter()
            .any(|callee| callee.symbol.qualified_name == "shared::sharedUniqueTarget")
    );
    // The test file constructs the controller: the test block is a test-kind
    // caller of the constructor, surfaced as a test relation in context.
    let constructor_context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName(
            "api::controller::PaymentController::constructor".to_owned(),
        )),
        ..ContextRequest::default()
    })?;
    assert!(
        constructor_context
            .data
            .tests
            .iter()
            .any(|test| test.symbol.qualified_name
                == "tests::paymentFlow::refund delegates to provider"),
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
    let controller = repository.path().join("src/api/controller.ts");
    let source = fs::read_to_string(&controller)?;
    fs::write(
        &controller,
        source.replace(
            "this.service.refund(amountCents);",
            "this.service.refund(amountCents + 0);",
        ),
    )?;

    let sources = chakra_language_typescript::scan_repository_sources(repository.path())?;
    let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
    assert_eq!(reconciled.metrics.reparsed_files, 1);
    assert_eq!(reconciled.metrics.modified_files, 1);
    let graph = reconciled.graph.ok_or("reconciled graph missing")?;
    graph.validate_consistency()?;
    Ok(())
}
