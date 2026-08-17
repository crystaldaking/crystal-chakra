//! Deterministic live-update regressions over a real temporary Git worktree.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::query::{
    ContextRequest, QueryService, RepoMapRequest, SymbolRef, SymbolSearchRequest,
};
use chakra_domain::state::{Freshness, FreshnessRequirement, WorkspaceStatus};
use chakra_domain::symbol::{CallResolution, Language};
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
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Indexing);
    update.set_freshness(Freshness::Stale);
    engine.publish(update)?;
    let live = start_live_index(report.repository_root, report.syntax_index, engine.clone())?;
    Ok((engine, live))
}

fn symbols(
    engine: &WorkspaceEngine,
    query: &str,
) -> Result<Vec<String>, chakra_domain::query::QueryError> {
    let result = engine.symbol_search(SymbolSearchRequest {
        query: query.to_owned(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
    })?;
    assert_eq!(result.freshness, Freshness::Fresh);
    Ok(result
        .data
        .candidates
        .into_iter()
        .map(|symbol| symbol.qualified_name)
        .collect())
}

#[test]
fn immediate_fresh_read_is_atomic_and_reindexes_only_one_file() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start(&repository)?;
    let before_unchanged_query = live.metrics();
    assert_eq!(symbols(&engine, "one::alpha")?, ["one::alpha"]);
    let after_unchanged_query = live.metrics();
    assert_eq!(
        after_unchanged_query.files_reparsed, before_unchanged_query.files_reparsed,
        "a fresh proof must not reparse unchanged content"
    );
    let old_snapshot = engine.snapshot();
    let baseline = after_unchanged_query;

    write(
        repository.path(),
        "src/one.rs",
        "pub fn alpha_after_edit() {}\n",
    )?;
    let reindex_started = Instant::now();
    let found = symbols(&engine, "alpha_after_edit")?;
    let reindex_elapsed = reindex_started.elapsed();
    let current = engine.snapshot();
    let metrics = live.metrics();

    assert_eq!(found, ["one::alpha_after_edit"]);
    assert!(current.revision() > old_snapshot.revision());
    current.graph().validate_consistency()?;
    assert_eq!(old_snapshot.graph().resolve_name("one::alpha").len(), 1);
    assert!(
        old_snapshot
            .graph()
            .resolve_name("one::alpha_after_edit")
            .is_empty()
    );
    assert!(current.graph().resolve_name("one::alpha").is_empty());
    assert_eq!(
        metrics.files_reparsed - baseline.files_reparsed,
        1,
        "ordinary edit must parse only its changed file"
    );
    assert_eq!(
        metrics.relationship_files_recomputed - baseline.relationship_files_recomputed,
        1,
        "unrelated relationship owners must not be recomputed"
    );
    eprintln!(
        "live_single_file_reindex: elapsed={reindex_elapsed:?}, reparsed={}, relationship_files_recomputed={}",
        metrics.files_reparsed - baseline.files_reparsed,
        metrics.relationship_files_recomputed - baseline.relationship_files_recomputed,
    );
    live.shutdown()?;
    Ok(())
}

#[test]
fn declaration_edit_re_resolves_call_sites_without_recomputing_callers()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(repository.path(), "src/target_a.rs", "pub fn target() {}\n")?;
    write(repository.path(), "src/target_b.rs", "pub fn target() {}\n")?;
    write(
        repository.path(),
        "src/caller.rs",
        "pub fn invoke() { target(); }\n",
    )?;
    let (engine, live) = start(&repository)?;

    let initial = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("caller::invoke".to_owned())),
        freshness: FreshnessRequirement::RequireFresh,
        ..ContextRequest::default()
    })?;
    assert!(initial.data.callees.is_empty());
    assert_eq!(initial.data.syntax_call_candidates.len(), 2);
    assert!(
        initial.data.syntax_call_candidates.iter().all(|candidate| {
            candidate.resolution == CallResolution::Ambiguous { candidates: 2 }
        })
    );
    let baseline = live.metrics();

    write(
        repository.path(),
        "src/target_b.rs",
        "pub fn other_target() {}\n",
    )?;
    let updated = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("caller::invoke".to_owned())),
        freshness: FreshnessRequirement::RequireFresh,
        ..ContextRequest::default()
    })?;

    assert!(updated.data.syntax_call_candidates.is_empty());
    assert_eq!(updated.data.callees.len(), 1);
    assert_eq!(
        updated.data.callees[0].symbol.qualified_name,
        "target_a::target"
    );
    let metrics = live.metrics();
    assert_eq!(metrics.files_reparsed - baseline.files_reparsed, 1);
    assert_eq!(
        metrics.relationship_files_recomputed - baseline.relationship_files_recomputed,
        1,
        "a declaration edit must not recompute the unchanged caller contribution"
    );
    engine.snapshot().graph().validate_consistency()?;
    live.shutdown()?;
    Ok(())
}

#[test]
fn rapid_editor_replacements_converge_to_the_latest_source() -> Result<(), Box<dyn Error>> {
    const EDITS: u64 = 32;

    let repository = repository()?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();
    let target = repository.path().join("src/one.rs");
    let swap = repository.path().join("src/.one.rs.chakra-swap");
    let backup = repository.path().join("src/.one.rs.chakra-backup");

    for edit in 0..EDITS {
        fs::write(&swap, format!("pub fn rapid_edit_{edit}() {{}}\n"))?;
        fs::rename(&target, &backup)?;
        fs::rename(&swap, &target)?;
        fs::remove_file(&backup)?;
    }

    let fresh_started = Instant::now();
    assert_eq!(symbols(&engine, "rapid_edit_31")?, ["one::rapid_edit_31"]);
    let fresh_elapsed = fresh_started.elapsed();
    assert!(symbols(&engine, "rapid_edit_0")?.is_empty());
    assert_eq!(symbols(&engine, "two::beta")?, ["two::beta"]);

    let metrics = live.metrics();
    let reparsed = metrics.files_reparsed - baseline.files_reparsed;
    assert!(reparsed >= 1, "the changed file must be reparsed");
    assert!(
        reparsed <= EDITS,
        "coalescing must not invent more parses than materialized edits"
    );
    engine.snapshot().graph().validate_consistency()?;
    eprintln!(
        "live_rapid_replacement: edits={EDITS}, fresh_barrier={fresh_elapsed:?}, reparsed={reparsed}, reconciliations={}, dropped_events={}",
        metrics.reconciliations, metrics.dropped_watcher_events,
    );

    live.shutdown()?;
    Ok(())
}

#[test]
fn create_rename_and_delete_are_visible_without_sleeps() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start(&repository)?;

    write(
        repository.path(),
        "src/created.rs",
        "pub fn appeared() {}\n",
    )?;
    assert_eq!(
        symbols(&engine, "created::appeared")?,
        ["created::appeared"]
    );

    fs::rename(
        repository.path().join("src/created.rs"),
        repository.path().join("src/renamed.rs"),
    )?;
    assert_eq!(
        symbols(&engine, "renamed::appeared")?,
        ["renamed::appeared"]
    );
    assert!(symbols(&engine, "created::appeared")?.is_empty());

    fs::remove_file(repository.path().join("src/renamed.rs"))?;
    assert!(symbols(&engine, "appeared")?.is_empty());
    let map = engine.repo_map(RepoMapRequest {
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
    })?;
    assert!(
        map.data
            .files
            .iter()
            .all(|file| file.path.as_str() != "src/renamed.rs")
    );
    live.shutdown()?;
    Ok(())
}

#[test]
fn atomic_save_and_temporary_syntax_error_publish_complete_revisions() -> Result<(), Box<dyn Error>>
{
    let repository = repository()?;
    let (engine, live) = start(&repository)?;

    write(
        repository.path(),
        "src/.one.rs.chakra-swap",
        "pub fn atomically_saved() {}\n",
    )?;
    fs::rename(
        repository.path().join("src/.one.rs.chakra-swap"),
        repository.path().join("src/one.rs"),
    )?;
    assert_eq!(
        symbols(&engine, "atomically_saved")?,
        ["one::atomically_saved"]
    );

    write(
        repository.path(),
        "src/one.rs",
        "pub fn retained() {}\npub fn temporarily_broken( {\n",
    )?;
    assert_eq!(symbols(&engine, "retained")?, ["one::retained"]);
    let broken_revision = engine.snapshot();
    broken_revision.graph().validate_consistency()?;
    assert_eq!(live.metrics().syntax_error_files, 1);

    write(repository.path(), "src/one.rs", "pub fn recovered() {}\n")?;
    assert_eq!(symbols(&engine, "recovered")?, ["one::recovered"]);
    assert_eq!(live.metrics().syntax_error_files, 0);
    assert!(engine.snapshot().revision() > broken_revision.revision());
    live.shutdown()?;
    Ok(())
}

#[test]
fn php_edit_is_immediately_fresh_and_does_not_reparse_rust() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "src/PaymentService.php",
        "<?php namespace App; class PaymentService { public function refund(): void {} }\n",
    )?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "src/PaymentService.php",
        "<?php namespace App; class PaymentService { public function refundNow(): void {} }\n",
    )?;
    let response = engine.symbol_search(SymbolSearchRequest {
        query: "refundNow".to_owned(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
    })?;
    assert_eq!(response.freshness, Freshness::Fresh);
    assert_eq!(response.data.candidates.len(), 1);
    assert_eq!(response.data.candidates[0].language, Language::Php);
    assert_eq!(
        response.data.candidates[0].qualified_name,
        "App::PaymentService::refundNow"
    );
    assert!(
        engine
            .snapshot()
            .graph()
            .resolve_name("App::PaymentService::refund")
            .is_empty()
    );

    let metrics = live.metrics();
    assert_eq!(metrics.files_reparsed - baseline.files_reparsed, 1);
    assert_eq!(
        metrics.relationship_files_recomputed - baseline.relationship_files_recomputed,
        1
    );
    engine.snapshot().graph().validate_consistency()?;
    live.shutdown()?;
    Ok(())
}
