//! End-to-end syntax coverage over the C# project/solution fixture.

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chakra_domain::diagnostic::SyntaxDiagnosticCause;
use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{CallersRequest, ContextRequest, QueryService, SymbolRef};
use chakra_domain::source::{SourceClassification, SourceRole};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_domain::symbol::{CallResolution, EdgeKind, Language, SymbolKind};
use chakra_engine::WorkspaceEngine;
use chakra_language_csharp::{IndexMetrics, index_repository};
use tempfile::TempDir;

fn source_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("csharp")
        .join("controller-service-provider")
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
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
    Ok((repository, engine, metrics))
}

#[test]
fn fixture_extracts_csharp_types_members_tests_heritage_and_calls() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, metrics) = indexed_engine()?;
    let snapshot = engine.snapshot();
    let graph = snapshot.graph();

    assert_eq!(metrics.discovered_files, 6);
    assert_eq!(metrics.parsed_files, 6);
    assert_eq!(metrics.syntax_error_files, 0);
    graph.validate_consistency()?;

    let required = [
        ("Chakra::Payments::Shared::Shared", SymbolKind::Class),
        (
            "Chakra::Payments::Shared::Shared::SharedUniqueTarget",
            SymbolKind::Method,
        ),
        (
            "Chakra::Payments::Shared::Shared::NormalizePayment",
            SymbolKind::Method,
        ),
        (
            "Chakra::Payments::Provider::IPaymentProvider",
            SymbolKind::Interface,
        ),
        (
            "Chakra::Payments::Provider::IPaymentProvider::ProviderLabel",
            SymbolKind::Property,
        ),
        (
            "Chakra::Payments::Provider::PaymentStatus::Paid",
            SymbolKind::Constant,
        ),
        (
            "Chakra::Payments::Provider::StripeProvider::constructor",
            SymbolKind::Method,
        ),
        (
            "Chakra::Payments::Service::PaymentService::RefundAsync",
            SymbolKind::Method,
        ),
        (
            "Chakra::Payments::Api::PaymentController",
            SymbolKind::Class,
        ),
        (
            "Chakra::Payments::Tests::PaymentFlowTests::Refund_delegates_to_provider",
            SymbolKind::Test,
        ),
        (
            "Chakra::Payments::Tests::PaymentFlowTests::NUnit_relationship",
            SymbolKind::Test,
        ),
        (
            "Chakra::Payments::Tests::PaymentFlowTests::MSTest_relationship",
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
        symbol.key.language == Language::CSharp
            && symbol.provenance == Provenance::TreeSitter
            && symbol.precision == Precision::Syntax
    }));

    let stripe = graph.resolve_name("Chakra::Payments::Provider::StripeProvider");
    let provider = graph.resolve_name("Chakra::Payments::Provider::IPaymentProvider");
    assert_eq!(stripe.len(), 1);
    assert_eq!(provider.len(), 1);
    assert!(graph.outgoing_edges(stripe[0]).iter().any(|edge| {
        edge.kind == EdgeKind::Implements
            && edge.to == provider[0]
            && edge.precision == Precision::Heuristic
    }));

    let call_sites: Vec<_> = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.call_sites_from(symbol.id))
        .collect();
    let shared = call_sites
        .iter()
        .find(|call| call.name == "SharedUniqueTarget")
        .ok_or("shared call missing")?;
    assert!(matches!(shared.resolution, CallResolution::Resolved { .. }));
    let extension = call_sites
        .iter()
        .find(|call| call.name == "NormalizePayment")
        .ok_or("extension-method call missing")?;
    assert!(matches!(
        extension.resolution,
        CallResolution::Resolved { .. }
    ));
    let extension_symbol = graph.resolve_name("Chakra::Payments::Shared::Shared::NormalizePayment");
    assert_eq!(extension_symbol.len(), 1);
    assert!(
        graph
            .symbol(extension_symbol[0])
            .and_then(|symbol| symbol.signature.as_deref())
            .is_some_and(|signature| signature.contains("this string value"))
    );
    let receiver_calls: Vec<_> = call_sites
        .iter()
        .filter(|call| call.name == "RefundAsync" && call.receiver_hint.is_some())
        .collect();
    assert!(!receiver_calls.is_empty());
    assert!(
        receiver_calls
            .iter()
            .any(|call| !matches!(call.resolution, CallResolution::Resolved { .. }))
    );
    let shared_target = graph.resolve_name("Chakra::Payments::Shared::Shared::SharedUniqueTarget");
    assert_eq!(shared_target.len(), 1);
    for test_name in ["NUnit_relationship", "MSTest_relationship"] {
        let test = graph.resolve_name(&format!(
            "Chakra::Payments::Tests::PaymentFlowTests::{test_name}"
        ));
        assert_eq!(test.len(), 1);
        assert!(graph.outgoing_edges(test[0]).iter().any(|edge| {
            edge.kind == EdgeKind::Tests
                && edge.to == shared_target[0]
                && edge.precision == Precision::Heuristic
        }));
    }
    Ok(())
}

#[test]
fn fixture_roles_and_projects_come_from_csproj_boundaries() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, _metrics) = indexed_engine()?;
    let graph = engine.snapshot().graph().clone();
    let coverage = graph.source_metadata_coverage();
    assert_eq!(coverage.total_files, 6);
    assert_eq!(coverage.dotnet_project_metadata_files, 6);

    let test = graph
        .file_metadata(&chakra_domain::location::RepoRelativePath::new(
            "tests/Payments.Core.Tests/PaymentFlowTests.cs",
        )?)
        .ok_or("test metadata missing")?;
    assert_eq!(test.role, SourceRole::Test);
    assert_eq!(
        test.classification,
        SourceClassification::DotnetProjectMetadata
    );
    assert_eq!(
        test.package.as_ref().map(|package| package.name.as_str()),
        Some("Payments.Core.Tests")
    );

    let production = graph
        .file_metadata(&chakra_domain::location::RepoRelativePath::new(
            "src/Payments.Core/PaymentService.cs",
        )?)
        .ok_or("production metadata missing")?;
    assert_eq!(production.role, SourceRole::Production);
    assert_eq!(
        production
            .package
            .as_ref()
            .map(|package| package.name.as_str()),
        Some("Chakra.Payments.Core")
    );
    Ok(())
}

#[test]
fn malformed_csharp_keeps_valid_symbols_and_reports_diagnostics() -> Result<(), Box<dyn Error>> {
    let repository = fixture_repository()?;
    fs::write(
        repository
            .path()
            .join("src/Payments.Core/PaymentController.cs"),
        "namespace Chakra.Payments.Api;\n\
         public class PaymentController {\n\
         \x20   public string RetainedMarker() => \"ok\";\n\
         \x20   public string Broken( { return \"broken\"; }\n\
         }\n",
    )?;

    let report = index_repository(repository.path())?;
    assert_eq!(report.metrics.syntax_error_files, 1);
    let diagnostics = report.graph.syntax_diagnostics(10);
    assert_eq!(diagnostics.files_with_diagnostics, 1);
    assert!(!diagnostics.diagnostics.is_empty());
    assert!(diagnostics.diagnostics.iter().all(|diagnostic| {
        diagnostic.language == Language::CSharp
            && diagnostic
                .range
                .file()
                .as_str()
                .ends_with("PaymentController.cs")
            && diagnostic.cause == SyntaxDiagnosticCause::ParseRecovery
    }));
    assert!(report.graph.symbols().iter().any(|symbol| {
        symbol.key.qualified_name == "Chakra::Payments::Api::PaymentController::RetainedMarker"
    }));
    Ok(())
}

#[test]
fn callers_context_and_incremental_reconcile_work_at_syntax_precision() -> Result<(), Box<dyn Error>>
{
    let (repository, engine, _metrics) = indexed_engine()?;
    let callers = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName(
            "Chakra::Payments::Shared::Shared::SharedUniqueTarget".to_owned(),
        )),
        ..CallersRequest::default()
    })?;
    let caller_names: BTreeSet<_> = callers
        .data
        .callers
        .iter()
        .map(|caller| caller.symbol.qualified_name.as_str())
        .collect();
    assert_eq!(caller_names.len(), callers.data.callers.len());
    assert_eq!(
        caller_names,
        BTreeSet::from([
            "Chakra::Payments::Service::PaymentService::RefundAsync",
            "Chakra::Payments::Tests::PaymentFlowTests::MSTest_relationship",
            "Chakra::Payments::Tests::PaymentFlowTests::NUnit_relationship",
        ])
    );
    assert!(
        callers
            .data
            .callers
            .iter()
            .all(|caller| caller.precision == Precision::Heuristic)
    );

    let context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName(
            "Chakra::Payments::Service::PaymentService::RefundAsync".to_owned(),
        )),
        ..ContextRequest::default()
    })?;
    assert!(context.data.callees.iter().any(|callee| {
        callee.symbol.qualified_name == "Chakra::Payments::Shared::Shared::SharedUniqueTarget"
    }));

    let report = index_repository(repository.path())?;
    let service = repository
        .path()
        .join("src/Payments.Core/PaymentService.cs");
    let source = fs::read_to_string(&service)?;
    fs::write(
        &service,
        source.replace("amountCents);", "amountCents + 0);"),
    )?;
    let sources = chakra_language_csharp::scan_repository_sources(repository.path())?;
    let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
    assert_eq!(reconciled.metrics.reparsed_files, 1);
    assert_eq!(reconciled.metrics.modified_files, 1);
    let graph = reconciled.graph.ok_or("reconciled graph missing")?;
    graph.validate_consistency()?;
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let callers = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName(
            "Chakra::Payments::Shared::Shared::SharedUniqueTarget".to_owned(),
        )),
        ..CallersRequest::default()
    })?;
    let caller_names: BTreeSet<_> = callers
        .data
        .callers
        .iter()
        .map(|caller| caller.symbol.qualified_name.as_str())
        .collect();
    assert_eq!(caller_names.len(), callers.data.callers.len());
    assert_eq!(
        caller_names,
        BTreeSet::from([
            "Chakra::Payments::Service::PaymentService::RefundAsync",
            "Chakra::Payments::Tests::PaymentFlowTests::MSTest_relationship",
            "Chakra::Payments::Tests::PaymentFlowTests::NUnit_relationship",
        ])
    );
    Ok(())
}
