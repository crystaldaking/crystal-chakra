//! End-to-end syntax coverage over the HCL/Terraform fixture.

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chakra_domain::diagnostic::SyntaxDiagnosticCause;
use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{CallersRequest, QueryService, SymbolRef};
use chakra_domain::source::{SourceClassification, SourceRole};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_domain::symbol::{CallResolution, Language, SymbolKind};
use chakra_engine::WorkspaceEngine;
use chakra_language_hcl::{IndexMetrics, index_repository};
use tempfile::TempDir;

fn source_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("hcl")
        .join("controller-service-provider")
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
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
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let metrics = report.metrics;
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    Ok((repository, engine, metrics))
}

#[test]
fn fixture_extracts_terraform_entities_imports_tests_and_references() -> Result<(), Box<dyn Error>>
{
    let (_repository, engine, metrics) = indexed_engine()?;
    let snapshot = engine.snapshot();
    let graph = snapshot.graph();
    assert_eq!(metrics.discovered_files, 8);
    assert_eq!(metrics.syntax_error_files, 0);
    graph.validate_consistency()?;

    for (qualified_name, kind) in [
        (
            "resource::null_resource::provider",
            SymbolKind::Configuration,
        ),
        (
            "resource::null_resource::service",
            SymbolKind::Configuration,
        ),
        ("module::shared", SymbolKind::Module),
        ("var::region", SymbolKind::Property),
        ("local::service_name", SymbolKind::Property),
        ("output::service_id", SymbolKind::Property),
        ("run::test_refund_flow", SymbolKind::Test),
    ] {
        assert!(
            graph.symbols().iter().any(|symbol| {
                symbol.key.qualified_name == qualified_name && symbol.key.kind == kind
            }),
            "missing {kind:?} {qualified_name}"
        );
    }
    assert!(graph.symbols().iter().all(|symbol| {
        symbol.key.language == Language::Hcl
            && symbol.provenance == Provenance::TreeSitter
            && symbol.precision == Precision::Syntax
    }));
    assert_eq!(
        graph
            .symbols()
            .iter()
            .filter(|symbol| symbol.key.kind == SymbolKind::Import)
            .count(),
        2
    );
    let configuration_calls: Vec<_> = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.call_sites_from(symbol.id))
        .filter(|call| call.target_kind == chakra_domain::symbol::CallTargetKind::Configuration)
        .collect();
    assert!(!configuration_calls.is_empty());
    assert!(configuration_calls.iter().all(|call| matches!(
        call.resolution,
        CallResolution::Resolved { .. }
    ) && call.precision == Precision::Syntax));
    Ok(())
}

#[test]
fn fixture_roles_and_projects_use_terraform_module_boundaries() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, _metrics) = indexed_engine()?;
    let graph = engine.snapshot().graph().clone();
    let coverage = graph.source_metadata_coverage();
    assert_eq!(coverage.total_files, 8);
    assert_eq!(coverage.terraform_module_metadata_files, 8);

    for (path, role, package) in [
        ("service.tf", SourceRole::Production, "repository"),
        (
            "tests/payment_flow.tftest.hcl",
            SourceRole::Test,
            "repository",
        ),
        ("vendor/external.tf", SourceRole::Vendor, "vendor"),
        (
            "generated/build_info.tf",
            SourceRole::Generated,
            "generated",
        ),
    ] {
        let metadata = graph
            .file_metadata(&chakra_domain::location::RepoRelativePath::new(path)?)
            .ok_or("source metadata missing")?;
        assert_eq!(metadata.role, role);
        assert_eq!(
            metadata.classification,
            SourceClassification::TerraformModuleMetadata
        );
        assert_eq!(
            metadata.package.as_ref().map(|scope| scope.name.as_str()),
            Some(package)
        );
    }
    Ok(())
}

#[test]
fn malformed_hcl_keeps_valid_symbols_and_reports_diagnostics() -> Result<(), Box<dyn Error>> {
    let repository = fixture_repository()?;
    fs::write(
        repository.path().join("service.tf"),
        "resource \"null_resource\" \"retained_marker\" {}\nresource \"broken\" \"x\" {\n",
    )?;
    let report = index_repository(repository.path())?;
    assert_eq!(report.metrics.syntax_error_files, 1);
    let diagnostics = report.graph.syntax_diagnostics(10);
    assert!(diagnostics.diagnostics.iter().all(|diagnostic| {
        diagnostic.language == Language::Hcl
            && diagnostic.cause == SyntaxDiagnosticCause::ParseRecovery
    }));
    assert!(
        report.graph.symbols().iter().any(|symbol| {
            symbol.key.qualified_name == "resource::null_resource::retained_marker"
        })
    );
    Ok(())
}

#[test]
fn callers_and_incremental_reconcile_use_hcl_reference_edges() -> Result<(), Box<dyn Error>> {
    let (repository, engine, _metrics) = indexed_engine()?;
    let callers = engine.callers(CallersRequest {
        source: Default::default(),
        symbol: Some(SymbolRef::ByName(
            "resource::null_resource::provider".to_owned(),
        )),
        ..CallersRequest::default()
    })?;
    let names: BTreeSet<_> = callers
        .data
        .callers
        .iter()
        .map(|caller| caller.symbol.qualified_name.as_str())
        .collect();
    assert_eq!(names, BTreeSet::from(["resource::null_resource::service"]));

    let report = index_repository(repository.path())?;
    let service = repository.path().join("service.tf");
    let source = fs::read_to_string(&service)?;
    fs::write(
        &service,
        source.replace("null_resource.provider.id", "null_resource.changed.id"),
    )?;
    let sources = chakra_language_hcl::scan_repository_sources(repository.path())?;
    let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
    assert_eq!(reconciled.metrics.reparsed_files, 1);
    assert_eq!(reconciled.metrics.modified_files, 1);
    reconciled
        .graph
        .ok_or("reconciled graph missing")?
        .validate_consistency()?;
    Ok(())
}
