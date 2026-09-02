use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::Barrier;
use std::thread;
use std::time::Duration;

use chakra_domain::composition::{CommitSnapshotOrigin, CommitSnapshotRejection, SourceLayer};
use chakra_domain::indexing::IndexCancellation;
use chakra_domain::location::SourceRange;
use chakra_domain::operation::OperationContext;
use chakra_domain::provenance::Provenance;
use chakra_domain::query::{
    CallersRequest, ChangeKind, QueryService, SearchRequest, StatusRequest, SymbolMatchMode,
    SymbolSearchRequest, WorkspaceQueryRouter,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::{Freshness, ProviderState, WorkspaceStatus};
use chakra_domain::symbol::Language;
use chakra_engine::{
    PreciseProvider, PreciseQueryRequest, PreciseQueryResult, PreciseRelation, WorkspaceEngine,
};
use chakra_workspace::{
    CommitSnapshotCache, CommitSnapshotCacheConfig, WorkspaceRegistry, WorkspaceRegistryConfig,
    WorkspaceRegistryError, WorkspaceStartOptions,
};
use tempfile::TempDir;

#[derive(Debug)]
struct RootScopedProvider {
    root: PathBuf,
    caller: SourceRange,
}

impl PreciseProvider for RootScopedProvider {
    fn name(&self) -> &'static str {
        "root-scoped-test-provider"
    }

    fn supports(&self, language: Language) -> bool {
        language == Language::Rust
    }

    fn state_for(&self, _revision: Revision) -> ProviderState {
        ProviderState::Ready
    }

    fn enrich_with_context(
        &self,
        request: PreciseQueryRequest,
        _operation: &OperationContext,
    ) -> PreciseQueryResult {
        if request.workspace.repository_root != self.root || request.symbol.name != "target" {
            return PreciseQueryResult::unavailable(
                request.workspace.revision,
                ProviderState::Degraded,
            );
        }
        PreciseQueryResult {
            revision: request.workspace.revision,
            state: ProviderState::Ready,
            fallback_cause: None,
            incoming: vec![PreciseRelation {
                name: "provider_caller".to_owned(),
                declaration: self.caller.clone(),
                occurrence_count: 1,
                call_sites: Vec::new(),
                provenance: Provenance::RustAnalyzer,
            }],
            outgoing: Vec::new(),
            incoming_truncated: false,
            outgoing_truncated: false,
        }
    }
}

#[test]
fn linked_worktrees_keep_files_revisions_and_provider_facts_isolated()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = LinkedWorktrees::new()?;
    assert!(fixture.linked.join(".git").is_file());

    let registry = WorkspaceRegistry::new(WorkspaceRegistryConfig { max_workspaces: 2 })?;
    let primary = registry.register(&fixture.primary, WorkspaceStartOptions::default())?;
    let linked = registry.register(&fixture.linked, WorkspaceStartOptions::default())?;

    assert_eq!(
        primary
            .engine()
            .snapshot()
            .layers()
            .commit_snapshot
            .reuse
            .origin,
        CommitSnapshotOrigin::ColdBuild
    );
    assert_eq!(
        primary
            .engine()
            .snapshot()
            .layers()
            .commit_snapshot
            .reuse
            .rejection,
        Some(CommitSnapshotRejection::CacheDisabled)
    );
    assert_eq!(
        linked
            .engine()
            .snapshot()
            .layers()
            .commit_snapshot
            .reuse
            .origin,
        CommitSnapshotOrigin::MemoryReuse
    );
    assert!(
        linked
            .engine()
            .snapshot()
            .layers()
            .commit_snapshot
            .reuse
            .reused_files
            > 0
    );

    assert_eq!(primary.identity().repository, linked.identity().repository);
    assert_ne!(primary.identity().workspace, linked.identity().workspace);
    assert_eq!(registry.workspaces().len(), 2);

    let primary_engine = registry.workspace(&primary.identity().workspace)?;
    let linked_engine = registry.workspace(&linked.identity().workspace)?;
    assert!(!Arc::ptr_eq(&primary_engine, &linked_engine));
    assert_eq!(
        primary_engine.snapshot().layers().commit_snapshot.commit,
        linked_engine.snapshot().layers().commit_snapshot.commit
    );
    assert!(
        linked_engine
            .snapshot()
            .layers()
            .worktree_overlay
            .files
            .is_empty()
    );
    assert!(matches!(
        registry.route(None),
        Err(chakra_domain::query::QueryError::WorkspaceSelectionRequired { .. })
    ));
    let explicitly_routed = registry.route(Some(&linked.identity().workspace))?;
    assert_eq!(
        explicitly_routed.status(StatusRequest)?.workspace_id,
        linked.identity().workspace
    );

    let primary_revision = primary_engine.snapshot().revision();
    let linked_revision = linked_engine.snapshot().revision();
    fs::write(
        fixture.primary.join("src/lib.rs"),
        "pub fn target() {}\npub fn provider_caller() {}\npub fn only_primary() {}\n",
    )?;

    assert_eq!(search_count(&primary_engine, "only_primary")?, 1);
    assert_eq!(search_count(&linked_engine, "only_primary")?, 0);
    assert!(primary_engine.snapshot().revision() > primary_revision);
    assert_eq!(linked_engine.snapshot().revision(), linked_revision);

    fs::write(
        fixture.linked.join("src/lib.rs"),
        "pub fn target() {}\npub fn provider_caller() {}\npub fn only_linked() {}\n",
    )?;
    assert_eq!(search_count(&linked_engine, "only_linked")?, 1);
    assert_eq!(search_count(&primary_engine, "only_linked")?, 0);

    let caller = exact_symbol(&primary_engine, "provider_caller")?;
    primary_engine.install_precise_provider(Arc::new(RootScopedProvider {
        root: fixture.primary.canonicalize()?,
        caller: caller.location,
    }))?;

    let primary_callers = primary_engine.callers(CallersRequest {
        symbol: Some(chakra_domain::query::SymbolRef::ByName("target".to_owned())),
        ..CallersRequest::default()
    })?;
    assert_eq!(primary_callers.data.callers.len(), 1);
    assert_eq!(
        primary_callers.data.callers[0].provenance,
        Provenance::RustAnalyzer
    );
    assert_eq!(
        primary_callers.layers.workspace_enrichment.revision,
        Some(primary_callers.revision)
    );

    let linked_callers = linked_engine.callers(CallersRequest {
        symbol: Some(chakra_domain::query::SymbolRef::ByName("target".to_owned())),
        ..CallersRequest::default()
    })?;
    assert!(linked_callers.data.callers.is_empty());
    assert!(linked_callers.data.provider.is_none());

    let retained_primary_engine = primary_engine.clone();
    registry.unregister(&primary.identity().workspace)?;
    let stopped = retained_primary_engine.status(StatusRequest)?;
    assert_eq!(stopped.freshness, Freshness::Stale);
    assert_eq!(stopped.status, WorkspaceStatus::Stale);
    assert!(matches!(
        registry.workspace(&primary.identity().workspace),
        Err(WorkspaceRegistryError::NotRegistered { .. })
    ));

    registry.shutdown()?;
    Ok(())
}

#[test]
fn registry_bounds_worktrees_and_rejects_cross_repository_state()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = LinkedWorktrees::new()?;
    let other = LinkedWorktrees::new_with_extra("pub fn other_repository_root() {}\n")?;
    let bounded = WorkspaceRegistry::new(WorkspaceRegistryConfig { max_workspaces: 1 })?;
    let primary = bounded.register(&fixture.primary, WorkspaceStartOptions::default())?;
    assert!(matches!(
        bounded.register(&fixture.linked, WorkspaceStartOptions::default()),
        Err(WorkspaceRegistryError::CapacityReached { limit: 1 })
    ));
    bounded.unregister(&primary.identity().workspace)?;
    bounded.shutdown()?;

    let repository_scoped = WorkspaceRegistry::new(WorkspaceRegistryConfig { max_workspaces: 2 })?;
    repository_scoped.register(&fixture.primary, WorkspaceStartOptions::default())?;
    assert!(matches!(
        repository_scoped.register(&other.primary, WorkspaceStartOptions::default()),
        Err(WorkspaceRegistryError::RepositoryMismatch { .. })
    ));
    repository_scoped.shutdown()?;
    Ok(())
}

#[test]
fn disk_snapshot_restore_rejects_corruption_migration_and_incompatible_budgets()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = LinkedWorktrees::new()?;
    let cache_dir = fixture._temp.path().join("snapshot-cache");
    fs::write(
        fixture.primary.join("src/stable.rs"),
        "pub fn stable_for_snapshot_restore() {}\n",
    )?;
    git(&fixture.primary, &["add", "src/stable.rs"])?;
    git(&fixture.primary, &["commit", "-m", "add stable source"])?;
    let shared_head = git_stdout(&fixture.primary, &["rev-parse", "HEAD"])?;
    git(&fixture.linked, &["checkout", "--detach", &shared_head])?;
    let mut config = CommitSnapshotCacheConfig::with_directory(cache_dir.clone());
    config.max_disk_artifacts = 1;
    let identity = chakra_git::resolve_workspace_identity(&fixture.primary)?;
    let commit = chakra_git::resolve_head_commit_with_context(
        &fixture.primary,
        &OperationContext::unbounded(),
    )?;

    let first_cache = CommitSnapshotCache::new(config.clone())?;
    let first = first_cache.load_or_build(
        &fixture.primary,
        &identity.repository,
        commit.as_deref(),
        chakra_language::IndexOptions::default(),
    )?;
    assert_eq!(first.reuse.origin, CommitSnapshotOrigin::ColdBuild);
    assert_eq!(
        first.reuse.rejection,
        Some(CommitSnapshotRejection::NotFound)
    );
    assert!(first.reuse.artifact_bytes.is_some_and(|bytes| bytes > 0));
    drop(first_cache);

    let initial_payload = fs::read(only_artifact(&cache_dir)?.join("snapshot.bin"))?;
    chakra_language::CommitIndexReport::decode_snapshot(
        fixture.linked.clone(),
        commit.as_deref(),
        chakra_domain::indexing::IndexBudgets::default(),
        &initial_payload,
        &IndexCancellation::default(),
    )
    .map_err(|error| format!("direct snapshot restore failed: {error}"))?;

    let restored = CommitSnapshotCache::new(config.clone())?.load_or_build(
        &fixture.linked,
        &identity.repository,
        commit.as_deref(),
        chakra_language::IndexOptions::default(),
    )?;
    assert_eq!(
        restored.reuse.origin,
        CommitSnapshotOrigin::DiskRestore,
        "reuse report: {:?}",
        restored.reuse
    );
    assert_eq!(restored.graph.symbol_count(), first.graph.symbol_count());
    let linked_source_path = fixture.linked.join("src/lib.rs");
    let linked_source = fs::read_to_string(&linked_source_path)?;
    fs::write(
        &linked_source_path,
        format!("{linked_source}pub fn restored_incremental_edit() {{}}\n"),
    )?;
    let layered = restored
        .clone()
        .compose_worktree(chakra_language::IndexOptions::default())?;
    fs::write(&linked_source_path, linked_source)?;
    assert_eq!(
        layered.effective.metrics.indexing.publication.rebuilt_files,
        1
    );
    assert!(layered.effective.metrics.indexing.publication.reused_files > 0);
    assert!(
        layered
            .effective
            .graph
            .symbols()
            .iter()
            .any(|symbol| symbol.name() == "restored_incremental_edit")
    );

    let artifact = only_artifact(&cache_dir)?;
    let payload_path = artifact.join("snapshot.bin");
    let mut payload = fs::read(&payload_path)?;
    let last = payload.len().saturating_sub(1);
    payload[last] ^= 0xff;
    fs::write(&payload_path, payload)?;
    let repaired = CommitSnapshotCache::new(config.clone())?.load_or_build(
        &fixture.primary,
        &identity.repository,
        commit.as_deref(),
        chakra_language::IndexOptions::default(),
    )?;
    assert_eq!(repaired.reuse.origin, CommitSnapshotOrigin::ColdBuild);
    assert_eq!(
        repaired.reuse.rejection,
        Some(CommitSnapshotRejection::Corrupt)
    );

    let artifact = only_artifact(&cache_dir)?;
    let payload_path = artifact.join("snapshot.bin");
    let mut payload = fs::read(&payload_path)?;
    payload[4..8].copy_from_slice(&0_u32.to_le_bytes());
    fs::write(&payload_path, payload)?;
    let migrated = CommitSnapshotCache::new(config.clone())?.load_or_build(
        &fixture.primary,
        &identity.repository,
        commit.as_deref(),
        chakra_language::IndexOptions::default(),
    )?;
    assert_eq!(migrated.reuse.origin, CommitSnapshotOrigin::ColdBuild);
    assert_eq!(
        migrated.reuse.rejection,
        Some(CommitSnapshotRejection::FormatMismatch)
    );

    let missing_payload = only_artifact(&cache_dir)?.join("snapshot.bin");
    fs::remove_file(&missing_payload)?;
    let repaired_missing_payload = CommitSnapshotCache::new(config.clone())?.load_or_build(
        &fixture.primary,
        &identity.repository,
        commit.as_deref(),
        chakra_language::IndexOptions::default(),
    )?;
    assert_eq!(
        repaired_missing_payload.reuse.origin,
        CommitSnapshotOrigin::ColdBuild
    );
    assert_eq!(
        repaired_missing_payload.reuse.rejection,
        Some(CommitSnapshotRejection::NotFound)
    );
    assert!(!fs::read(only_artifact(&cache_dir)?.join("snapshot.bin"))?.is_empty());

    let mut budgets = chakra_domain::indexing::IndexBudgets::default();
    budgets.max_symbols = budgets.max_symbols.saturating_sub(1);
    let incompatible = CommitSnapshotCache::new(config.clone())?.load_or_build(
        &fixture.primary,
        &identity.repository,
        commit.as_deref(),
        chakra_language::IndexOptions::new(budgets, IndexCancellation::default())?,
    )?;
    assert_eq!(incompatible.reuse.origin, CommitSnapshotOrigin::ColdBuild);
    assert_eq!(
        incompatible.reuse.rejection,
        Some(CommitSnapshotRejection::NotFound)
    );
    assert_eq!(artifact_count(&cache_dir)?, 1);

    let cancellation = IndexCancellation::default();
    cancellation.cancel();
    assert!(matches!(
        CommitSnapshotCache::new(config)?.load_or_build(
            &fixture.primary,
            &identity.repository,
            commit.as_deref(),
            chakra_language::IndexOptions::new(
                chakra_domain::indexing::IndexBudgets::default(),
                cancellation,
            )?,
        ),
        Err(chakra_workspace::CommitSnapshotCacheError::Cancelled)
    ));

    let tiny_dir = fixture._temp.path().join("tiny-snapshot-cache");
    let mut tiny = CommitSnapshotCacheConfig::with_directory(tiny_dir.clone());
    tiny.max_artifact_bytes = 32;
    let oversized = CommitSnapshotCache::new(tiny)?.load_or_build(
        &fixture.primary,
        &identity.repository,
        commit.as_deref(),
        chakra_language::IndexOptions::default(),
    )?;
    assert_eq!(oversized.reuse.origin, CommitSnapshotOrigin::ColdBuild);
    assert_eq!(
        oversized.reuse.rejection,
        Some(CommitSnapshotRejection::Oversized)
    );
    assert_eq!(artifact_count(&tiny_dir)?, 0);
    Ok(())
}

#[test]
fn concurrent_snapshot_builders_coalesce_in_memory_and_publish_one_disk_artifact()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = LinkedWorktrees::new()?;
    let identity = chakra_git::resolve_workspace_identity(&fixture.primary)?;
    let commit = chakra_git::resolve_head_commit_with_context(
        &fixture.primary,
        &OperationContext::unbounded(),
    )?;

    let memory_cache = Arc::new(CommitSnapshotCache::new(
        CommitSnapshotCacheConfig::default(),
    )?);
    let start = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for root in [fixture.primary.clone(), fixture.linked.clone()] {
        let cache = memory_cache.clone();
        let start = start.clone();
        let repository = identity.repository.clone();
        let commit = commit.clone();
        workers.push(thread::spawn(move || {
            start.wait();
            cache.load_or_build(
                &root,
                &repository,
                commit.as_deref(),
                chakra_language::IndexOptions::default(),
            )
        }));
    }
    start.wait();
    let mut origins = Vec::new();
    for worker in workers {
        let report = worker
            .join()
            .map_err(|_| "memory cache worker panicked")??;
        origins.push(report.reuse.origin);
    }
    origins.sort_by_key(|origin| match origin {
        CommitSnapshotOrigin::ColdBuild => 0,
        CommitSnapshotOrigin::MemoryReuse => 1,
        CommitSnapshotOrigin::DiskRestore => 2,
    });
    assert_eq!(
        origins,
        vec![
            CommitSnapshotOrigin::ColdBuild,
            CommitSnapshotOrigin::MemoryReuse
        ]
    );

    let cache_dir = fixture._temp.path().join("concurrent-disk-cache");
    let config = CommitSnapshotCacheConfig::with_directory(cache_dir.clone());
    let start = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for root in [fixture.primary.clone(), fixture.linked.clone()] {
        let cache = CommitSnapshotCache::new(config.clone())?;
        let start = start.clone();
        let repository = identity.repository.clone();
        let commit = commit.clone();
        workers.push(thread::spawn(move || {
            start.wait();
            cache.load_or_build(
                &root,
                &repository,
                commit.as_deref(),
                chakra_language::IndexOptions::default(),
            )
        }));
    }
    start.wait();
    let mut disk_origins = Vec::new();
    for worker in workers {
        let report = worker.join().map_err(|_| "disk cache worker panicked")??;
        disk_origins.push(report.reuse.origin);
    }
    assert!(disk_origins.contains(&CommitSnapshotOrigin::ColdBuild));
    assert!(disk_origins.iter().all(|origin| matches!(
        origin,
        CommitSnapshotOrigin::ColdBuild | CommitSnapshotOrigin::DiskRestore
    )));
    assert_eq!(artifact_count(&cache_dir)?, 1);
    assert_eq!(
        CommitSnapshotCache::new(config)?
            .load_or_build(
                &fixture.primary,
                &identity.repository,
                commit.as_deref(),
                chakra_language::IndexOptions::default(),
            )?
            .reuse
            .origin,
        CommitSnapshotOrigin::DiskRestore
    );
    Ok(())
}

#[test]
fn head_transitions_reuse_commit_state_without_sharing_worktree_overlay()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = LinkedWorktrees::new()?;
    let registry = WorkspaceRegistry::new(WorkspaceRegistryConfig { max_workspaces: 2 })?;
    let primary = registry.register(&fixture.primary, WorkspaceStartOptions::default())?;
    let linked = registry.register(&fixture.linked, WorkspaceStartOptions::default())?;

    fs::write(
        fixture.primary.join("src/lib.rs"),
        "pub fn target() {}\npub fn provider_caller() {}\npub fn committed_next() {}\n",
    )?;
    git(&fixture.primary, &["add", "src/lib.rs"])?;
    git(&fixture.primary, &["commit", "-m", "next commit"])?;
    let next = git_stdout(&fixture.primary, &["rev-parse", "HEAD"])?;
    assert_eq!(search_count(&primary.engine(), "committed_next")?, 1);
    assert_eq!(
        primary
            .engine()
            .snapshot()
            .layers()
            .commit_snapshot
            .reuse
            .origin,
        CommitSnapshotOrigin::ColdBuild
    );

    git(&fixture.linked, &["checkout", "--detach", &next])?;
    assert_eq!(search_count(&linked.engine(), "committed_next")?, 1);
    let snapshot = linked.engine().snapshot();
    assert_eq!(
        snapshot.layers().commit_snapshot.reuse.origin,
        CommitSnapshotOrigin::MemoryReuse
    );
    assert!(snapshot.layers().worktree_overlay.files.is_empty());
    registry.shutdown()?;
    Ok(())
}

#[test]
fn composes_commit_snapshot_overlay_and_rebased_head_atomically()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = LinkedWorktrees::new()?;
    let root = &fixture.primary;
    fs::write(root.join("src/unchanged.rs"), "pub fn unchanged() {}\n")?;
    fs::write(root.join("src/old.rs"), "pub fn renamed_symbol() {}\n")?;
    fs::write(root.join("src/deleted.rs"), "pub fn deleted_symbol() {}\n")?;
    git(
        root,
        &["add", "src/unchanged.rs", "src/old.rs", "src/deleted.rs"],
    )?;
    git(root, &["commit", "-m", "layer base"])?;
    let base = git_stdout(root, &["rev-parse", "HEAD"])?;

    fs::write(
        root.join("src/lib.rs"),
        "pub fn target() {}\npub fn provider_caller() {}\npub fn dirty_symbol() {}\n",
    )?;
    git(root, &["mv", "src/old.rs", "src/renamed.rs"])?;
    fs::write(
        root.join("src/renamed.rs"),
        "pub fn renamed_symbol() {}\npub fn renamed_dirty() {}\n",
    )?;
    fs::remove_file(root.join("src/deleted.rs"))?;
    fs::write(root.join("src/added.rs"), "pub fn added_symbol() {}\n")?;

    let registry = WorkspaceRegistry::new(WorkspaceRegistryConfig { max_workspaces: 1 })?;
    let registered = registry.register(root, WorkspaceStartOptions::default())?;
    let engine = registered.engine();
    let snapshot = engine.snapshot();
    assert_eq!(
        snapshot.layers().commit_snapshot.commit.as_deref(),
        Some(base.as_str())
    );
    assert_eq!(
        snapshot
            .commit_graph()
            .file_source(&chakra_domain::location::RepoRelativePath::new(
                "src/lib.rs"
            )?),
        Some("pub fn target() {}\npub fn provider_caller() {}\n")
    );
    assert!(
        snapshot
            .commit_graph()
            .file_source(&chakra_domain::location::RepoRelativePath::new(
                "src/added.rs"
            )?)
            .is_none()
    );
    assert_eq!(
        exact_symbol(&engine, "unchanged")?.source_layer,
        SourceLayer::CommitSnapshot
    );
    assert_eq!(
        exact_symbol(&engine, "dirty_symbol")?.source_layer,
        SourceLayer::WorktreeOverlay
    );
    assert_eq!(
        exact_symbol(&engine, "added_symbol")?.source_layer,
        SourceLayer::WorktreeOverlay
    );
    assert_eq!(
        exact_symbol(&engine, "renamed_dirty")?.source_layer,
        SourceLayer::WorktreeOverlay
    );
    assert!(
        snapshot
            .layers()
            .worktree_overlay
            .files
            .iter()
            .any(|change| {
                change.change == ChangeKind::Deleted && change.path.as_str() == "src/deleted.rs"
            })
    );
    assert!(
        snapshot
            .layers()
            .worktree_overlay
            .files
            .iter()
            .any(|change| {
                change.change == ChangeKind::Renamed
                    && change.path.as_str() == "src/renamed.rs"
                    && change
                        .previous_path
                        .as_ref()
                        .is_some_and(|path| path.as_str() == "src/old.rs")
            })
    );
    let status = engine.status(StatusRequest)?;
    assert_eq!(
        status.layers.commit_snapshot,
        snapshot.layers().commit_snapshot
    );
    assert_eq!(
        status.layers.worktree_overlay,
        snapshot.layers().worktree_overlay
    );

    git(root, &["add", "-A"])?;
    git(root, &["commit", "-m", "materialize overlay"])?;
    engine.require_fresh()?;
    let committed = engine.snapshot();
    let new_head = git_stdout(root, &["rev-parse", "HEAD"])?;
    assert_ne!(new_head, base);
    assert_eq!(
        committed.layers().commit_snapshot.commit.as_deref(),
        Some(new_head.as_str())
    );
    assert!(committed.layers().worktree_overlay.files.is_empty());
    assert_eq!(
        exact_symbol(&engine, "dirty_symbol")?.source_layer,
        SourceLayer::CommitSnapshot
    );

    registry.shutdown()?;
    Ok(())
}

#[test]
fn rapidly_changing_worktree_eventually_publishes_one_final_composition()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = LinkedWorktrees::new()?;
    let registry = WorkspaceRegistry::new(WorkspaceRegistryConfig { max_workspaces: 1 })?;
    let registered = registry.register(&fixture.primary, WorkspaceStartOptions::default())?;
    let engine = registered.engine();
    let source = fixture.primary.join("src/lib.rs");
    let writer = thread::spawn(move || -> Result<(), std::io::Error> {
        for version in 0..20 {
            fs::write(
                &source,
                format!(
                    "pub fn target() {{}}\npub fn provider_caller() {{}}\npub fn rapid_{version}() {{}}\n"
                ),
            )?;
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    });
    let _ = engine.require_fresh();
    writer.join().map_err(|_| "rapid writer panicked")??;
    engine.require_fresh()?;
    assert_eq!(search_count(&engine, "rapid_19")?, 1);
    assert_eq!(
        exact_symbol(&engine, "rapid_19")?.source_layer,
        SourceLayer::WorktreeOverlay
    );
    let snapshot = engine.snapshot();
    assert!(
        snapshot
            .layers()
            .worktree_overlay
            .files
            .iter()
            .any(|change| {
                change.change == ChangeKind::Modified && change.path.as_str() == "src/lib.rs"
            })
    );
    registry.shutdown()?;
    Ok(())
}

fn search_count(
    engine: &WorkspaceEngine,
    query: &str,
) -> Result<usize, Box<dyn std::error::Error>> {
    let response = engine.search(SearchRequest {
        query: query.to_owned(),
        ..SearchRequest::default()
    })?;
    Ok(response.data.matches.len())
}

fn exact_symbol(
    engine: &WorkspaceEngine,
    query: &str,
) -> Result<chakra_domain::query::SymbolView, Box<dyn std::error::Error>> {
    let response = engine.symbol_search(SymbolSearchRequest {
        query: query.to_owned(),
        match_mode: SymbolMatchMode::Exact,
        ..SymbolSearchRequest::default()
    })?;
    response
        .data
        .candidates
        .into_iter()
        .next()
        .ok_or_else(|| format!("missing symbol {query}").into())
}

struct LinkedWorktrees {
    _temp: TempDir,
    primary: PathBuf,
    linked: PathBuf,
}

impl LinkedWorktrees {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_extra("")
    }

    fn new_with_extra(extra: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let primary = temp.path().join("primary");
        let linked = temp.path().join("linked");
        fs::create_dir_all(primary.join("src"))?;
        git(temp.path(), &["init", "--initial-branch=main", "primary"])?;
        git(&primary, &["config", "user.name", "Chakra Tests"])?;
        git(
            &primary,
            &["config", "user.email", "chakra@example.invalid"],
        )?;
        fs::write(
            primary.join("src/lib.rs"),
            format!("pub fn target() {{}}\npub fn provider_caller() {{}}\n{extra}"),
        )?;
        fs::write(
            primary.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        git(&primary, &["add", "Cargo.toml", "src/lib.rs"])?;
        git(&primary, &["commit", "-m", "initial"])?;
        git(
            &primary,
            &["worktree", "add", "--detach", path_text(&linked)?, "HEAD"],
        )?;
        Ok(Self {
            _temp: temp,
            primary,
            linked,
        })
    }
}

fn git(cwd: &Path, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if output.status.success() {
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    } else {
        Err(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}

fn path_text(path: &Path) -> Result<&str, Box<dyn std::error::Error>> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 test path: {}", path.display()).into())
}

fn artifact_count(cache_dir: &Path) -> Result<usize, Box<dyn std::error::Error>> {
    let entries = cache_dir.join("entries");
    if !entries.is_dir() {
        return Ok(0);
    }
    Ok(fs::read_dir(entries)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .count())
}

fn only_artifact(cache_dir: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let entries = fs::read_dir(cache_dir.join("entries"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    match entries.as_slice() {
        [entry] => Ok(entry.clone()),
        _ => Err(format!("expected one cache artifact, found {}", entries.len()).into()),
    }
}
