//! End-to-end syntax index coverage over the project Java fixture.

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
use chakra_language_java::{IndexMetrics, index_repository};
use tempfile::TempDir;

fn source_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("java")
        .join("controller-service-provider")
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            if entry.file_name() == "target" {
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
fn fixture_extracts_required_java_syntax_facts() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, metrics) = indexed_engine()?;
    let snapshot = engine.snapshot();
    let graph = snapshot.graph();

    assert_eq!(metrics.discovered_files, 8);
    assert_eq!(metrics.parsed_files, 8);
    assert_eq!(metrics.syntax_error_files, 0);
    assert_eq!(graph.file_count(), 8);
    graph.validate_consistency()?;

    let required = [
        ("chakra::payments::shared::Shared", SymbolKind::Class),
        (
            "chakra::payments::shared::Shared::recordEvent",
            SymbolKind::Method,
        ),
        (
            "chakra::payments::shared::Shared::sharedUniqueTarget",
            SymbolKind::Method,
        ),
        (
            "chakra::payments::provider::PaymentProvider",
            SymbolKind::Interface,
        ),
        (
            "chakra::payments::provider::PaymentProvider::PROVIDER_LABEL",
            SymbolKind::Field,
        ),
        (
            "chakra::payments::provider::PaymentProvider::refund",
            SymbolKind::Method,
        ),
        (
            "chakra::payments::provider::PaymentStatus",
            SymbolKind::Enum,
        ),
        (
            "chakra::payments::provider::PaymentStatus::PAID",
            SymbolKind::Constant,
        ),
        (
            "chakra::payments::provider::StripeProvider",
            SymbolKind::Class,
        ),
        (
            "chakra::payments::provider::StripeProvider::constructor",
            SymbolKind::Method,
        ),
        (
            "chakra::payments::provider::StripeProvider::apiKey",
            SymbolKind::Field,
        ),
        (
            "chakra::payments::provider::StripeProvider::refund",
            SymbolKind::Method,
        ),
        (
            "chakra::payments::service::PaymentService",
            SymbolKind::Class,
        ),
        (
            "chakra::payments::service::PaymentService::refund",
            SymbolKind::Method,
        ),
        (
            "chakra::payments::service::PaymentService::buildPaymentService",
            SymbolKind::Method,
        ),
        (
            "chakra::payments::api::PaymentController",
            SymbolKind::Class,
        ),
        (
            "chakra::payments::api::PaymentController::refund",
            SymbolKind::Method,
        ),
        ("chakra::payments::view::Panel", SymbolKind::Class),
        (
            "chakra::payments::PaymentFlowTest::refund_delegates_to_provider",
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
        symbol.key.language == Language::Java
            && symbol.provenance == Provenance::TreeSitter
            && symbol.precision == Precision::Syntax
    }));

    // Import facts: single-type imports and the `import static` member
    // import are recorded with syntax provenance.
    let imports: Vec<_> = graph
        .symbols()
        .iter()
        .filter(|symbol| symbol.key.kind == SymbolKind::Import)
        .collect();
    assert!(imports.len() >= 5);
    assert!(
        imports.iter().any(|symbol| symbol
            .key
            .qualified_name
            .contains("import static chakra.payments.shared.Shared.recordEvent")),
        "static import fact missing: {imports:?}"
    );

    // Byte-accurate ranges: `sharedUniqueTarget` spans lines 15-17 and its
    // declaration node starts at column 5 (inside the class body).
    let target = graph
        .symbols()
        .iter()
        .find(|symbol| {
            symbol.key.qualified_name == "chakra::payments::shared::Shared::sharedUniqueTarget"
        })
        .ok_or("shared target missing")?;
    assert_eq!(target.location.start().line(), 15);
    assert_eq!(target.location.start().column(), 5);
    assert_eq!(target.location.end().line(), 17);

    // The `implements` relation resolves through the same-package import.
    let stripe = graph.resolve_name("chakra::payments::provider::StripeProvider");
    let provider = graph.resolve_name("chakra::payments::provider::PaymentProvider");
    assert_eq!(stripe.len(), 1);
    assert_eq!(provider.len(), 1);
    assert!(graph.outgoing_edges(stripe[0]).iter().any(|edge| {
        edge.kind == EdgeKind::Implements
            && edge.to == provider[0]
            && edge.provenance == Provenance::TreeSitter
            && edge.precision == Precision::Heuristic
    }));

    // Import-alias, static-import, and constructor calls resolve; the
    // receiver-typed `this.provider.refund(...)` call stays honestly
    // unresolved, never guessed.
    let call_sites: Vec<_> = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.call_sites_from(symbol.id))
        .collect();
    let aliased = call_sites
        .iter()
        .find(|call| {
            call.name == "recordEvent" && call.receiver_hint.as_deref() == Some("recordEvent")
        })
        .ok_or("static-import call missing")?;
    assert_eq!(
        aliased.qualifier.as_deref(),
        Some("chakra::payments::shared::Shared")
    );
    assert!(matches!(
        aliased.resolution,
        CallResolution::Resolved { .. }
    ));
    let type_qualified = call_sites
        .iter()
        .find(|call| {
            call.name == "sharedUniqueTarget" && call.receiver_hint.as_deref() == Some("Shared")
        })
        .ok_or("type-qualified static call missing")?;
    assert!(matches!(
        type_qualified.resolution,
        CallResolution::Resolved { .. }
    ));
    let constructor = call_sites
        .iter()
        .find(|call| {
            call.name == "constructor" && call.qualifier.as_deref() == Some("PaymentController")
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
fn fixture_roles_and_module_scopes_come_from_the_pom() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, _metrics) = indexed_engine()?;
    let graph = engine.snapshot().graph().clone();
    let coverage = graph.source_metadata_coverage();
    assert_eq!(coverage.total_files, 8);
    assert_eq!(coverage.maven_metadata_files, 8);
    let test_file = graph
        .file_metadata(&chakra_domain::location::RepoRelativePath::new(
            "src/test/java/chakra/payments/PaymentFlowTest.java",
        )?)
        .ok_or("test file metadata missing")?;
    assert_eq!(test_file.role, SourceRole::Test);
    assert_eq!(
        test_file.classification,
        SourceClassification::MavenMetadata
    );
    assert_eq!(
        test_file
            .package
            .as_ref()
            .map(|package| package.name.as_str()),
        Some("controller-service-provider")
    );
    let panel = graph
        .file_metadata(&chakra_domain::location::RepoRelativePath::new(
            "src/main/java/chakra/payments/view/Panel.java",
        )?)
        .ok_or("panel metadata missing")?;
    assert_eq!(panel.role, SourceRole::Production);
    Ok(())
}

#[test]
fn broken_java_keeps_valid_symbols_and_reports_actionable_diagnostics() -> Result<(), Box<dyn Error>>
{
    let repository = fixture_repository()?;
    fs::write(
        repository
            .path()
            .join("src/main/java/chakra/payments/view/Panel.java"),
        "package chakra.payments.view;\n\
         public class Panel {\n\
         \x20   public String retainedMarker() { return \"ok\"; }\n\
         \x20   public String broken() { int x = ; }\n\
         }\n",
    )?;

    let report = index_repository(repository.path())?;
    assert_eq!(report.metrics.syntax_error_files, 1);
    let diagnostics = report.graph.syntax_diagnostics(10);
    assert_eq!(diagnostics.files_with_diagnostics, 1);
    assert!(!diagnostics.diagnostics.is_empty());
    assert!(diagnostics.diagnostics.iter().all(|diagnostic| {
        diagnostic.language == Language::Java
            && diagnostic
                .range
                .file()
                .as_str()
                .ends_with("view/Panel.java")
            && diagnostic.cause == SyntaxDiagnosticCause::ParseRecovery
    }));
    assert!(report.graph.symbols().iter().any(|symbol| {
        symbol.key.qualified_name == "chakra::payments::view::Panel::retainedMarker"
    }));
    Ok(())
}

#[test]
fn ambiguous_names_never_resolve_silently() -> Result<(), Box<dyn Error>> {
    let repository = fixture_repository()?;
    fs::create_dir_all(repository.path().join("src/main/java/chakra/payments/a"))?;
    fs::write(
        repository
            .path()
            .join("src/main/java/chakra/payments/a/A.java"),
        "package chakra.payments.a;\n\
         public class A {\n\
         \x20   public static String collidingHelper() { return \"a\"; }\n\
         }\n",
    )?;
    fs::create_dir_all(repository.path().join("src/main/java/chakra/payments/b"))?;
    fs::write(
        repository
            .path()
            .join("src/main/java/chakra/payments/b/B.java"),
        "package chakra.payments.b;\n\
         public class B {\n\
         \x20   public static String collidingHelper() { return \"b\"; }\n\
         }\n",
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
    assert_eq!(
        found,
        [
            "chakra::payments::a::A::collidingHelper",
            "chakra::payments::b::B::collidingHelper"
        ]
    );
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
        symbol: Some(SymbolRef::ByName(
            "chakra::payments::shared::Shared::sharedUniqueTarget".to_owned(),
        )),
        ..CallersRequest::default()
    })?;
    assert_eq!(callers.data.callers.len(), 1);
    let caller = &callers.data.callers[0];
    assert_eq!(
        caller.symbol.qualified_name,
        "chakra::payments::service::PaymentService::refund"
    );
    assert_eq!(caller.provenance, Provenance::TreeSitter);
    assert_eq!(caller.precision, Precision::Heuristic);

    let context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName(
            "chakra::payments::service::PaymentService::refund".to_owned(),
        )),
        ..ContextRequest::default()
    })?;
    assert!(context.data.callees.iter().any(|callee| {
        callee.symbol.qualified_name == "chakra::payments::shared::Shared::sharedUniqueTarget"
    }));
    // The test constructs the controller: the test method is a test-kind
    // caller of the constructor, surfaced as a test relation in context.
    let constructor_context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName(
            "chakra::payments::api::PaymentController::constructor".to_owned(),
        )),
        ..ContextRequest::default()
    })?;
    assert!(
        constructor_context
            .data
            .tests
            .iter()
            .any(|test| test.symbol.qualified_name
                == "chakra::payments::PaymentFlowTest::refund_delegates_to_provider"),
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
    let controller = repository
        .path()
        .join("src/main/java/chakra/payments/api/PaymentController.java");
    let source = fs::read_to_string(&controller)?;
    fs::write(
        &controller,
        source.replace(
            "this.service.refund(amountCents);",
            "this.service.refund(amountCents + 0);",
        ),
    )?;

    let sources = chakra_language_java::scan_repository_sources(repository.path())?;
    let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
    assert_eq!(reconciled.metrics.reparsed_files, 1);
    assert_eq!(reconciled.metrics.modified_files, 1);
    let graph = reconciled.graph.ok_or("reconciled graph missing")?;
    graph.validate_consistency()?;
    Ok(())
}
