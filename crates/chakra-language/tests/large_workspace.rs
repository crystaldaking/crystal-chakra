//! Opt-in public large-workspace acceptance harness for indexing budgets.

use std::error::Error;
use std::fs;
use std::hash::Hasher;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use chakra_domain::indexing::{IndexBudgets, IndexCancellation, IndexPhase};
use chakra_domain::query::{QueryService, RepoMapRequest, SymbolSearchRequest};
use chakra_domain::state::{Freshness, FreshnessRequirement, WorkspaceStatus};
use chakra_engine::WorkspaceEngine;
use chakra_language::{
    IndexOptions, index_repository, index_repository_with_options, start_live_index,
};
use tempfile::TempDir;

fn graph_fingerprint(graph: &chakra_engine::SymbolGraph) -> u64 {
    let mut fingerprint = std::collections::hash_map::DefaultHasher::new();
    fingerprint.write(format!("{:?}", graph.file_summaries()).as_bytes());
    for symbol in graph.symbols() {
        fingerprint.write(format!("{symbol:?}").as_bytes());
        fingerprint.write(format!("{:?}", graph.outgoing_edges(symbol.id)).as_bytes());
        fingerprint.write(
            format!("{:?}", graph.call_sites_from(symbol.id).collect::<Vec<_>>()).as_bytes(),
        );
    }
    fingerprint.finish()
}

#[test]
#[ignore = "set CHAKRA_LARGE_REPOSITORY to an external Git worktree"]
fn large_repository_stays_within_published_budgets() -> Result<(), Box<dyn Error>> {
    let repository = PathBuf::from(
        std::env::var_os("CHAKRA_LARGE_REPOSITORY")
            .ok_or("CHAKRA_LARGE_REPOSITORY must name an external Git worktree")?,
    );
    let configured_workers = std::env::var("CHAKRA_INDEX_WORKERS")
        .ok()
        .map(|workers| workers.parse::<u64>())
        .transpose()?
        .unwrap_or(IndexBudgets::default().max_workers);
    let report = index_repository_with_options(
        &repository,
        IndexOptions::new(
            IndexBudgets {
                max_workers: configured_workers,
                ..IndexBudgets::default()
            },
            IndexCancellation::default(),
        )?,
    )?;
    let status = &report.metrics.indexing;

    assert!(status.coverage.indexed_files <= status.budgets.max_files);
    assert!(status.coverage.source_bytes <= status.budgets.max_workspace_source_bytes);
    assert!(report.graph.symbol_count() <= status.budgets.max_symbols);
    assert!(report.graph.edge_count() <= status.budgets.max_edges);
    assert!(report.graph.call_site_count() <= status.budgets.max_call_sites);
    assert!(status.scheduling.peak_active_workers <= status.scheduling.effective_worker_limit);
    assert!(status.scheduling.effective_worker_limit <= configured_workers);
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
    #[cfg(unix)]
    for phase in status.phases.iter().filter(|phase| phase.work_items > 0) {
        assert!(phase.cpu_micros.is_some(), "missing CPU time: {phase:?}");
        assert!(
            phase.cpu_utilization_per_mille.is_some(),
            "missing CPU utilization: {phase:?}"
        );
        assert!(
            phase.peak_rss_bytes.is_some(),
            "missing peak RSS: {phase:?}"
        );
    }
    report.graph.validate_consistency()?;
    eprintln!(
        "large_workspace_index: root={} elapsed_ms={} discovered_files={} indexed_files={} source_bytes={} symbols={}/{} edges={}/{} call_sites={}/{} degraded={} configured_workers={} available_parallelism={} effective_worker_limit={} peak_active_workers={} parallel_parse_files={} current_rss_bytes={:?} observed_phase_peak_rss_bytes={:?}",
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
        status.scheduling.configured_max_workers,
        status.scheduling.available_parallelism,
        status.scheduling.effective_worker_limit,
        status.scheduling.peak_active_workers,
        status.scheduling.parallel_parse_files,
        status.memory.current_rss_bytes,
        status.memory.observed_phase_peak_rss_bytes,
    );
    for phase in &status.phases {
        eprintln!(
            "large_workspace_phase: phase={:?} language={:?} elapsed_us={} cpu_us={:?} cpu_per_mille={:?} workers={} peak_active_workers={} queue_depth={} work_items={} bytes={} rss_bytes={:?} peak_rss_bytes={:?}",
            phase.phase,
            phase.language,
            phase.elapsed_micros,
            phase.cpu_micros,
            phase.cpu_utilization_per_mille,
            phase.effective_workers,
            phase.peak_active_workers,
            phase.peak_queue_depth,
            phase.work_items,
            phase.bytes,
            phase.rss_bytes,
            phase.peak_rss_bytes,
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

#[test]
#[ignore = "set CHAKRA_LARGE_REPOSITORY to an external Git worktree"]
fn large_repository_worker_matrix_is_deterministic() -> Result<(), Box<dyn Error>> {
    let repository = PathBuf::from(
        std::env::var_os("CHAKRA_LARGE_REPOSITORY")
            .ok_or("CHAKRA_LARGE_REPOSITORY must name an external Git worktree")?,
    );
    let available = std::thread::available_parallelism()
        .map(|value| value.get() as u64)
        .unwrap_or(1);
    let mut worker_counts = vec![1, 2, IndexBudgets::default().max_workers.min(available)];
    worker_counts.sort_unstable();
    worker_counts.dedup();
    let mut baseline = None;

    for workers in worker_counts {
        let report = index_repository_with_options(
            &repository,
            IndexOptions::new(
                IndexBudgets {
                    max_workers: workers,
                    ..IndexBudgets::default()
                },
                IndexCancellation::default(),
            )?,
        )?;
        let fingerprint = graph_fingerprint(&report.graph);
        let stable = (
            fingerprint,
            report.metrics.indexing.coverage.clone(),
            report.metrics.indexing.capabilities.clone(),
            report.metrics.indexing.degradations.clone(),
        );
        if let Some(expected) = baseline.as_ref() {
            assert_eq!(&stable, expected);
        } else {
            baseline = Some(stable);
        }
        eprintln!(
            "large_workspace_determinism: workers={} effective={} elapsed_ms={} fingerprint={fingerprint:016x}",
            workers,
            report.metrics.indexing.scheduling.effective_worker_limit,
            report.metrics.elapsed.as_millis(),
        );
    }
    Ok(())
}

#[test]
#[ignore = "set CHAKRA_LARGE_REPOSITORY to an external Git worktree"]
fn large_repository_one_file_revision_is_structural() -> Result<(), Box<dyn Error>> {
    let source = PathBuf::from(
        std::env::var_os("CHAKRA_LARGE_REPOSITORY")
            .ok_or("CHAKRA_LARGE_REPOSITORY must name an external Git worktree")?,
    );
    let temporary = TempDir::new()?;
    let checkout = temporary.path().join("repository");
    let clone = Command::new("git")
        .args([
            "-c",
            "advice.detachedHead=false",
            "clone",
            "--quiet",
            "--shared",
        ])
        .arg(&source)
        .arg(&checkout)
        .status()?;
    if !clone.success() {
        return Err("failed to create isolated benchmark clone".into());
    }

    let report = index_repository(&checkout)?;
    let target = report
        .syntax_index
        .paths()
        .into_iter()
        .next()
        .ok_or("large repository has no supported source file")?;
    let target_path = checkout.join(target.as_str());
    let original = fs::read_to_string(&target_path)?;
    let identity = chakra_git::resolve_workspace_identity(&report.repository_root)?;
    let engine = Arc::new(WorkspaceEngine::new(identity));
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Indexing);
    update.set_freshness(Freshness::Stale);
    engine.publish(update)?;
    let live = start_live_index(report.repository_root, report.syntax_index, engine.clone())?;
    let baseline = live.metrics();

    fs::write(
        &target_path,
        format!("{original}\n// chakra structural-publication measurement\n"),
    )?;
    let started = Instant::now();
    let response = engine.repo_map(RepoMapRequest::default())?;
    let elapsed = started.elapsed();
    let snapshot = engine.snapshot();
    let publication = snapshot.indexing().publication;
    let metrics = live.metrics();

    assert_eq!(response.freshness, Freshness::Fresh);
    assert!(publication.structurally_incremental);
    assert_eq!(publication.rebuilt_files, 1);
    assert_eq!(publication.copied_source_bytes, 0);
    assert_eq!(publication.copied_symbols, 0);
    assert_eq!(publication.copied_edges, 0);
    assert_eq!(publication.copied_call_sites, 0);
    assert_eq!(metrics.files_reparsed - baseline.files_reparsed, 1);
    eprintln!(
        "large_workspace_update: root={} target={} elapsed_us={} retained_files={} reused_files={} rebuilt_files={} retained_symbols={} reused_symbols={} rebuilt_symbols={} retained_edges={} reused_edges={} rebuilt_edges={} retained_call_sites={} reused_call_sites={} rebuilt_call_sites={} current_rss_bytes={:?} observed_phase_peak_rss_bytes={:?}",
        source.display(),
        target,
        elapsed.as_micros(),
        snapshot.graph().file_count(),
        publication.reused_files,
        publication.rebuilt_files,
        snapshot.graph().symbol_count(),
        publication.reused_symbols,
        publication.rebuilt_symbols,
        snapshot.graph().edge_count(),
        publication.reused_edges,
        publication.rebuilt_edges,
        snapshot.graph().call_site_count(),
        publication.reused_call_sites,
        publication.rebuilt_call_sites,
        snapshot.indexing().memory.current_rss_bytes,
        snapshot.indexing().memory.observed_phase_peak_rss_bytes,
    );

    live.shutdown()?;
    Ok(())
}

#[test]
#[ignore = "set CHAKRA_LARGE_REPOSITORY to an external Git worktree"]
fn large_repository_warmed_noop_freshness_is_bounded() -> Result<(), Box<dyn Error>> {
    const RUNS: u64 = 10;
    let repository = PathBuf::from(
        std::env::var_os("CHAKRA_LARGE_REPOSITORY")
            .ok_or("CHAKRA_LARGE_REPOSITORY must name an external Git worktree")?,
    );
    let report = index_repository(&repository)?;
    let query = report
        .graph
        .symbols()
        .first()
        .map(|symbol| symbol.name().to_owned())
        .ok_or("large repository has no indexed symbols")?;
    let identity = chakra_git::resolve_workspace_identity(&report.repository_root)?;
    let engine = Arc::new(WorkspaceEngine::new(identity));
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Indexing);
    update.set_freshness(Freshness::Stale);
    engine.publish(update)?;
    let live = start_live_index(report.repository_root, report.syntax_index, engine.clone())?;
    let baseline = live.metrics();
    let mut samples = Vec::with_capacity(RUNS as usize);

    for _ in 0..RUNS {
        let started = Instant::now();
        let response = engine.symbol_search(SymbolSearchRequest {
            query: query.clone(),
            limit: Some(1),
            freshness: FreshnessRequirement::RequireFresh,
            ..SymbolSearchRequest::default()
        })?;
        samples.push(started.elapsed());
        assert_eq!(response.freshness, Freshness::Fresh);
        assert!(!response.data.candidates.is_empty());
    }

    samples.sort();
    let metrics = live.metrics();
    assert_eq!(metrics.files_read - baseline.files_read, 0);
    assert_eq!(metrics.source_bytes_read - baseline.source_bytes_read, 0);
    assert_eq!(
        metrics.git_subprocesses - baseline.git_subprocesses,
        RUNS * 2
    );
    assert_eq!(
        metrics.no_op_reconciliations - baseline.no_op_reconciliations,
        RUNS
    );
    assert_eq!(
        metrics.full_reconciliations - baseline.full_reconciliations,
        0
    );
    eprintln!(
        "large_workspace_noop_fresh: root={} runs={} min_us={} median_us={} max_us={} files_inspected={} bytes_inspected={} metadata_files_inspected={} metadata_bytes_inspected={} git_subprocesses={} files_read={} bytes_read={}",
        repository.display(),
        RUNS,
        samples.first().map_or(0, |sample| sample.as_micros()),
        samples[samples.len() / 2].as_micros(),
        samples.last().map_or(0, |sample| sample.as_micros()),
        metrics.files_inspected - baseline.files_inspected,
        metrics.source_bytes_inspected - baseline.source_bytes_inspected,
        metrics.metadata_files_inspected - baseline.metadata_files_inspected,
        metrics.metadata_bytes_inspected - baseline.metadata_bytes_inspected,
        metrics.git_subprocesses - baseline.git_subprocesses,
        metrics.files_read - baseline.files_read,
        metrics.source_bytes_read - baseline.source_bytes_read,
    );
    live.shutdown()?;
    Ok(())
}
