//! Live indexing diagnostics over a real temporary Git worktree (issue #43).

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::indexing::{
    CacheHealth, FileInvalidationReason, FullReconciliationReason, MAX_FILE_INVALIDATION_RECORDS,
    ReconciliationKind, SYNTAX_FACT_CACHE_DISABLED_REASON,
};
use chakra_domain::location::RepoRelativePath;
use chakra_domain::query::{QueryService, StatusRequest};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_engine::WorkspaceEngine;
use chakra_language::{LiveIndex, index_repository, start_live_index};
use tempfile::TempDir;

fn write(root: &Path, path: &str, source: &str) -> Result<(), Box<dyn Error>> {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, source)?;
    Ok(())
}

fn repository() -> Result<TempDir, Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    write(
        repository.path(),
        "src/lib.rs",
        "pub mod one;\npub mod two;\n",
    )?;
    write(repository.path(), "src/one.rs", "pub fn alpha() {}\n")?;
    write(repository.path(), "src/two.rs", "pub fn beta() {}\n")?;
    Ok(repository)
}

fn start(repository: &TempDir) -> Result<(Arc<WorkspaceEngine>, LiveIndex), Box<dyn Error>> {
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = Arc::new(WorkspaceEngine::new(identity));
    let mut update = engine.begin_update();
    update.set_provider_inputs(report.provider_inputs.clone());
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Indexing);
    update.set_freshness(Freshness::Stale);
    engine.publish(update)?;
    let live = start_live_index(report.repository_root, report.syntax_index, engine.clone())?;
    Ok((engine, live))
}

fn invalidation_reason(
    live: &LiveIndex,
    path: &str,
) -> Result<Option<FileInvalidationReason>, Box<dyn Error>> {
    let path = RepoRelativePath::new(path)?;
    Ok(live
        .diagnostics()
        .recent_file_invalidations
        .iter()
        .rev()
        .find(|record| record.path == path)
        .map(|record| record.reason))
}

#[test]
fn cold_start_and_cache_health_are_reported_honestly() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (_engine, live) = start(&repository)?;

    let diagnostics = live.diagnostics();
    assert_eq!(
        diagnostics.cache,
        CacheHealth::Disabled {
            reason: SYNTAX_FACT_CACHE_DISABLED_REASON.to_owned(),
        }
    );
    assert!(
        diagnostics.full_reconciliation_reasons.cold_start >= 1,
        "the mandatory startup reconciliation must report its cold-start cause"
    );
    assert!(
        diagnostics
            .last_full_reconciliation_reasons
            .contains(&FullReconciliationReason::ColdStart)
    );
    assert_eq!(diagnostics.counters.full_reconciliations, 1);
    // The startup window must not be flooded with one Added record per file.
    assert!(diagnostics.recent_file_invalidations.is_empty());
    assert_eq!(diagnostics.file_invalidation_records, 0);
    assert!(diagnostics.queue.barrier_requests >= 1);
    assert_eq!(
        diagnostics.queue.requested_barrier_generation,
        diagnostics.queue.completed_barrier_generation
    );
    assert_eq!(diagnostics.queue.watcher_event_queue_capacity, 256);
    live.shutdown()?;
    Ok(())
}

#[test]
fn one_file_edit_reports_targeted_reconcile_and_content_reason() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start(&repository)?;

    write(
        repository.path(),
        "src/one.rs",
        "pub fn alpha() {}\npub fn alpha_two() {}\n",
    )?;
    engine.require_fresh()?;

    let diagnostics = live.diagnostics();
    assert_eq!(diagnostics.counters.one_file_edits, 1);
    assert!(diagnostics.counters.targeted_reconciliations >= 1);
    assert_eq!(diagnostics.counters.full_reconciliations, 1);
    assert_eq!(
        invalidation_reason(&live, "src/one.rs")?,
        Some(FileInvalidationReason::ContentChanged)
    );
    assert_eq!(diagnostics.file_invalidation_records, 1);
    assert_eq!(
        diagnostics.last_reconciliation_kind,
        ReconciliationKind::Targeted
    );
    live.shutdown()?;
    Ok(())
}

#[test]
fn create_and_delete_report_added_and_removed_reasons() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start(&repository)?;

    write(repository.path(), "src/three.rs", "pub fn gamma() {}\n")?;
    engine.require_fresh()?;
    assert_eq!(
        invalidation_reason(&live, "src/three.rs")?,
        Some(FileInvalidationReason::Added)
    );

    fs::remove_file(repository.path().join("src/three.rs"))?;
    engine.require_fresh()?;
    assert_eq!(
        invalidation_reason(&live, "src/three.rs")?,
        Some(FileInvalidationReason::Removed)
    );
    live.shutdown()?;
    Ok(())
}

#[test]
fn content_preserving_rewrite_reports_metadata_reason() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start(&repository)?;

    // Same bytes, fresh timestamps: the identity changes while the retained
    // source does not, which is exactly the distinguishable metadata case.
    write(repository.path(), "src/one.rs", "pub fn alpha() {}\n")?;
    engine.require_fresh()?;

    assert_eq!(
        invalidation_reason(&live, "src/one.rs")?,
        Some(FileInvalidationReason::MetadataChanged)
    );
    live.shutdown()?;
    Ok(())
}

#[test]
fn manifest_change_reports_metadata_reason_without_reparse() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "Cargo.toml",
        "[package]\nname = \"chakra-live-diagnostics\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    let (engine, live) = start(&repository)?;
    let metrics_before = live.metrics();

    write(
        repository.path(),
        "Cargo.toml",
        "[package]\nname = \"chakra-live-diagnostics\"\nversion = \"0.1.0\"\nedition = \"2024\"\n# reload\n",
    )?;
    engine.require_fresh()?;

    assert_eq!(
        invalidation_reason(&live, "Cargo.toml")?,
        Some(FileInvalidationReason::MetadataChanged)
    );
    assert_eq!(
        live.metrics().files_reparsed,
        metrics_before.files_reparsed,
        "a manifest-only change must not reparse unchanged sources"
    );
    live.shutdown()?;
    Ok(())
}

#[test]
fn warm_barrier_is_a_counted_noop_reconciliation() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start(&repository)?;

    engine.require_fresh()?;

    let diagnostics = live.diagnostics();
    assert!(diagnostics.counters.no_op_reconciliations >= 1);
    assert_eq!(
        diagnostics.last_reconciliation_kind,
        ReconciliationKind::Noop
    );
    assert_eq!(diagnostics.file_invalidation_records, 0);
    live.shutdown()?;
    Ok(())
}

#[test]
fn invalidation_window_is_bounded_and_counts_dropped_records() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start(&repository)?;

    let edits = (MAX_FILE_INVALIDATION_RECORDS + 4) as u64;
    for round in 0..edits {
        write(
            repository.path(),
            "src/one.rs",
            &format!("pub fn alpha() {{}}\npub fn round_{round}() {{}}\n"),
        )?;
        engine.require_fresh()?;
    }

    let diagnostics = live.diagnostics();
    assert_eq!(
        diagnostics.recent_file_invalidations.len(),
        MAX_FILE_INVALIDATION_RECORDS
    );
    assert_eq!(diagnostics.file_invalidation_records, edits);
    assert_eq!(diagnostics.counters.one_file_edits, edits);
    live.shutdown()?;
    Ok(())
}

#[test]
fn status_query_carries_diagnostics_and_engine_cold_build_count() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start(&repository)?;

    let envelope = engine.status(StatusRequest)?;
    let diagnostics = envelope
        .data
        .index_diagnostics
        .ok_or("live diagnostics must reach the status query")?;
    assert_eq!(
        diagnostics.cache,
        CacheHealth::Disabled {
            reason: SYNTAX_FACT_CACHE_DISABLED_REASON.to_owned(),
        }
    );
    assert!(
        diagnostics.counters.cold_builds >= 1,
        "the initial full index publication must be counted as a cold build"
    );
    assert!(diagnostics.full_reconciliation_reasons.cold_start >= 1);
    live.shutdown()?;
    Ok(())
}
