//! Opt-in public large-workspace acceptance harness for indexing budgets.

use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use chakra_domain::indexing::IndexPhase;
use chakra_domain::query::{QueryService, RepoMapRequest, SymbolSearchRequest};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_engine::WorkspaceEngine;
use chakra_language::index_repository;

#[test]
#[ignore = "set CHAKRA_LARGE_REPOSITORY to an external Git worktree"]
fn large_repository_stays_within_published_budgets() -> Result<(), Box<dyn Error>> {
    let repository = PathBuf::from(
        std::env::var_os("CHAKRA_LARGE_REPOSITORY")
            .ok_or("CHAKRA_LARGE_REPOSITORY must name an external Git worktree")?,
    );
    let report = index_repository(&repository)?;
    let status = &report.metrics.indexing;

    assert!(status.coverage.indexed_files <= status.budgets.max_files);
    assert!(status.coverage.source_bytes <= status.budgets.max_workspace_source_bytes);
    assert!(report.graph.symbol_count() <= status.budgets.max_symbols);
    assert!(report.graph.edge_count() <= status.budgets.max_edges);
    assert!(report.graph.call_site_count() <= status.budgets.max_call_sites);
    for required in [
        IndexPhase::GitInventory,
        IndexPhase::SourceRead,
        IndexPhase::ParseExtraction,
        IndexPhase::SymbolCatalog,
        IndexPhase::Relationships,
        IndexPhase::GraphMaterialization,
        IndexPhase::LanguageComposition,
        IndexPhase::GraphValidation,
    ] {
        assert!(
            status.phases.iter().any(|phase| phase.phase == required),
            "missing phase measurement: {required:?}"
        );
    }
    report.graph.validate_consistency()?;
    eprintln!(
        "large_workspace_index: root={} elapsed_ms={} discovered_files={} indexed_files={} source_bytes={} symbols={}/{} edges={}/{} call_sites={}/{} degraded={} current_rss_bytes={:?} observed_phase_peak_rss_bytes={:?}",
        report.repository_root.display(),
        report.metrics.elapsed.as_millis(),
        status.coverage.discovered_files,
        status.coverage.indexed_files,
        status.coverage.source_bytes,
        status.coverage.retained_symbols,
        status.budgets.max_symbols,
        status.coverage.retained_edges,
        status.budgets.max_edges,
        status.coverage.retained_call_sites,
        status.budgets.max_call_sites,
        status.is_degraded(),
        status.memory.current_rss_bytes,
        status.memory.observed_phase_peak_rss_bytes,
    );
    for phase in &status.phases {
        eprintln!(
            "large_workspace_phase: phase={:?} language={:?} elapsed_us={} work_items={} bytes={}",
            phase.phase, phase.language, phase.elapsed_micros, phase.work_items, phase.bytes,
        );
    }
    let first_symbol = report
        .graph
        .symbols()
        .first()
        .map(|symbol| symbol.name().to_owned());
    let indexing = status.clone();
    let identity = chakra_git::resolve_workspace_identity(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(indexing);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    let publication_started = Instant::now();
    engine.publish(update)?;
    eprintln!(
        "large_workspace_phase: phase={:?} language=None elapsed_us={} work_items=1 bytes=0",
        IndexPhase::RevisionPublication,
        publication_started.elapsed().as_micros(),
    );
    let map = engine.repo_map(RepoMapRequest {
        limit: Some(1),
        ..RepoMapRequest::default()
    })?;
    assert_eq!(map.data.files.len(), 1);
    if let Some(first_symbol) = first_symbol {
        let symbols = engine.symbol_search(SymbolSearchRequest {
            query: first_symbol,
            limit: Some(1),
            ..SymbolSearchRequest::default()
        })?;
        assert_eq!(symbols.data.candidates.len(), 1);
        assert_eq!(symbols.indexing.coverage, map.indexing.coverage);
    }
    Ok(())
}
