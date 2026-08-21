//! Deterministic live-update regressions over a real temporary Git worktree.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::indexing::{IndexBudgetKind, IndexBudgets, IndexCancellation, IndexPhase};
use chakra_domain::location::RepoRelativePath;
use chakra_domain::query::{
    ContextRequest, DiffContextRequest, QueryError, QueryService, RepoMapRequest, StatusRequest,
    SymbolRef, SymbolSearchRequest,
};
use chakra_domain::source::SourceClassification;
use chakra_domain::state::{Freshness, FreshnessRequirement, WorkspaceStatus};
use chakra_domain::symbol::{CallResolution, EdgeKind, Language};
use chakra_engine::WorkspaceEngine;
use chakra_language::{
    IndexOptions, LiveIndex, LiveIndexError, LiveIndexOptions, ReconciliationKind,
    index_repository, index_repository_with_options, start_live_index,
    start_live_index_with_options,
};
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
    start_with_options(repository, None)
}

fn start_with_options(
    repository: &TempDir,
    options: Option<LiveIndexOptions>,
) -> Result<(Arc<WorkspaceEngine>, LiveIndex), Box<dyn Error>> {
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = Arc::new(WorkspaceEngine::new(identity));
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Indexing);
    update.set_freshness(Freshness::Stale);
    engine.publish(update)?;
    let live = if let Some(options) = options {
        start_live_index_with_options(
            report.repository_root,
            report.syntax_index,
            engine.clone(),
            options,
        )?
    } else {
        start_live_index(report.repository_root, report.syntax_index, engine.clone())?
    };
    Ok((engine, live))
}

#[test]
fn zero_full_reconciliation_interval_is_rejected() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = Arc::new(WorkspaceEngine::new(identity));

    let result = start_live_index_with_options(
        report.repository_root,
        report.syntax_index,
        engine,
        LiveIndexOptions {
            full_reconcile_interval: 0,
        },
    );

    assert!(matches!(
        result,
        Err(LiveIndexError::InvalidFullReconcileInterval)
    ));
    Ok(())
}

#[test]
fn degraded_budget_metadata_survives_incremental_live_updates() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let budgets = IndexBudgets {
        max_files: 2,
        ..IndexBudgets::default()
    };
    let report = index_repository_with_options(
        repository.path(),
        IndexOptions::new(budgets, IndexCancellation::default())?,
    )?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = Arc::new(WorkspaceEngine::new(identity));
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Indexing);
    update.set_freshness(Freshness::Stale);
    engine.publish(update)?;
    let live = start_live_index(report.repository_root, report.syntax_index, engine.clone())?;

    let initial = engine.snapshot();
    assert_eq!(initial.status(), WorkspaceStatus::Degraded);
    assert_eq!(initial.indexing().coverage.discovered_files, 3);
    assert_eq!(initial.indexing().coverage.indexed_files, 2);
    assert!(
        initial
            .indexing()
            .degradations
            .iter()
            .any(|item| { item.cause == IndexBudgetKind::Files && item.omitted == 1 })
    );
    let baseline = live.metrics();

    write(
        repository.path(),
        "src/one.rs",
        "pub fn alpha_after_budgeted_edit() {}\n",
    )?;
    let updated = engine.symbol_search(SymbolSearchRequest {
        query: "alpha_after_budgeted_edit".to_owned(),
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(updated.freshness, Freshness::Fresh);
    assert_eq!(
        updated
            .data
            .candidates
            .iter()
            .map(|candidate| candidate.qualified_name.as_str())
            .collect::<Vec<_>>(),
        ["one::alpha_after_budgeted_edit"]
    );
    assert_eq!(updated.status, WorkspaceStatus::Degraded);
    assert_eq!(updated.indexing.coverage.discovered_files, 3);
    assert_eq!(updated.indexing.coverage.indexed_files, 2);
    for required in [
        IndexPhase::ParseExtraction,
        IndexPhase::SymbolCatalog,
        IndexPhase::Relationships,
        IndexPhase::GraphMaterialization,
        IndexPhase::LanguageComposition,
        IndexPhase::LiveReconciliation,
    ] {
        assert!(
            updated
                .indexing
                .phases
                .iter()
                .any(|measurement| measurement.phase == required),
            "missing live phase measurement: {required:?}"
        );
    }
    assert_eq!(live.metrics().files_reparsed - baseline.files_reparsed, 1);
    engine.snapshot().graph().validate_consistency()?;
    live.shutdown()?;
    Ok(())
}

fn symbols(
    engine: &WorkspaceEngine,
    query: &str,
) -> Result<Vec<String>, chakra_domain::query::QueryError> {
    let result = engine.symbol_search(SymbolSearchRequest {
        query: query.to_owned(),
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
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
    let unchanged_path = RepoRelativePath::new("src/two.rs")?;
    let changed_path = RepoRelativePath::new("src/one.rs")?;
    let unchanged_symbol = old_snapshot
        .graph()
        .resolve_name("two::beta")
        .into_iter()
        .next()
        .ok_or("fixture beta symbol must exist")?;
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
    assert!(
        current
            .graph()
            .shares_file_payload_with(old_snapshot.graph(), &unchanged_path),
        "unchanged file payload must be physically shared across revisions"
    );
    assert!(
        current
            .graph()
            .shares_symbol_payload_with(old_snapshot.graph(), unchanged_symbol),
        "unchanged symbol payload must be physically shared across revisions"
    );
    assert!(matches!(
        engine.context(ContextRequest {
            symbol: Some(SymbolRef::ById {
                id: unchanged_symbol,
                revision: old_snapshot.revision(),
            }),
            freshness: FreshnessRequirement::RequireFresh,
            ..ContextRequest::default()
        }),
        Err(QueryError::StaleSymbolRef {
            reference_revision,
            current_revision,
        }) if reference_revision == old_snapshot.revision()
            && current_revision > old_snapshot.revision()
    ));
    assert!(
        !current
            .graph()
            .shares_file_payload_with(old_snapshot.graph(), &changed_path),
        "changed file payload must be replaced"
    );
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
    assert_eq!(
        metrics.full_reconciliations - baseline.full_reconciliations,
        0,
        "a normal supported-source edit must remain a targeted reconciliation"
    );
    let publication = current.indexing().publication;
    assert!(publication.structurally_incremental);
    assert_eq!(publication.rebuilt_files, 1);
    assert_eq!(publication.reused_files, current.graph().file_count() - 1);
    assert_eq!(publication.copied_source_bytes, 0);
    assert_eq!(publication.copied_symbols, 0);
    assert!(
        publication.copied_edges < current.graph().edge_count().saturating_mul(2),
        "a one-file edit must not copy both adjacency indexes in full"
    );
    assert_eq!(publication.copied_call_sites, 0);
    assert_eq!(
        metrics.graph_files_rebuilt - baseline.graph_files_rebuilt,
        1
    );
    assert_eq!(
        metrics.graph_symbols_copied - baseline.graph_symbols_copied,
        0
    );
    assert_eq!(
        metrics.graph_edges_copied - baseline.graph_edges_copied,
        publication.copied_edges
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
fn source_only_edit_reuses_every_graph_fact() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start(&repository)?;
    let old = engine.snapshot();
    let path = RepoRelativePath::new("src/one.rs")?;
    let alpha = old
        .graph()
        .resolve_name("one::alpha")
        .into_iter()
        .next()
        .ok_or("fixture alpha symbol must exist")?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "src/one.rs",
        "pub fn alpha() {}\n// source-only edit\n",
    )?;
    assert_eq!(symbols(&engine, "one::alpha")?, ["one::alpha"]);
    let current = engine.snapshot();
    let publication = current.indexing().publication;

    assert!(publication.structurally_incremental);
    assert_eq!(publication.rebuilt_files, 1);
    assert_eq!(publication.rebuilt_symbols, 0);
    assert_eq!(publication.rebuilt_edges, 0);
    assert_eq!(publication.rebuilt_call_sites, 0);
    assert_eq!(publication.copied_source_bytes, 0);
    assert_eq!(publication.copied_symbols, 0);
    assert_eq!(publication.copied_edges, 0);
    assert_eq!(publication.copied_call_sites, 0);
    assert_eq!(publication.reused_symbols, current.graph().symbol_count());
    assert_eq!(publication.reused_edges, current.graph().edge_count());
    assert_eq!(
        publication.reused_call_sites,
        current.graph().call_site_count()
    );
    let composition = current
        .indexing()
        .phases
        .iter()
        .find(|phase| phase.phase == IndexPhase::LanguageComposition)
        .ok_or("live update must report shallow language composition")?;
    // Every registered adapter composes shallowly; empty language partitions
    // cost no graph facts. Keep the invariant tied to the domain catalog so
    // append-only language additions cannot stale this assertion.
    assert_eq!(composition.work_items, u64::try_from(Language::ALL.len())?);
    let materialization = current
        .indexing()
        .phases
        .iter()
        .find(|phase| phase.phase == IndexPhase::GraphMaterialization)
        .ok_or("live update must report graph materialization")?;
    assert_eq!(materialization.work_items, 1);
    assert!(
        current
            .indexing()
            .phases
            .iter()
            .all(|phase| phase.phase != IndexPhase::GraphValidation),
        "an ordinary live delta must not claim a full consistency audit"
    );
    assert!(
        current
            .graph()
            .shares_symbol_payload_with(old.graph(), alpha)
    );
    assert!(!current.graph().shares_file_payload_with(old.graph(), &path));
    let metrics = live.metrics();
    assert_eq!(metrics.files_reparsed - baseline.files_reparsed, 1);
    assert_eq!(
        metrics.relationship_files_recomputed - baseline.relationship_files_recomputed,
        0
    );
    current.graph().validate_consistency()?;
    old.graph().validate_consistency()?;
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
    assert!(initial.data.syntax_call_candidates.iter().all(|candidate| {
        candidate
            .representative_evidence
            .iter()
            .all(|evidence| evidence.resolution == CallResolution::Ambiguous { candidates: 2 })
    }));
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
    assert_eq!(
        metrics.full_reconciliations - baseline.full_reconciliations,
        0,
        "editor replacement temp paths must not force full source rereads"
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
    let before_rename = engine.repo_map(RepoMapRequest {
        limit: Some(1),
        ..RepoMapRequest::default()
    })?;
    let rename_cursor = before_rename
        .data
        .next_cursor
        .ok_or("rename cursor missing")?;

    fs::rename(
        repository.path().join("src/created.rs"),
        repository.path().join("src/renamed.rs"),
    )?;
    assert_eq!(
        symbols(&engine, "renamed::appeared")?,
        ["renamed::appeared"]
    );
    assert!(symbols(&engine, "created::appeared")?.is_empty());
    assert!(matches!(
        engine.repo_map(RepoMapRequest {
            cursor: Some(rename_cursor),
            ..RepoMapRequest::default()
        }),
        Err(QueryError::StaleCursor { .. })
    ));
    let before_delete = engine.repo_map(RepoMapRequest {
        limit: Some(1),
        ..RepoMapRequest::default()
    })?;
    let delete_cursor = before_delete
        .data
        .next_cursor
        .ok_or("delete cursor missing")?;

    fs::remove_file(repository.path().join("src/renamed.rs"))?;
    assert!(symbols(&engine, "appeared")?.is_empty());
    assert!(matches!(
        engine.repo_map(RepoMapRequest {
            cursor: Some(delete_cursor),
            ..RepoMapRequest::default()
        }),
        Err(QueryError::StaleCursor { .. })
    ));
    let map = engine.repo_map(RepoMapRequest {
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..RepoMapRequest::default()
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
    let broken_status = engine.status(StatusRequest)?;
    assert_eq!(broken_status.revision, broken_revision.revision());
    assert_eq!(
        broken_status.data.syntax_diagnostics.files_with_diagnostics,
        1
    );
    assert!(
        broken_status
            .data
            .syntax_diagnostics
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.range.file().as_str() == "src/one.rs")
    );

    write(repository.path(), "src/one.rs", "pub fn recovered() {}\n")?;
    assert_eq!(symbols(&engine, "recovered")?, ["one::recovered"]);
    assert_eq!(live.metrics().syntax_error_files, 0);
    assert!(engine.snapshot().revision() > broken_revision.revision());
    let recovered_status = engine.status(StatusRequest)?;
    assert_eq!(
        recovered_status.data.syntax_diagnostics.total_diagnostics,
        0
    );
    assert!(
        recovered_status
            .data
            .syntax_diagnostics
            .diagnostics
            .is_empty()
    );
    live.shutdown()?;
    Ok(())
}

#[test]
fn non_utf8_edit_degrades_to_skipped_file_without_wedging_the_worker() -> Result<(), Box<dyn Error>>
{
    let repository = repository()?;
    let (engine, live) = start(&repository)?;
    assert_eq!(symbols(&engine, "one::alpha")?, ["one::alpha"]);

    // Latin-1 bytes that are not valid UTF-8: the file must be skipped and
    // counted instead of wedging the reconciliation worker.
    fs::write(
        repository.path().join("src/one.rs"),
        b"// caf\xe9\r\npub fn alpha_lost() {}\n",
    )?;
    assert_eq!(symbols(&engine, "two::beta")?, ["two::beta"]);
    let degraded = engine.snapshot();
    degraded.graph().validate_consistency()?;
    assert_eq!(degraded.indexing().coverage.unreadable_files, 1);
    assert!(degraded.graph().resolve_name("one::alpha").is_empty());
    assert!(degraded.graph().resolve_name("alpha_lost").is_empty());

    write(
        repository.path(),
        "src/one.rs",
        "pub fn alpha_recovered() {}\n",
    )?;
    assert_eq!(
        symbols(&engine, "alpha_recovered")?,
        ["one::alpha_recovered"]
    );
    let recovered = engine.snapshot();
    assert_eq!(recovered.indexing().coverage.unreadable_files, 0);
    assert!(recovered.revision() > degraded.revision());
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
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
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

#[test]
fn typescript_edit_is_immediately_fresh_and_does_not_reparse_rust() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "src/paymentService.ts",
        "export class PaymentService { refund(): void {} }\n",
    )?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "src/paymentService.ts",
        "export class PaymentService { refundNow(): void {} }\n",
    )?;
    let response = engine.symbol_search(SymbolSearchRequest {
        query: "refundNow".to_owned(),
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(response.freshness, Freshness::Fresh);
    assert_eq!(response.data.candidates.len(), 1);
    assert_eq!(response.data.candidates[0].language, Language::TypeScript);
    assert_eq!(
        response.data.candidates[0].qualified_name,
        "paymentService::PaymentService::refundNow"
    );
    assert!(
        engine
            .snapshot()
            .graph()
            .resolve_name("paymentService::PaymentService::refund")
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

#[test]
fn python_edit_is_immediately_fresh_and_does_not_reparse_rust() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "src/payment_service.py",
        "class PaymentService:\n    def refund(self):\n        pass\n",
    )?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "src/payment_service.py",
        "class PaymentService:\n    def refund_now(self):\n        pass\n",
    )?;
    let response = engine.symbol_search(SymbolSearchRequest {
        query: "refund_now".to_owned(),
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(response.freshness, Freshness::Fresh);
    assert_eq!(response.data.candidates.len(), 1);
    assert_eq!(response.data.candidates[0].language, Language::Python);
    assert_eq!(
        response.data.candidates[0].qualified_name,
        "payment_service::PaymentService::refund_now"
    );
    assert!(
        engine
            .snapshot()
            .graph()
            .resolve_name("payment_service::PaymentService::refund")
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

#[test]
fn javascript_edit_is_immediately_fresh_and_does_not_reparse_rust() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "src/payment_service.js",
        "class PaymentService {\n    refund() {}\n}\nmodule.exports = { PaymentService };\n",
    )?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "src/payment_service.js",
        "class PaymentService {\n    refundNow() {}\n}\nmodule.exports = { PaymentService };\n",
    )?;
    let response = engine.symbol_search(SymbolSearchRequest {
        query: "refundNow".to_owned(),
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(response.freshness, Freshness::Fresh);
    assert_eq!(response.data.candidates.len(), 1);
    assert_eq!(response.data.candidates[0].language, Language::JavaScript);
    assert_eq!(
        response.data.candidates[0].qualified_name,
        "payment_service::PaymentService::refundNow"
    );
    assert!(
        engine
            .snapshot()
            .graph()
            .resolve_name("payment_service::PaymentService::refund")
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

#[test]
fn java_edit_is_immediately_fresh_and_does_not_reparse_rust() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "src/main/java/chakra/PaymentService.java",
        "package chakra;\npublic class PaymentService {\n    void refund() {}\n}\n",
    )?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "src/main/java/chakra/PaymentService.java",
        "package chakra;\npublic class PaymentService {\n    void refundNow() {}\n}\n",
    )?;
    let response = engine.symbol_search(SymbolSearchRequest {
        query: "refundNow".to_owned(),
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(response.freshness, Freshness::Fresh);
    assert_eq!(response.data.candidates.len(), 1);
    assert_eq!(response.data.candidates[0].language, Language::Java);
    assert_eq!(
        response.data.candidates[0].qualified_name,
        "chakra::PaymentService::refundNow"
    );
    assert!(
        engine
            .snapshot()
            .graph()
            .resolve_name("chakra::PaymentService::refund")
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

#[test]
fn csharp_edit_is_immediately_fresh_and_does_not_reparse_rust() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "src/main/csharp/Chakra/PaymentService.cs",
        "namespace Chakra;\npublic class PaymentService {\n    void Refund() {}\n}\n",
    )?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "src/main/csharp/Chakra/PaymentService.cs",
        "namespace Chakra;\npublic class PaymentService {\n    void RefundNow() {}\n}\n",
    )?;
    let response = engine.symbol_search(SymbolSearchRequest {
        query: "RefundNow".to_owned(),
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(response.freshness, Freshness::Fresh);
    assert_eq!(response.data.candidates.len(), 1);
    assert_eq!(response.data.candidates[0].language, Language::CSharp);
    assert_eq!(
        response.data.candidates[0].qualified_name,
        "Chakra::PaymentService::RefundNow"
    );
    assert!(
        engine
            .snapshot()
            .graph()
            .resolve_name("Chakra::PaymentService::Refund")
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

#[test]
fn shell_edit_is_immediately_fresh_and_does_not_reparse_rust() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "scripts/payment_service.sh",
        "refund() { true; }\n",
    )?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "scripts/payment_service.sh",
        "refund_now() { true; }\n",
    )?;
    let response = engine.symbol_search(SymbolSearchRequest {
        query: "refund_now".to_owned(),
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(response.freshness, Freshness::Fresh);
    assert_eq!(response.data.candidates.len(), 1);
    assert_eq!(response.data.candidates[0].language, Language::Shell);
    assert_eq!(
        response.data.candidates[0].qualified_name,
        "scripts::payment_service::refund_now"
    );
    assert!(
        engine
            .snapshot()
            .graph()
            .resolve_name("scripts::payment_service::refund")
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

#[test]
fn cpp_edit_is_immediately_fresh_and_does_not_reparse_rust() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(repository.path(), "CMakeLists.txt", "project(chakra)\n")?;
    write(
        repository.path(),
        "src/payment_service.cpp",
        "namespace chakra { class PaymentService { public: void refund() {} }; }\n",
    )?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "src/payment_service.cpp",
        "namespace chakra { class PaymentService { public: void refund_now() {} }; }\n",
    )?;
    let response = engine.symbol_search(SymbolSearchRequest {
        query: "refund_now".to_owned(),
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(response.freshness, Freshness::Fresh);
    assert_eq!(response.data.candidates.len(), 1);
    assert_eq!(response.data.candidates[0].language, Language::Cpp);
    assert_eq!(
        response.data.candidates[0].qualified_name,
        "chakra::PaymentService::refund_now"
    );
    assert!(
        engine
            .snapshot()
            .graph()
            .resolve_name("chakra::PaymentService::refund")
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

#[test]
fn hcl_edit_is_immediately_fresh_and_does_not_reparse_rust() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "infra/main.tf",
        "resource \"null_resource\" \"refund\" {}\n",
    )?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "infra/main.tf",
        "resource \"null_resource\" \"refund_now\" {}\n",
    )?;
    let response = engine.symbol_search(SymbolSearchRequest {
        query: "refund_now".to_owned(),
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(response.freshness, Freshness::Fresh);
    assert_eq!(response.data.candidates.len(), 1);
    assert_eq!(response.data.candidates[0].language, Language::Hcl);
    assert_eq!(
        response.data.candidates[0].qualified_name,
        "resource::null_resource::refund_now"
    );
    assert!(
        engine
            .snapshot()
            .graph()
            .resolve_name("resource::null_resource::refund")
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

#[test]
fn go_edit_is_immediately_fresh_and_does_not_reparse_rust() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "service.go",
        "package service\nfunc refund() {}\n",
    )?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "service.go",
        "package service\nfunc refundNow() {}\n",
    )?;
    let response = engine.symbol_search(SymbolSearchRequest {
        query: "refundNow".to_owned(),
        source: Default::default(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(response.freshness, Freshness::Fresh);
    assert_eq!(response.data.candidates.len(), 1);
    assert_eq!(response.data.candidates[0].language, Language::Go);
    assert_eq!(
        response.data.candidates[0].qualified_name,
        "service::refundNow"
    );
    assert!(
        engine
            .snapshot()
            .graph()
            .resolve_name("service::refund")
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

#[test]
fn first_file_of_a_live_language_receives_budget_without_reparsing_other_files()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();

    write(
        repository.path(),
        "src/PaymentService.php",
        "<?php namespace App; class PaymentService { public function refund(): void {} }\n",
    )?;
    let response = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;

    assert_eq!(response.data.candidates.len(), 1);
    assert_eq!(response.data.candidates[0].language, Language::Php);
    assert_eq!(
        response.data.candidates[0].qualified_name,
        "App::PaymentService::refund"
    );
    assert_eq!(symbols(&engine, "one::alpha")?, ["one::alpha"]);
    assert!(!engine.snapshot().indexing().is_degraded());
    assert_eq!(
        live.metrics().files_reparsed - baseline.files_reparsed,
        1,
        "activating PHP may rebalance cached graphs but must parse only the new file"
    );
    engine.snapshot().graph().validate_consistency()?;
    live.shutdown()?;
    Ok(())
}

#[test]
fn warmed_noop_fresh_barrier_reads_no_source_bodies_or_watch_sets() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();
    let started = Instant::now();

    assert_eq!(symbols(&engine, "one::alpha")?, ["one::alpha"]);

    let elapsed = started.elapsed();
    let metrics = live.metrics();
    assert_eq!(metrics.barrier_requests - baseline.barrier_requests, 1);
    assert_eq!(
        metrics.barrier_generations_completed - baseline.barrier_generations_completed,
        1
    );
    assert_eq!(metrics.git_subprocesses - baseline.git_subprocesses, 2);
    assert_eq!(metrics.files_read - baseline.files_read, 0);
    assert_eq!(metrics.source_bytes_read - baseline.source_bytes_read, 0);
    assert_eq!(metrics.files_reparsed - baseline.files_reparsed, 0);
    assert_eq!(
        metrics.watch_set_recomputations - baseline.watch_set_recomputations,
        0
    );
    assert_eq!(
        metrics.no_op_reconciliations - baseline.no_op_reconciliations,
        1
    );
    assert_eq!(metrics.last_reconciliation_kind, ReconciliationKind::Noop);
    eprintln!(
        "live_noop_fresh: elapsed_us={} files_inspected={} bytes_inspected={} git_subprocesses={} files_read={} bytes_read={}",
        elapsed.as_micros(),
        metrics.files_inspected - baseline.files_inspected,
        metrics.source_bytes_inspected - baseline.source_bytes_inspected,
        metrics.git_subprocesses - baseline.git_subprocesses,
        metrics.files_read - baseline.files_read,
        metrics.source_bytes_read - baseline.source_bytes_read,
    );
    live.shutdown()?;
    Ok(())
}

#[test]
fn clean_diff_context_uses_two_lightweight_proofs_without_body_scans() -> Result<(), Box<dyn Error>>
{
    let repository = repository()?;
    for args in [
        ["config", "user.email", "tests@example.invalid"].as_slice(),
        ["config", "user.name", "Chakra Tests"].as_slice(),
        ["add", "src"].as_slice(),
        ["commit", "--quiet", "-m", "fixture"].as_slice(),
    ] {
        let status = Command::new("git")
            .current_dir(repository.path())
            .args(args)
            .status()?;
        if !status.success() {
            return Err(format!("git {} failed", args.join(" ")).into());
        }
    }
    let (engine, live) = start(&repository)?;
    engine.install_diff_provider(Arc::new(chakra_git::GitWorkspaceDiff))?;
    let baseline = live.metrics();

    let response = engine.diff_context(DiffContextRequest {
        limit: None,
        freshness: FreshnessRequirement::RequireFresh,
        ..DiffContextRequest::default()
    })?;

    assert!(response.data.changed_files.is_empty());
    let metrics = live.metrics();
    assert_eq!(metrics.barrier_requests - baseline.barrier_requests, 2);
    assert_eq!(metrics.git_subprocesses - baseline.git_subprocesses, 4);
    assert_eq!(metrics.files_read - baseline.files_read, 0);
    assert_eq!(metrics.source_bytes_read - baseline.source_bytes_read, 0);
    assert_eq!(metrics.files_reparsed - baseline.files_reparsed, 0);
    assert_eq!(
        metrics.no_op_reconciliations - baseline.no_op_reconciliations,
        2
    );
    assert_eq!(
        metrics.full_reconciliations - baseline.full_reconciliations,
        0
    );
    live.shutdown()?;
    Ok(())
}

#[test]
fn configured_checkpoint_forces_a_bounded_full_reread() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let (engine, live) = start_with_options(
        &repository,
        Some(LiveIndexOptions {
            full_reconcile_interval: 1,
        }),
    )?;
    assert_eq!(symbols(&engine, "one::alpha")?, ["one::alpha"]);
    let baseline = live.metrics();

    assert_eq!(symbols(&engine, "one::alpha")?, ["one::alpha"]);

    let metrics = live.metrics();
    assert_eq!(
        metrics.full_reconciliations - baseline.full_reconciliations,
        1
    );
    assert_eq!(metrics.files_read - baseline.files_read, 3);
    assert_eq!(metrics.files_reparsed - baseline.files_reparsed, 0);
    assert_eq!(metrics.last_reconciliation_kind, ReconciliationKind::Full);
    live.shutdown()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn same_size_timestamp_preserving_edit_is_read_immediately() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let source = repository.path().join("src/one.rs");
    let timestamp_reference = repository.path().join("one.timestamp-reference");
    let copied = Command::new("cp")
        .args(["-p"])
        .arg(&source)
        .arg(&timestamp_reference)
        .status()?;
    if !copied.success() {
        return Err("cp -p failed".into());
    }
    let (engine, live) = start(&repository)?;
    let baseline = live.metrics();
    fs::write(&source, "pub fn omega() {}\n")?;
    let restored = Command::new("touch")
        .args(["-r"])
        .arg(&timestamp_reference)
        .arg(&source)
        .status()?;
    if !restored.success() {
        return Err("touch -r failed".into());
    }

    assert_eq!(symbols(&engine, "one::omega")?, ["one::omega"]);
    assert!(
        engine
            .snapshot()
            .graph()
            .resolve_name("one::alpha")
            .is_empty()
    );

    let metrics = live.shutdown_with_metrics()?;
    assert!(
        (1..=2).contains(&(metrics.files_read - baseline.files_read)),
        "a racing watcher epoch may retry the changed body, but must not trigger a full body scan"
    );
    assert_eq!(metrics.files_reparsed - baseline.files_reparsed, 1);
    assert_eq!(
        metrics.targeted_reconciliations - baseline.targeted_reconciliations,
        1
    );
    assert_eq!(
        metrics.full_reconciliations - baseline.full_reconciliations,
        0
    );
    Ok(())
}

#[test]
fn laravel_edit_recomputes_one_framework_contribution_and_publishes_fresh_relations()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "composer.json",
        r#"{"require":{"laravel/framework":"^13.0"}}"#,
    )?;
    let path = "app/Provider.php";
    let source = r#"<?php
namespace App;
interface Reporter { public function report(): void; }
final class DatabaseReporter implements Reporter { public function report(): void {} }
final class Provider {
    public function register(): void {
        $this->app->bind(Reporter::class, DatabaseReporter::class);
    }
}
"#;
    write(repository.path(), path, source)?;
    let (engine, live) = start(&repository)?;
    let initial = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("App::DatabaseReporter".to_owned())),
        freshness: FreshnessRequirement::RequireFresh,
        ..ContextRequest::default()
    })?;
    assert!(initial.data.related_relations.iter().any(|relation| {
        relation.relation.edge_kind == EdgeKind::Binds
            && relation.relation.symbol.qualified_name == "App::Reporter"
    }));
    let baseline = live.metrics();

    write(
        repository.path(),
        path,
        &source.replace("->bind(", "->singleton("),
    )?;
    let current = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("App::DatabaseReporter".to_owned())),
        freshness: FreshnessRequirement::RequireFresh,
        ..ContextRequest::default()
    })?;
    assert_eq!(current.freshness, Freshness::Fresh);
    assert!(current.data.related_relations.iter().any(|relation| {
        relation.relation.edge_kind == EdgeKind::Binds
            && relation.relation.provenance == chakra_domain::provenance::Provenance::Heuristic
    }));

    let metrics = live.metrics();
    assert_eq!(metrics.files_reparsed - baseline.files_reparsed, 1);
    assert_eq!(
        metrics.framework_files_reparsed - baseline.framework_files_reparsed,
        1
    );
    assert_eq!(
        metrics.framework_relationship_files_recomputed
            - baseline.framework_relationship_files_recomputed,
        1
    );
    assert_eq!(metrics.framework_truncated_files, 0);
    engine.snapshot().graph().validate_consistency()?;
    live.shutdown()?;
    Ok(())
}

#[test]
fn cargo_manifest_change_refreshes_package_scope_without_reparsing() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    write(
        repository.path(),
        "Cargo.toml",
        "[package]\nname = \"before\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    let lock = Command::new("cargo")
        .current_dir(repository.path())
        .args(["generate-lockfile", "--offline"])
        .status()?;
    if !lock.success() {
        return Err("initial cargo generate-lockfile failed".into());
    }
    let (engine, live) = start(&repository)?;
    let before = engine.symbol_search(SymbolSearchRequest {
        query: "alpha".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(
        before.data.candidates[0]
            .package
            .as_ref()
            .map(|package| package.name.as_str()),
        Some("before")
    );
    assert_eq!(
        before.data.candidates[0].source_classification,
        SourceClassification::CargoMetadata
    );
    let baseline_revision = before.revision;
    let baseline_metrics = live.metrics();

    write(
        repository.path(),
        "Cargo.toml",
        "[package]\nname = \"after\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    let lock = Command::new("cargo")
        .current_dir(repository.path())
        .args(["generate-lockfile", "--offline"])
        .status()?;
    if !lock.success() {
        return Err("updated cargo generate-lockfile failed".into());
    }
    let after = engine.symbol_search(SymbolSearchRequest {
        query: "alpha".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    assert!(after.revision > baseline_revision);
    assert_eq!(
        after.data.candidates[0]
            .package
            .as_ref()
            .map(|package| package.name.as_str()),
        Some("after")
    );
    assert_eq!(
        live.metrics().files_reparsed,
        baseline_metrics.files_reparsed,
        "metadata-only refresh must not reparse stable Rust source"
    );
    let metrics = live.metrics();
    assert!(
        metrics.metadata_files_inspected - baseline_metrics.metadata_files_inspected >= 4,
        "Cargo.toml and Cargo.lock must participate in both sides of the stable identity proof"
    );
    live.shutdown()?;
    Ok(())
}

#[test]
fn composer_manifest_change_refreshes_package_scope_without_reparsing() -> Result<(), Box<dyn Error>>
{
    let repository = repository()?;
    write(
        repository.path(),
        "composer.json",
        r#"{"name":"before/package","autoload":{"psr-4":{"App\\":"app/"}}}"#,
    )?;
    write(
        repository.path(),
        "app/Service.php",
        "<?php namespace App; class Service {}\n",
    )?;
    let (engine, live) = start(&repository)?;
    let before = engine.symbol_search(SymbolSearchRequest {
        query: "Service".to_owned(),
        include_languages: vec![Language::Php],
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(
        before.data.candidates[0]
            .package
            .as_ref()
            .map(|package| package.name.as_str()),
        Some("before/package")
    );
    assert_eq!(
        before.data.candidates[0].source_classification,
        SourceClassification::ComposerMetadata
    );
    let baseline_revision = before.revision;
    let baseline_metrics = live.metrics();

    write(
        repository.path(),
        "composer.json",
        r#"{"name":"after/package","autoload":{"psr-4":{"App\\":"app/"}}}"#,
    )?;
    let after = engine.symbol_search(SymbolSearchRequest {
        query: "Service".to_owned(),
        include_languages: vec![Language::Php],
        ..SymbolSearchRequest::default()
    })?;
    assert!(after.revision > baseline_revision);
    assert_eq!(
        after.data.candidates[0]
            .package
            .as_ref()
            .map(|package| package.name.as_str()),
        Some("after/package")
    );
    assert_eq!(
        live.metrics().files_reparsed,
        baseline_metrics.files_reparsed,
        "metadata-only refresh must not reparse stable PHP source"
    );
    let metrics = live.metrics();
    assert!(
        metrics.metadata_files_inspected - baseline_metrics.metadata_files_inspected >= 2,
        "composer.json must participate in both sides of the stable identity proof"
    );
    live.shutdown()?;
    Ok(())
}
