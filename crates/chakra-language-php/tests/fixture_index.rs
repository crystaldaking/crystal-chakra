use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chakra_domain::diagnostic::{KnownSyntaxGrammarGap, SyntaxDiagnosticCause};
use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{QueryService, SymbolSearchRequest};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_domain::symbol::{CallResolution, EdgeKind, Language, ReceiverTypeSource, SymbolKind};
use chakra_engine::WorkspaceEngine;
use chakra_language_php::index_repository;
use tempfile::TempDir;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures/php/controller-service-provider")
}

fn laravel_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures/php/laravel-relationships")
}

fn diagnostics_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/php83_typed_default.php")
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

#[test]
fn realistic_php_fixture_exposes_bounded_syntax_intelligence() -> Result<(), Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    copy_tree(&fixture_root(), repository.path())?;

    let report = index_repository(repository.path())?;
    eprintln!(
        "php_fixture_index: files={}, symbols={}, edges={}, call_sites={}, ambiguous_call_sites={}, unresolved_call_sites={}, elapsed={:?}",
        report.metrics.parsed_files,
        report.metrics.symbols,
        report.metrics.edges,
        report.metrics.call_sites,
        report.metrics.ambiguous_call_sites,
        report.metrics.unresolved_call_sites,
        report.metrics.elapsed
    );
    assert_eq!(report.metrics.parsed_files, 4);
    assert_eq!(report.metrics.syntax_error_files, 0);
    assert!(report.graph.symbols().iter().all(|symbol| {
        symbol.key.language == Language::Php
            && symbol.provenance == Provenance::TreeSitter
            && symbol.precision == Precision::Syntax
    }));
    assert!(report.graph.symbols().iter().any(|symbol| {
        symbol.key.qualified_name == "ChakraFixture::Service::PaymentService"
            && symbol.key.kind == SymbolKind::Class
    }));
    assert!(report.graph.symbols().iter().any(|symbol| {
        symbol.key.qualified_name
            == "ChakraFixture::Tests::PaymentServiceTest::testRefundDelegatesToProvider"
            && symbol.key.kind == SymbolKind::Test
    }));

    let refund = report
        .graph
        .resolve_name("ChakraFixture::Service::PaymentService::refund");
    assert_eq!(refund.len(), 1);
    let callers: Vec<_> = report
        .graph
        .incoming_edges(refund[0])
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Calls)
        .collect();
    assert_eq!(callers.len(), 1);
    assert_eq!(callers[0].precision, Precision::Heuristic);
    let controller_call = report
        .graph
        .symbols()
        .iter()
        .flat_map(|symbol| report.graph.call_sites_from(symbol.id))
        .find(|call_site| {
            call_site.name == "refund"
                && call_site.receiver_type.as_deref()
                    == Some("ChakraFixture::Service::PaymentService")
        })
        .ok_or("typed PaymentService refund call missing")?;
    assert_eq!(
        controller_call.receiver_type_source,
        Some(ReceiverTypeSource::PromotedProperty)
    );
    assert!(matches!(
        controller_call.resolution,
        CallResolution::Resolved { .. }
    ));
    let unresolved_refunds = report
        .graph
        .symbols()
        .iter()
        .flat_map(|symbol| report.graph.call_sites_from(symbol.id))
        .filter(|call_site| {
            call_site.name == "refund"
                && call_site.resolution == CallResolution::Unresolved
                && call_site.provenance == Provenance::TreeSitter
                && call_site.precision == Precision::Syntax
        })
        .count();
    assert_eq!(unresolved_refunds, 1);

    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    let payment_service = engine.symbol_search(SymbolSearchRequest {
        query: "PaymentService".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(
        payment_service.data.candidates[0].qualified_name,
        "ChakraFixture::Service::PaymentService"
    );
    assert_eq!(payment_service.data.candidates[0].kind, SymbolKind::Class);
    assert!(
        payment_service
            .data
            .candidates
            .iter()
            .any(|candidate| candidate.kind == SymbolKind::Import)
    );
    Ok(())
}

fn only_edge(
    report: &chakra_language_php::IndexReport,
    from: &str,
    to: &str,
    kind: EdgeKind,
) -> Result<(), Box<dyn Error>> {
    let from_ids = report.graph.resolve_name(from);
    let to_ids = report.graph.resolve_name(to);
    if from_ids.len() != 1 || to_ids.len() != 1 {
        return Err(format!("could not resolve framework endpoints {from} -> {to}").into());
    }
    let edges: Vec<_> = report
        .graph
        .outgoing_edges(from_ids[0])
        .iter()
        .filter(|edge| edge.kind == kind && edge.to == to_ids[0])
        .collect();
    assert_eq!(edges.len(), 1, "missing {kind:?}: {from} -> {to}");
    assert_eq!(edges[0].provenance, Provenance::Heuristic);
    assert_eq!(edges[0].precision, Precision::Heuristic);
    assert!(edges[0].location.is_some());
    Ok(())
}

#[test]
fn laravel_fixture_exposes_typed_heuristic_relationships() -> Result<(), Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    copy_tree(&laravel_fixture_root(), repository.path())?;

    let report = index_repository(repository.path())?;
    eprintln!(
        "laravel_fixture_index: files={}, framework_symbols={}, framework_edges={}, elapsed={:?}",
        report.metrics.parsed_files,
        report.metrics.framework_symbols,
        report.metrics.framework_edges,
        report.metrics.elapsed
    );
    assert!(report.metrics.laravel_detected);
    assert_eq!(report.metrics.parsed_files, 11);
    assert_eq!(report.metrics.framework_truncated_files, 0);
    assert!(report.metrics.framework_symbols >= 5);
    assert!(report.metrics.framework_edges >= 11);

    only_edge(
        &report,
        "App::Contracts::Reporter",
        "App::Services::DatabaseReporter",
        EdgeKind::Binds,
    )?;
    only_edge(
        &report,
        "App::Providers::AppServiceProvider",
        "App::Contracts::Reporter",
        EdgeKind::DependsOn,
    )?;
    only_edge(
        &report,
        "App::Providers::AppServiceProvider::register",
        "App::Contracts::Reporter",
        EdgeKind::Resolves,
    )?;
    only_edge(
        &report,
        "App::Providers::AppServiceProvider::register",
        "App::Console::Commands::SendDigest::handle",
        EdgeKind::Registers,
    )?;
    only_edge(
        &report,
        "App::Listeners::SendWelcome::handle",
        "App::Events::UserCreated",
        EdgeKind::ListensTo,
    )?;
    only_edge(
        &report,
        "App::Models::User",
        "App::Policies::UserPolicy",
        EdgeKind::AuthorizesWith,
    )?;

    let show = report
        .graph
        .resolve_name("App::Http::Controllers::UserController::show");
    assert_eq!(show.len(), 1);
    assert!(report.graph.incoming_edges(show[0]).iter().any(|edge| {
        edge.kind == EdgeKind::RoutesTo
            && report
                .graph
                .symbol(edge.from)
                .is_some_and(|symbol| symbol.key.kind == SymbolKind::Configuration)
    }));
    let job = report.graph.resolve_name("App::Jobs::SyncReport::handle");
    assert_eq!(job.len(), 1);
    let job_kinds: Vec<_> = report
        .graph
        .incoming_edges(job[0])
        .iter()
        .map(|edge| edge.kind)
        .collect();
    assert!(job_kinds.contains(&EdgeKind::Dispatches));
    assert!(job_kinds.contains(&EdgeKind::Schedules));
    Ok(())
}

#[test]
fn laravel_enrichment_is_inactive_without_composer_signal() -> Result<(), Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    fs::create_dir_all(repository.path().join("routes"))?;
    fs::write(
        repository.path().join("routes/web.php"),
        "<?php use Illuminate\\Support\\Facades\\Route; Route::get('/x', Controller::class);",
    )?;
    let report = index_repository(repository.path())?;
    assert!(!report.metrics.laravel_detected);
    assert_eq!(report.metrics.framework_symbols, 0);
    assert_eq!(report.metrics.framework_edges, 0);
    assert!(
        report
            .graph
            .symbols()
            .iter()
            .all(|symbol| symbol.key.kind != SymbolKind::Configuration)
    );
    Ok(())
}

#[test]
fn one_laravel_edit_recomputes_only_affected_framework_file() -> Result<(), Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    copy_tree(&laravel_fixture_root(), repository.path())?;
    let report = index_repository(repository.path())?;
    let provider = repository
        .path()
        .join("app/Providers/AppServiceProvider.php");
    let source = fs::read_to_string(&provider)?;
    fs::write(
        provider,
        source.replace("app(Reporter::class);", "resolve(Reporter::class);"),
    )?;
    let sources = chakra_language_php::scan_repository_sources(repository.path())?;
    let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
    assert_eq!(reconciled.metrics.reparsed_files, 1);
    assert_eq!(reconciled.metrics.framework_files_reparsed, 1);
    assert_eq!(
        reconciled.metrics.framework_relationship_files_recomputed,
        1
    );
    assert_eq!(reconciled.metrics.framework_truncated_files, 0);
    assert!(reconciled.graph.is_some());
    Ok(())
}

#[test]
fn php_83_typed_default_gap_is_actionable_in_the_index() -> Result<(), Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    fs::create_dir(repository.path().join("src"))?;
    fs::copy(
        diagnostics_fixture(),
        repository.path().join("src/CheckoutPage.php"),
    )?;

    let report = index_repository(repository.path())?;
    assert_eq!(report.metrics.parsed_files, 1);
    assert_eq!(report.metrics.syntax_error_files, 1);
    let diagnostics = report.graph.syntax_diagnostics(10);
    assert_eq!(diagnostics.files_with_diagnostics, 1);
    assert_eq!(diagnostics.total_diagnostics, 1);
    assert_eq!(diagnostics.diagnostics.len(), 1);
    let diagnostic = &diagnostics.diagnostics[0];
    assert_eq!(diagnostic.range.file().as_str(), "src/CheckoutPage.php");
    assert_eq!(diagnostic.provenance, Provenance::TreeSitter);
    assert_eq!(diagnostic.precision, Precision::Syntax);
    assert_eq!(
        diagnostic.cause,
        SyntaxDiagnosticCause::KnownGrammarGap(
            KnownSyntaxGrammarGap::PhpTypedClassConstantNamedDefault,
        )
    );
    Ok(())
}
