//! Bounded filesystem notifications plus deterministic multi-language freshness.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_engine::{FreshnessBarrier, FreshnessBarrierError, SymbolGraph, WorkspaceEngine};
use notify::event::{AccessKind, AccessMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tracing::{error, info, warn};

use crate::indexer::{
    IndexOptions, ReconcileMetrics, ReconcileReport, WorkspaceIndexError, WorkspaceSourceLoader,
    WorkspaceSourceScan, WorkspaceSyntaxIndex, scan_discovered_sources_with_options,
};

const EVENT_QUEUE_CAPACITY: usize = 256;
const DEBOUNCE_QUIET: Duration = Duration::from_millis(50);
const DEBOUNCE_MAX: Duration = Duration::from_millis(250);
const FRESHNESS_TIMEOUT: Duration = Duration::from_secs(30);
const FRESHNESS_CANCELLATION_POLL: Duration = Duration::from_millis(10);
const MAX_STABLE_SCAN_ATTEMPTS: usize = 3;
const MAX_WATCHED_DIRECTORIES: usize = 4_096;
const MAX_PUBLISH_ATTEMPTS: usize = 3;
const MAX_EVENT_HINT_PATHS: usize = 32;
const DEFAULT_FULL_RECONCILE_INTERVAL: u64 = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReconciliationKind {
    #[default]
    None,
    Noop,
    Targeted,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveIndexOptions {
    /// Successful reconciliations between forced content rereads. Metadata
    /// identities and Git inventory are still verified on every barrier.
    pub full_reconcile_interval: u64,
}

impl Default for LiveIndexOptions {
    fn default() -> Self {
        Self {
            full_reconcile_interval: DEFAULT_FULL_RECONCILE_INTERVAL,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveIndexMetrics {
    pub barrier_requests: u64,
    pub barrier_generations_completed: u64,
    pub barrier_waiters_coalesced: u64,
    pub reconciliations: u64,
    pub reconciliation_failures: u64,
    pub published_revisions: u64,
    pub files_scanned: u64,
    pub files_reparsed: u64,
    pub relationship_files_recomputed: u64,
    pub framework_files_reparsed: u64,
    pub framework_relationship_files_recomputed: u64,
    pub framework_truncated_files: u64,
    pub unchanged_files: u64,
    pub created_files: u64,
    pub modified_files: u64,
    pub deleted_files: u64,
    pub syntax_error_files: u64,
    pub graph_files_reused: u64,
    pub graph_files_rebuilt: u64,
    pub graph_source_bytes_reused: u64,
    pub graph_source_bytes_rebuilt: u64,
    pub graph_source_bytes_copied: u64,
    pub graph_symbols_reused: u64,
    pub graph_symbols_rebuilt: u64,
    pub graph_symbols_copied: u64,
    pub graph_edges_reused: u64,
    pub graph_edges_rebuilt: u64,
    pub graph_edges_copied: u64,
    pub graph_call_sites_reused: u64,
    pub graph_call_sites_rebuilt: u64,
    pub graph_call_sites_copied: u64,
    pub watcher_events: u64,
    pub dropped_watcher_events: u64,
    pub watcher_errors: u64,
    pub watched_directories: u64,
    pub watcher_hint_paths: u64,
    pub git_subprocesses: u64,
    pub files_inspected: u64,
    pub source_bytes_inspected: u64,
    pub metadata_files_inspected: u64,
    pub metadata_bytes_inspected: u64,
    pub files_read: u64,
    pub source_bytes_read: u64,
    pub no_op_reconciliations: u64,
    pub targeted_reconciliations: u64,
    pub full_reconciliations: u64,
    pub watch_set_recomputations: u64,
    pub last_reconciliation_kind: ReconciliationKind,
}

#[derive(Debug, Default)]
struct MetricsState {
    barrier_requests: AtomicU64,
    barrier_generations_completed: AtomicU64,
    barrier_waiters_coalesced: AtomicU64,
    reconciliations: AtomicU64,
    reconciliation_failures: AtomicU64,
    published_revisions: AtomicU64,
    files_scanned: AtomicU64,
    files_reparsed: AtomicU64,
    relationship_files_recomputed: AtomicU64,
    framework_files_reparsed: AtomicU64,
    framework_relationship_files_recomputed: AtomicU64,
    framework_truncated_files: AtomicU64,
    unchanged_files: AtomicU64,
    created_files: AtomicU64,
    modified_files: AtomicU64,
    deleted_files: AtomicU64,
    syntax_error_files: AtomicU64,
    graph_files_reused: AtomicU64,
    graph_files_rebuilt: AtomicU64,
    graph_source_bytes_reused: AtomicU64,
    graph_source_bytes_rebuilt: AtomicU64,
    graph_source_bytes_copied: AtomicU64,
    graph_symbols_reused: AtomicU64,
    graph_symbols_rebuilt: AtomicU64,
    graph_symbols_copied: AtomicU64,
    graph_edges_reused: AtomicU64,
    graph_edges_rebuilt: AtomicU64,
    graph_edges_copied: AtomicU64,
    graph_call_sites_reused: AtomicU64,
    graph_call_sites_rebuilt: AtomicU64,
    graph_call_sites_copied: AtomicU64,
    watcher_events: AtomicU64,
    dropped_watcher_events: AtomicU64,
    watcher_errors: AtomicU64,
    watched_directories: AtomicU64,
    watcher_hint_paths: AtomicU64,
    git_subprocesses: AtomicU64,
    files_inspected: AtomicU64,
    source_bytes_inspected: AtomicU64,
    metadata_files_inspected: AtomicU64,
    metadata_bytes_inspected: AtomicU64,
    files_read: AtomicU64,
    source_bytes_read: AtomicU64,
    no_op_reconciliations: AtomicU64,
    targeted_reconciliations: AtomicU64,
    full_reconciliations: AtomicU64,
    watch_set_recomputations: AtomicU64,
    last_reconciliation_kind: AtomicU64,
    event_epoch: AtomicU64,
}

impl MetricsState {
    fn snapshot(&self) -> LiveIndexMetrics {
        let load = |value: &AtomicU64| value.load(Ordering::Relaxed);
        LiveIndexMetrics {
            barrier_requests: load(&self.barrier_requests),
            barrier_generations_completed: load(&self.barrier_generations_completed),
            barrier_waiters_coalesced: load(&self.barrier_waiters_coalesced),
            reconciliations: load(&self.reconciliations),
            reconciliation_failures: load(&self.reconciliation_failures),
            published_revisions: load(&self.published_revisions),
            files_scanned: load(&self.files_scanned),
            files_reparsed: load(&self.files_reparsed),
            relationship_files_recomputed: load(&self.relationship_files_recomputed),
            framework_files_reparsed: load(&self.framework_files_reparsed),
            framework_relationship_files_recomputed: load(
                &self.framework_relationship_files_recomputed,
            ),
            framework_truncated_files: load(&self.framework_truncated_files),
            unchanged_files: load(&self.unchanged_files),
            created_files: load(&self.created_files),
            modified_files: load(&self.modified_files),
            deleted_files: load(&self.deleted_files),
            syntax_error_files: load(&self.syntax_error_files),
            graph_files_reused: load(&self.graph_files_reused),
            graph_files_rebuilt: load(&self.graph_files_rebuilt),
            graph_source_bytes_reused: load(&self.graph_source_bytes_reused),
            graph_source_bytes_rebuilt: load(&self.graph_source_bytes_rebuilt),
            graph_source_bytes_copied: load(&self.graph_source_bytes_copied),
            graph_symbols_reused: load(&self.graph_symbols_reused),
            graph_symbols_rebuilt: load(&self.graph_symbols_rebuilt),
            graph_symbols_copied: load(&self.graph_symbols_copied),
            graph_edges_reused: load(&self.graph_edges_reused),
            graph_edges_rebuilt: load(&self.graph_edges_rebuilt),
            graph_edges_copied: load(&self.graph_edges_copied),
            graph_call_sites_reused: load(&self.graph_call_sites_reused),
            graph_call_sites_rebuilt: load(&self.graph_call_sites_rebuilt),
            graph_call_sites_copied: load(&self.graph_call_sites_copied),
            watcher_events: load(&self.watcher_events),
            dropped_watcher_events: load(&self.dropped_watcher_events),
            watcher_errors: load(&self.watcher_errors),
            watched_directories: load(&self.watched_directories),
            watcher_hint_paths: load(&self.watcher_hint_paths),
            git_subprocesses: load(&self.git_subprocesses),
            files_inspected: load(&self.files_inspected),
            source_bytes_inspected: load(&self.source_bytes_inspected),
            metadata_files_inspected: load(&self.metadata_files_inspected),
            metadata_bytes_inspected: load(&self.metadata_bytes_inspected),
            files_read: load(&self.files_read),
            source_bytes_read: load(&self.source_bytes_read),
            no_op_reconciliations: load(&self.no_op_reconciliations),
            targeted_reconciliations: load(&self.targeted_reconciliations),
            full_reconciliations: load(&self.full_reconciliations),
            watch_set_recomputations: load(&self.watch_set_recomputations),
            last_reconciliation_kind: match load(&self.last_reconciliation_kind) {
                1 => ReconciliationKind::Noop,
                2 => ReconciliationKind::Targeted,
                3 => ReconciliationKind::Full,
                _ => ReconciliationKind::None,
            },
        }
    }

    fn record_reconciliation_kind(&self, kind: ReconciliationKind) {
        let (counter, raw) = match kind {
            ReconciliationKind::None => return,
            ReconciliationKind::Noop => (&self.no_op_reconciliations, 1),
            ReconciliationKind::Targeted => (&self.targeted_reconciliations, 2),
            ReconciliationKind::Full => (&self.full_reconciliations, 3),
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.last_reconciliation_kind.store(raw, Ordering::Relaxed);
    }

    fn record_barrier_completion(&self, covered: u64, completed_before: u64) {
        let completed = covered.saturating_sub(completed_before);
        self.barrier_generations_completed
            .fetch_add(completed, Ordering::Relaxed);
        self.barrier_waiters_coalesced
            .fetch_add(completed.saturating_sub(1), Ordering::Relaxed);
    }

    fn record_reconcile(&self, metrics: ReconcileMetrics) {
        self.reconciliations.fetch_add(1, Ordering::Relaxed);
        self.files_scanned
            .fetch_add(metrics.scanned_files, Ordering::Relaxed);
        self.files_reparsed
            .fetch_add(metrics.reparsed_files, Ordering::Relaxed);
        self.relationship_files_recomputed
            .fetch_add(metrics.relationship_files_recomputed, Ordering::Relaxed);
        self.framework_files_reparsed
            .fetch_add(metrics.framework_files_reparsed, Ordering::Relaxed);
        self.framework_relationship_files_recomputed.fetch_add(
            metrics.framework_relationship_files_recomputed,
            Ordering::Relaxed,
        );
        self.framework_truncated_files
            .store(metrics.framework_truncated_files, Ordering::Relaxed);
        self.unchanged_files
            .fetch_add(metrics.unchanged_files, Ordering::Relaxed);
        self.created_files
            .fetch_add(metrics.created_files, Ordering::Relaxed);
        self.modified_files
            .fetch_add(metrics.modified_files, Ordering::Relaxed);
        self.deleted_files
            .fetch_add(metrics.deleted_files, Ordering::Relaxed);
        self.syntax_error_files
            .store(metrics.syntax_error_files, Ordering::Relaxed);
        let publication = metrics.publication;
        self.graph_files_reused
            .fetch_add(publication.reused_files, Ordering::Relaxed);
        self.graph_files_rebuilt
            .fetch_add(publication.rebuilt_files, Ordering::Relaxed);
        self.graph_source_bytes_reused
            .fetch_add(publication.reused_source_bytes, Ordering::Relaxed);
        self.graph_source_bytes_rebuilt
            .fetch_add(publication.rebuilt_source_bytes, Ordering::Relaxed);
        self.graph_source_bytes_copied
            .fetch_add(publication.copied_source_bytes, Ordering::Relaxed);
        self.graph_symbols_reused
            .fetch_add(publication.reused_symbols, Ordering::Relaxed);
        self.graph_symbols_rebuilt
            .fetch_add(publication.rebuilt_symbols, Ordering::Relaxed);
        self.graph_symbols_copied
            .fetch_add(publication.copied_symbols, Ordering::Relaxed);
        self.graph_edges_reused
            .fetch_add(publication.reused_edges, Ordering::Relaxed);
        self.graph_edges_rebuilt
            .fetch_add(publication.rebuilt_edges, Ordering::Relaxed);
        self.graph_edges_copied
            .fetch_add(publication.copied_edges, Ordering::Relaxed);
        self.graph_call_sites_reused
            .fetch_add(publication.reused_call_sites, Ordering::Relaxed);
        self.graph_call_sites_rebuilt
            .fetch_add(publication.rebuilt_call_sites, Ordering::Relaxed);
        self.graph_call_sites_copied
            .fetch_add(publication.copied_call_sites, Ordering::Relaxed);
    }
}

#[derive(Debug, Error)]
pub enum LiveIndexError {
    #[error("failed to start live index worker: {0}")]
    Thread(#[from] std::io::Error),
    #[error("live index worker failed during startup: {0}")]
    Startup(String),
    #[error("live index worker stopped during startup")]
    StartupDisconnected,
    #[error("workspace freshness owner is already installed")]
    BarrierAlreadyInstalled,
    #[error("full reconciliation interval must be greater than zero")]
    InvalidFullReconcileInterval,
    #[error(transparent)]
    Freshness(#[from] FreshnessBarrierError),
    #[error("live index worker panicked")]
    WorkerPanicked,
}

#[derive(Debug)]
enum WorkerSignal {
    Filesystem {
        epoch: u64,
        hints: Vec<chakra_domain::location::RepoRelativePath>,
        uncertain: bool,
    },
    Barrier,
    Shutdown,
}

#[derive(Debug, Default)]
struct BarrierState {
    requested: u64,
    completed: u64,
    waiters: BTreeMap<u64, OperationContext>,
    outcomes: BTreeMap<u64, Result<(), String>>,
    worker_operation: Option<OperationContext>,
    shutdown: bool,
}

#[derive(Debug)]
struct BarrierShared {
    state: Mutex<BarrierState>,
    completed: Condvar,
}

impl BarrierShared {
    fn pending_generation(&self) -> Result<(u64, u64), String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "freshness state lock is poisoned".to_owned())?;
        let requested = state
            .waiters
            .keys()
            .next_back()
            .copied()
            .unwrap_or(state.completed);
        Ok((requested, state.completed))
    }

    fn register(&self, operation: OperationContext) -> Result<u64, FreshnessBarrierError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| FreshnessBarrierError::new("freshness state lock is poisoned"))?;
        if state.shutdown {
            return Err(FreshnessBarrierError::new("live index worker is stopped"));
        }
        state.requested = state
            .requested
            .checked_add(1)
            .ok_or_else(|| FreshnessBarrierError::new("freshness generation overflow"))?;
        let target = state.requested;
        state.waiters.insert(target, operation);
        Ok(target)
    }

    fn begin_barrier_reconciliation(&self) -> Result<(u64, u64, OperationContext), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "freshness state lock is poisoned".to_owned())?;
        let generation = state
            .waiters
            .keys()
            .next_back()
            .copied()
            .unwrap_or(state.completed);
        let operation = OperationContext::unbounded();
        if !state
            .waiters
            .range((state.completed.saturating_add(1))..=generation)
            .any(|(_, waiter)| waiter.check().is_ok())
        {
            operation.cancel();
        }
        state.worker_operation = Some(operation.clone());
        Ok((generation, state.completed, operation))
    }

    fn complete(&self, generation: u64, result: Result<(), String>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let outcome = result;
        let targets: Vec<_> = state
            .waiters
            .range(..=generation)
            .map(|(target, _)| *target)
            .collect();
        for target in targets {
            state
                .outcomes
                .entry(target)
                .or_insert_with(|| outcome.clone());
        }
        state.completed = state.completed.max(generation);
        state.worker_operation = None;
        self.completed.notify_all();
    }

    fn finish_waiter(&self, target: u64) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.waiters.remove(&target);
        state.outcomes.remove(&target);
        let has_active = state.waiters.iter().any(|(generation, operation)| {
            *generation > state.completed && operation.check().is_ok()
        });
        if !has_active && let Some(operation) = &state.worker_operation {
            operation.cancel();
        }
        self.completed.notify_all();
    }

    fn abandon_worker(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.worker_operation = None;
        self.completed.notify_all();
    }

    fn stop(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.shutdown = true;
        self.completed.notify_all();
    }
}

#[derive(Debug)]
struct LiveFreshnessBarrier {
    shared: Arc<BarrierShared>,
    sender: SyncSender<WorkerSignal>,
    metrics: Arc<MetricsState>,
}

impl FreshnessBarrier for LiveFreshnessBarrier {
    fn require_fresh(&self) -> Result<(), FreshnessBarrierError> {
        self.require_fresh_with_context(&OperationContext::with_timeout(FRESHNESS_TIMEOUT))
    }

    fn require_fresh_with_context(
        &self,
        operation: &OperationContext,
    ) -> Result<(), FreshnessBarrierError> {
        self.metrics
            .barrier_requests
            .fetch_add(1, Ordering::Relaxed);
        let operation = operation.bounded_by(FRESHNESS_TIMEOUT);
        operation
            .check()
            .map_err(|error| FreshnessBarrierError::new(error.to_string()))?;
        let target = self.shared.register(operation.clone())?;
        match self.sender.try_send(WorkerSignal::Barrier) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {
                self.shared.stop();
                self.shared.finish_waiter(target);
                return Err(FreshnessBarrierError::new("live index worker disconnected"));
            }
        }

        let result = (|| {
            let mut state = self
                .shared
                .state
                .lock()
                .map_err(|_| FreshnessBarrierError::new("freshness state lock is poisoned"))?;
            loop {
                operation
                    .check()
                    .map_err(|error| FreshnessBarrierError::new(error.to_string()))?;
                if let Some(outcome) = state.outcomes.get(&target) {
                    return outcome.as_ref().map_or_else(
                        |message| Err(FreshnessBarrierError::new(message.clone())),
                        |_| Ok(()),
                    );
                }
                if state.shutdown {
                    return Err(FreshnessBarrierError::new("live index worker stopped"));
                }
                let wait = operation
                    .poll_timeout(FRESHNESS_CANCELLATION_POLL)
                    .map_err(|error| FreshnessBarrierError::new(error.to_string()))?;
                let (next, _) = self
                    .shared
                    .completed
                    .wait_timeout(state, wait)
                    .map_err(|_| FreshnessBarrierError::new("freshness state lock is poisoned"))?;
                state = next;
            }
        })();
        self.shared.finish_waiter(target);
        result
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.mode(),
            #[cfg(unix)]
            modified_seconds: metadata.mtime(),
            #[cfg(unix)]
            modified_nanoseconds: metadata.mtime_nsec(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    fn trustworthy_for_reuse(&self) -> bool {
        cfg!(unix)
    }
}

#[derive(Debug, Clone)]
struct CachedSource {
    identity: FileIdentity,
    source: Arc<str>,
}

#[derive(Debug, Clone, Default)]
struct SourceSnapshotCache {
    initialized: bool,
    inventory: chakra_git::WorkspaceInventory,
    entries: BTreeMap<chakra_domain::location::RepoRelativePath, CachedSource>,
}

struct CachedSourceLoader<'a> {
    previous: &'a SourceSnapshotCache,
    next: BTreeMap<chakra_domain::location::RepoRelativePath, CachedSource>,
    observed: BTreeMap<chakra_domain::location::RepoRelativePath, FileIdentity>,
    metadata_paths: BTreeSet<chakra_domain::location::RepoRelativePath>,
    force_full: bool,
    files_read: u64,
    metrics: &'a MetricsState,
    operation: &'a OperationContext,
}

impl<'a> CachedSourceLoader<'a> {
    fn new(
        previous: &'a SourceSnapshotCache,
        force_full: bool,
        metrics: &'a MetricsState,
        operation: &'a OperationContext,
    ) -> Self {
        Self {
            previous,
            next: BTreeMap::new(),
            observed: BTreeMap::new(),
            metadata_paths: BTreeSet::new(),
            force_full,
            files_read: 0,
            metrics,
            operation,
        }
    }

    fn inspect(&self, metadata: &fs::Metadata) {
        self.metrics.files_inspected.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .source_bytes_inspected
            .fetch_add(metadata.len(), Ordering::Relaxed);
    }
}

impl WorkspaceSourceLoader for CachedSourceLoader<'_> {
    fn observe(
        &mut self,
        path: &chakra_domain::location::RepoRelativePath,
        metadata: &fs::Metadata,
    ) {
        self.inspect(metadata);
        self.observed
            .insert(path.clone(), FileIdentity::from_metadata(metadata));
    }

    fn observe_metadata(
        &mut self,
        path: &chakra_domain::location::RepoRelativePath,
        metadata: &fs::Metadata,
    ) {
        self.metrics
            .metadata_files_inspected
            .fetch_add(1, Ordering::Relaxed);
        self.metrics
            .metadata_bytes_inspected
            .fetch_add(metadata.len(), Ordering::Relaxed);
        self.observed
            .insert(path.clone(), FileIdentity::from_metadata(metadata));
        self.metadata_paths.insert(path.clone());
    }

    fn load(
        &mut self,
        absolute: &Path,
        path: &chakra_domain::location::RepoRelativePath,
        metadata: &fs::Metadata,
        max_bytes: u64,
    ) -> Result<Arc<str>, WorkspaceIndexError> {
        self.operation
            .check()
            .map_err(|_| WorkspaceIndexError::Cancelled)?;
        let before = FileIdentity::from_metadata(metadata);
        let reused = !self.force_full
            && before.trustworthy_for_reuse()
            && self
                .previous
                .entries
                .get(path)
                .is_some_and(|cached| cached.identity == before);
        let source = if reused {
            self.previous
                .entries
                .get(path)
                .map(|cached| cached.source.clone())
                .ok_or_else(|| {
                    WorkspaceIndexError::Update(format!(
                        "source cache entry disappeared for `{path}`"
                    ))
                })?
        } else {
            let file = fs::File::open(absolute).map_err(|source| WorkspaceIndexError::Read {
                path: path.clone(),
                source,
            })?;
            let mut source = String::new();
            file.take(max_bytes.saturating_add(1))
                .read_to_string(&mut source)
                .map_err(|source| WorkspaceIndexError::Read {
                    path: path.clone(),
                    source,
                })?;
            self.files_read = self.files_read.saturating_add(1);
            self.metrics.files_read.fetch_add(1, Ordering::Relaxed);
            self.metrics
                .source_bytes_read
                .fetch_add(source.len() as u64, Ordering::Relaxed);
            Arc::<str>::from(source)
        };
        self.operation
            .check()
            .map_err(|_| WorkspaceIndexError::Cancelled)?;
        let after = if reused {
            before.clone()
        } else {
            let after_metadata =
                fs::metadata(absolute).map_err(|source| WorkspaceIndexError::Read {
                    path: path.clone(),
                    source,
                })?;
            self.inspect(&after_metadata);
            FileIdentity::from_metadata(&after_metadata)
        };
        if before != after {
            return Err(WorkspaceIndexError::Update(format!(
                "source `{path}` changed while its freshness snapshot was read"
            )));
        }
        self.next.insert(
            path.clone(),
            CachedSource {
                identity: after,
                source: source.clone(),
            },
        );
        Ok(source)
    }
}

/// Owner of the watcher, worker cancellation, and live instrumentation.
#[derive(Debug)]
pub struct LiveIndex {
    sender: SyncSender<WorkerSignal>,
    shared: Arc<BarrierShared>,
    metrics: Arc<MetricsState>,
    worker: Option<JoinHandle<()>>,
}

impl LiveIndex {
    pub fn metrics(&self) -> LiveIndexMetrics {
        self.metrics.snapshot()
    }

    pub fn shutdown(mut self) -> Result<(), LiveIndexError> {
        self.stop_and_join()
    }

    /// Stops the watcher owner, waits for any in-flight reconciliation, and
    /// then returns a stable instrumentation snapshot.
    pub fn shutdown_with_metrics(mut self) -> Result<LiveIndexMetrics, LiveIndexError> {
        self.stop_and_join()?;
        Ok(self.metrics.snapshot())
    }

    fn stop_and_join(&mut self) -> Result<(), LiveIndexError> {
        let _ = self.sender.send(WorkerSignal::Shutdown);
        self.shared.stop();
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| LiveIndexError::WorkerPanicked)?;
        }
        Ok(())
    }
}

impl Drop for LiveIndex {
    fn drop(&mut self) {
        if let Err(error) = self.stop_and_join() {
            error!(%error, "failed to stop live syntax index worker");
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DebounceWindow {
    first: Instant,
    latest: Instant,
}

impl DebounceWindow {
    fn new(now: Instant) -> Self {
        Self {
            first: now,
            latest: now,
        }
    }

    fn observe(&mut self, now: Instant) {
        self.latest = now;
    }

    fn deadline(self) -> Instant {
        (self.latest + DEBOUNCE_QUIET).min(self.first + DEBOUNCE_MAX)
    }
}

/// Starts the owned live pipeline and performs a mandatory reconciliation
/// after the watcher is active, closing the initial-index/startup race.
pub fn start_live_index(
    repository_root: PathBuf,
    syntax_index: WorkspaceSyntaxIndex,
    engine: Arc<WorkspaceEngine>,
) -> Result<LiveIndex, LiveIndexError> {
    start_live_index_with_options(
        repository_root,
        syntax_index,
        engine,
        LiveIndexOptions::default(),
    )
}

pub fn start_live_index_with_options(
    repository_root: PathBuf,
    syntax_index: WorkspaceSyntaxIndex,
    engine: Arc<WorkspaceEngine>,
    options: LiveIndexOptions,
) -> Result<LiveIndex, LiveIndexError> {
    if options.full_reconcile_interval == 0 {
        return Err(LiveIndexError::InvalidFullReconcileInterval);
    }
    let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let shared = Arc::new(BarrierShared {
        state: Mutex::new(BarrierState::default()),
        completed: Condvar::new(),
    });
    let metrics = Arc::new(MetricsState::default());
    let publication_gate = Arc::new(Mutex::new(()));
    let worker_sender = sender.clone();
    let worker_shared = shared.clone();
    let worker_metrics = metrics.clone();
    let worker_engine = engine.clone();
    let worker_publication_gate = publication_gate.clone();
    let worker = thread::Builder::new()
        .name("chakra-live-syntax-index".to_owned())
        .spawn(move || {
            run_worker(
                repository_root,
                syntax_index,
                worker_engine,
                receiver,
                worker_sender,
                worker_shared,
                worker_metrics,
                worker_publication_gate,
                ready_sender,
                options,
            );
        })?;

    match ready_receiver.recv() {
        Ok(Ok(())) => {}
        Ok(Err(message)) => {
            shared.stop();
            let _ = worker.join();
            return Err(LiveIndexError::Startup(message));
        }
        Err(_) => {
            shared.stop();
            let _ = worker.join();
            return Err(LiveIndexError::StartupDisconnected);
        }
    }

    let barrier = Arc::new(LiveFreshnessBarrier {
        shared: shared.clone(),
        sender: sender.clone(),
        metrics: metrics.clone(),
    });
    let engine_barrier: Arc<dyn FreshnessBarrier> = barrier.clone();
    if engine.install_freshness_barrier(engine_barrier).is_err() {
        let mut live = LiveIndex {
            sender,
            shared,
            metrics,
            worker: Some(worker),
        };
        let _ = live.stop_and_join();
        return Err(LiveIndexError::BarrierAlreadyInstalled);
    }
    if let Err(error) = barrier.require_fresh() {
        let mut live = LiveIndex {
            sender,
            shared,
            metrics,
            worker: Some(worker),
        };
        let _ = live.stop_and_join();
        return Err(LiveIndexError::Freshness(error));
    }
    Ok(LiveIndex {
        sender,
        shared,
        metrics,
        worker: Some(worker),
    })
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    repository_root: PathBuf,
    mut syntax_index: WorkspaceSyntaxIndex,
    engine: Arc<WorkspaceEngine>,
    receiver: Receiver<WorkerSignal>,
    sender: SyncSender<WorkerSignal>,
    shared: Arc<BarrierShared>,
    metrics: Arc<MetricsState>,
    publication_gate: Arc<Mutex<()>>,
    ready: SyncSender<Result<(), String>>,
    options: LiveIndexOptions,
) {
    let administrative_paths = match chakra_git::resolve_git_administrative_paths(&repository_root)
    {
        Ok(paths) => paths,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            shared.stop();
            return;
        }
    };
    let callback_sender = sender.clone();
    let callback_metrics = metrics.clone();
    let callback_engine = engine.clone();
    let callback_publication_gate = publication_gate.clone();
    let callback_root = repository_root.clone();
    let watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
        let (hints, uncertain) = match event {
            Ok(event) => {
                callback_metrics
                    .watcher_events
                    .fetch_add(1, Ordering::Relaxed);
                if !event_may_change_workspace(&event, &administrative_paths) {
                    return;
                }
                event_hints(&callback_root, &event)
            }
            Err(error) => {
                callback_metrics
                    .watcher_errors
                    .fetch_add(1, Ordering::Relaxed);
                warn!(%error, "filesystem watcher reported an error");
                (Vec::new(), true)
            }
        };
        callback_metrics
            .watcher_hint_paths
            .fetch_add(hints.len() as u64, Ordering::Relaxed);
        let epoch = invalidate_for_event(
            &callback_engine,
            &callback_metrics,
            &callback_publication_gate,
        );
        match callback_sender.try_send(WorkerSignal::Filesystem {
            epoch,
            hints,
            uncertain,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                callback_metrics
                    .dropped_watcher_events
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    });
    let mut watcher = match watcher {
        Ok(watcher) => watcher,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            shared.stop();
            return;
        }
    };
    let mut watched = BTreeSet::new();
    let initial_paths = syntax_index.paths();
    metrics
        .watch_set_recomputations
        .fetch_add(1, Ordering::Relaxed);
    let mut watcher_degraded = match refresh_watches(
        &mut watcher,
        &repository_root,
        &initial_paths,
        &mut watched,
        &metrics,
        false,
    ) {
        Ok(degraded) => degraded,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            shared.stop();
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        shared.stop();
        return;
    }

    let mut reconciled_event_epoch = 0_u64;
    let mut source_cache = SourceSnapshotCache::default();
    let mut reconciliations_since_full = 0_u64;
    let mut reconciled_watcher_errors = 0_u64;
    let mut reconciled_dropped_events = 0_u64;
    let mut indexed_paths = initial_paths;
    while let Ok(signal) = receiver.recv() {
        match signal {
            WorkerSignal::Shutdown => break,
            WorkerSignal::Barrier => {
                if shared
                    .pending_generation()
                    .is_ok_and(|(requested, completed)| requested > completed)
                {
                    reconcile(
                        &repository_root,
                        &mut syntax_index,
                        &engine,
                        &mut watcher,
                        &mut watched,
                        &metrics,
                        &shared,
                        &mut watcher_degraded,
                        &mut reconciled_event_epoch,
                        &publication_gate,
                        &mut source_cache,
                        &mut reconciliations_since_full,
                        &mut reconciled_watcher_errors,
                        &mut reconciled_dropped_events,
                        &mut indexed_paths,
                        &options,
                        &BTreeSet::new(),
                        false,
                        true,
                    );
                }
            }
            WorkerSignal::Filesystem {
                epoch,
                hints,
                uncertain,
            } => {
                if epoch <= reconciled_event_epoch {
                    continue;
                }
                mark_stale(&engine);
                let mut window = DebounceWindow::new(Instant::now());
                let mut shutdown = false;
                let mut hints: BTreeSet<_> = hints.into_iter().collect();
                let mut latest_signal_epoch = epoch;
                let mut uncertain = uncertain || epoch != reconciled_event_epoch.saturating_add(1);
                loop {
                    let deadline = window.deadline();
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    match receiver.recv_timeout(remaining) {
                        Ok(WorkerSignal::Filesystem {
                            epoch: observed_epoch,
                            hints: observed,
                            uncertain: observed_uncertain,
                        }) => {
                            window.observe(Instant::now());
                            let non_contiguous = event_epoch_is_non_contiguous(
                                &mut latest_signal_epoch,
                                observed_epoch,
                            );
                            uncertain |= observed_uncertain || non_contiguous;
                            for hint in observed {
                                if hints.len() >= MAX_EVENT_HINT_PATHS {
                                    uncertain = true;
                                    break;
                                }
                                hints.insert(hint);
                            }
                        }
                        // The generation counter already records the waiter.
                        // Keep the bounded quiet window open so an editor's
                        // write/metadata/rename burst is reconciled as one
                        // stable state instead of publishing an avoidable
                        // intermediate revision merely because a caller asked
                        // for freshness immediately.
                        Ok(WorkerSignal::Barrier) => {}
                        Ok(WorkerSignal::Shutdown) => {
                            shutdown = true;
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            shutdown = true;
                            break;
                        }
                    }
                }
                if shutdown {
                    break;
                }
                reconcile(
                    &repository_root,
                    &mut syntax_index,
                    &engine,
                    &mut watcher,
                    &mut watched,
                    &metrics,
                    &shared,
                    &mut watcher_degraded,
                    &mut reconciled_event_epoch,
                    &publication_gate,
                    &mut source_cache,
                    &mut reconciliations_since_full,
                    &mut reconciled_watcher_errors,
                    &mut reconciled_dropped_events,
                    &mut indexed_paths,
                    &options,
                    &hints,
                    uncertain,
                    false,
                );
            }
        }

        // A barrier signal may have been coalesced because the bounded queue
        // was full. The generation counter is the durable source of demand.
        if shared
            .pending_generation()
            .is_ok_and(|(requested, completed)| requested > completed)
        {
            reconcile(
                &repository_root,
                &mut syntax_index,
                &engine,
                &mut watcher,
                &mut watched,
                &metrics,
                &shared,
                &mut watcher_degraded,
                &mut reconciled_event_epoch,
                &publication_gate,
                &mut source_cache,
                &mut reconciliations_since_full,
                &mut reconciled_watcher_errors,
                &mut reconciled_dropped_events,
                &mut indexed_paths,
                &options,
                &BTreeSet::new(),
                false,
                true,
            );
        }
    }
    shared.stop();
    info!("live syntax index worker stopped");
}

fn event_may_change_workspace(event: &Event, administrative_paths: &[PathBuf]) -> bool {
    if !event.paths.is_empty()
        && event.paths.iter().all(|path| {
            administrative_paths
                .iter()
                .any(|administrative| path.starts_with(administrative))
        })
    {
        return false;
    }
    match event.kind {
        // Linux inotify reports the indexer's own source reads as open/close
        // access events. Treating them as mutations makes every stable scan
        // invalidate itself. A close-after-write remains a conservative
        // mutation signal in addition to the backend's Modify event.
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        _ => true,
    }
}

fn event_hints(repository_root: &Path, event: &Event) -> (Vec<RepoRelativePath>, bool) {
    let mut hints = Vec::new();
    let mut uncertain = event.paths.is_empty();
    for absolute in event.paths.iter().take(MAX_EVENT_HINT_PATHS) {
        let Ok(relative) = absolute.strip_prefix(repository_root) else {
            uncertain = true;
            continue;
        };
        let Some(relative) = relative.to_str() else {
            uncertain = true;
            continue;
        };
        if chakra_git::source_language(relative).is_none() {
            // Editor atomic-save sequences commonly create, write and rename
            // a temporary non-source path before the source destination is
            // reported. The stable scan always proves the complete Git
            // source/metadata inventory and every retained file identity, so
            // an in-worktree non-source path is not evidence that source
            // events were missed and does not require rereading every body.
            continue;
        }
        match RepoRelativePath::new(relative) {
            Ok(path) => hints.push(path),
            Err(_) => uncertain = true,
        }
    }
    if event.paths.len() > MAX_EVENT_HINT_PATHS {
        uncertain = true;
    }
    hints.sort();
    hints.dedup();
    (hints, uncertain)
}

fn event_epoch_is_non_contiguous(latest: &mut u64, observed: u64) -> bool {
    let non_contiguous = observed != latest.saturating_add(1);
    *latest = (*latest).max(observed);
    non_contiguous
}

fn invalidate_for_event(
    engine: &WorkspaceEngine,
    metrics: &MetricsState,
    publication_gate: &Mutex<()>,
) -> u64 {
    // Serialize event invalidation with fresh publication. Freshness is
    // revoked before the event epoch advances, so an observed epoch can
    // never coexist with an older graph still labeled Fresh.
    let _publication = match publication_gate.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    mark_stale(engine);
    metrics.event_epoch.fetch_add(1, Ordering::AcqRel) + 1
}

#[allow(clippy::too_many_arguments)]
fn reconcile(
    repository_root: &Path,
    syntax_index: &mut WorkspaceSyntaxIndex,
    engine: &WorkspaceEngine,
    watcher: &mut RecommendedWatcher,
    watched: &mut BTreeSet<PathBuf>,
    metrics: &MetricsState,
    shared: &BarrierShared,
    watcher_degraded: &mut bool,
    reconciled_event_epoch: &mut u64,
    publication_gate: &Mutex<()>,
    source_cache: &mut SourceSnapshotCache,
    reconciliations_since_full: &mut u64,
    reconciled_watcher_errors: &mut u64,
    reconciled_dropped_events: &mut u64,
    indexed_paths: &mut Vec<RepoRelativePath>,
    options: &LiveIndexOptions,
    hints: &BTreeSet<RepoRelativePath>,
    uncertain_hint: bool,
    cancel_when_unobserved: bool,
) {
    let pending = shared.pending_generation().unwrap_or((0, 0));
    let (generation, completed_before, operation) = if cancel_when_unobserved {
        let Ok(reconciliation) = shared.begin_barrier_reconciliation() else {
            return;
        };
        reconciliation
    } else {
        (pending.0, pending.1, OperationContext::unbounded())
    };
    let watcher_errors = metrics.watcher_errors.load(Ordering::Acquire);
    let dropped_events = metrics.dropped_watcher_events.load(Ordering::Acquire);
    let watcher_error_advanced = watcher_errors > *reconciled_watcher_errors;
    let dropped_event_advanced = dropped_events > *reconciled_dropped_events;
    let force_full = requires_full_reconciliation(ReconciliationPolicy {
        cache_initialized: source_cache.initialized,
        watcher_health_degraded: *watcher_degraded,
        watcher_error_advanced,
        dropped_event_advanced,
        uncertain_hint,
        reconciliations_since_full: *reconciliations_since_full,
        full_reconcile_interval: options.full_reconcile_interval,
    });
    // A stable partial watch set (for example after the 4,096-directory cap)
    // degrades notification coverage, not authoritative reconciliation.
    // RequireFresh still verifies the complete Git inventory and every file
    // identity, so only a newly observed watcher error requires reinstalling
    // watches and forcing a full body reread.
    let force_reinstall_watches = watcher_error_advanced;
    let mut watch_set_dirty = false;
    let result = (|| {
        for _ in 0..MAX_STABLE_SCAN_ATTEMPTS {
            let stable = stable_scan(
                repository_root,
                syntax_index,
                metrics,
                shared,
                source_cache,
                force_full,
                &operation,
            )?;
            let ReconcileReport {
                graph,
                metrics: reconcile_metrics,
                next_index,
                indexing,
            } = syntax_index
                .reconcile_sources_with_cancellation(stable.scan, &operation.cancellation())?;
            let next_paths: Vec<_> = stable.cache.entries.keys().cloned().collect();
            let paths_changed = *indexed_paths != next_paths;
            if paths_changed || force_reinstall_watches || watch_set_dirty {
                metrics
                    .watch_set_recomputations
                    .fetch_add(1, Ordering::Relaxed);
                // Until this candidate publishes, the external watcher may no
                // longer match `indexed_paths`. A discarded candidate forces
                // another refresh; a failed reconciliation leaves the watcher
                // degraded so the next barrier reinstalls it.
                watch_set_dirty = true;
                *watcher_degraded = refresh_watches(
                    watcher,
                    repository_root,
                    &next_paths,
                    watched,
                    metrics,
                    force_reinstall_watches,
                )?;
            }
            let status = if *watcher_degraded || indexing.is_degraded() {
                WorkspaceStatus::Degraded
            } else {
                WorkspaceStatus::Ready
            };

            let _publication = match publication_gate.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if metrics.event_epoch.load(Ordering::Acquire) != stable.event_epoch {
                continue;
            }
            operation
                .check()
                .map_err(|_| WorkspaceIndexError::Cancelled)?;
            let published = publish_fresh(engine, graph.as_ref(), status, &indexing)
                .map_err(WorkspaceIndexError::Update)?;
            if let Some(next_index) = next_index {
                *syntax_index = next_index;
            }
            *source_cache = stable.cache;
            *indexed_paths = next_paths;
            watch_set_dirty = false;
            if stable.kind == ReconciliationKind::Full {
                *reconciliations_since_full = 0;
            } else {
                *reconciliations_since_full = (*reconciliations_since_full).saturating_add(1);
            }
            // Consume only the uncertainty that this reconciliation observed
            // before it started. Errors or drops racing the scan remain
            // outstanding and force the next reconciliation to be full.
            *reconciled_watcher_errors = watcher_errors;
            *reconciled_dropped_events = dropped_events;
            metrics.record_reconcile(reconcile_metrics);
            metrics.record_reconciliation_kind(stable.kind);
            if published {
                metrics.published_revisions.fetch_add(1, Ordering::Relaxed);
            }
            *reconciled_event_epoch = stable.event_epoch;
            metrics.record_barrier_completion(stable.covered_generation, completed_before);
            info!(
                kind = ?stable.kind,
                hinted_paths = hints.len(),
                force_full,
                files_read = stable.files_read,
                covered_generation = stable.covered_generation,
                "freshness reconciliation completed"
            );
            return Ok(stable.covered_generation);
        }
        Err(WorkspaceIndexError::Update(
            "worktree changed before fresh revision publication".to_owned(),
        ))
    })();
    if watch_set_dirty {
        *watcher_degraded = true;
    }
    match result {
        Ok(completed_generation) => shared.complete(completed_generation, Ok(())),
        Err(WorkspaceIndexError::Cancelled) => {
            shared.abandon_worker();
            info!("freshness reconciliation cancelled before publication");
        }
        Err(error) => {
            metrics
                .reconciliation_failures
                .fetch_add(1, Ordering::Relaxed);
            mark_failed_reconciliation(engine);
            let message = error.to_string();
            error!(%error, "live syntax reconciliation failed");
            shared.complete(generation, Err(message));
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ReconciliationPolicy {
    cache_initialized: bool,
    /// Current watcher coverage may stay degraded after a stable directory
    /// cap. Only a newly advanced error counter is reconciliation
    /// uncertainty; the current health flag remains lifecycle metadata.
    watcher_health_degraded: bool,
    watcher_error_advanced: bool,
    dropped_event_advanced: bool,
    uncertain_hint: bool,
    reconciliations_since_full: u64,
    full_reconcile_interval: u64,
}

fn requires_full_reconciliation(policy: ReconciliationPolicy) -> bool {
    let _watcher_health_degraded = policy.watcher_health_degraded;
    !policy.cache_initialized
        || policy.watcher_error_advanced
        || policy.dropped_event_advanced
        || policy.uncertain_hint
        || policy.reconciliations_since_full >= policy.full_reconcile_interval
}

struct StableSourceSnapshot {
    scan: WorkspaceSourceScan,
    event_epoch: u64,
    covered_generation: u64,
    cache: SourceSnapshotCache,
    kind: ReconciliationKind,
    files_read: u64,
}

fn stable_scan(
    repository_root: &Path,
    syntax_index: &WorkspaceSyntaxIndex,
    metrics: &MetricsState,
    shared: &BarrierShared,
    previous: &SourceSnapshotCache,
    force_full: bool,
    operation: &OperationContext,
) -> Result<StableSourceSnapshot, WorkspaceIndexError> {
    let mut last_error = None;
    let mut attempt_force_full = force_full;
    let mut full_performed = force_full;
    let mut retry_cache = None;
    let mut files_read = 0_u64;
    let initial_watcher_errors = metrics.watcher_errors.load(Ordering::Acquire);
    let initial_dropped_events = metrics.dropped_watcher_events.load(Ordering::Acquire);
    for _ in 0..MAX_STABLE_SCAN_ATTEMPTS {
        operation
            .check()
            .map_err(|_| WorkspaceIndexError::Cancelled)?;
        let epoch = metrics.event_epoch.load(Ordering::Acquire);
        let inventory_started = Instant::now();
        metrics.git_subprocesses.fetch_add(1, Ordering::Relaxed);
        let inventory = match chakra_git::discover_workspace_inventory_in_worktree_with_context(
            repository_root,
            operation,
        ) {
            Ok(inventory) => inventory,
            Err(error) => {
                last_error = Some(WorkspaceIndexError::Discovery(error));
                attempt_force_full = true;
                full_performed = true;
                continue;
            }
        };
        let inventory_elapsed = inventory_started.elapsed();
        let inventory_changed = !previous.initialized || previous.inventory != inventory;
        let scan_options = IndexOptions::new(syntax_index.budgets(), operation.cancellation())?;
        let cache = retry_cache.as_ref().unwrap_or(previous);
        let mut loader = CachedSourceLoader::new(cache, attempt_force_full, metrics, operation);
        let scan_result = scan_discovered_sources_with_options(
            repository_root,
            &scan_options,
            &inventory,
            inventory_elapsed,
            &mut loader,
            operation,
        );
        files_read = files_read.saturating_add(loader.files_read);
        let scan = match scan_result {
            Ok(scan) => scan,
            Err(error) => {
                last_error = Some(error);
                attempt_force_full = true;
                full_performed = true;
                continue;
            }
        };
        let covered_generation = shared
            .pending_generation()
            .map_or(0, |(requested, _)| requested);

        metrics.git_subprocesses.fetch_add(1, Ordering::Relaxed);
        let verified_inventory =
            match chakra_git::discover_workspace_inventory_in_worktree_with_context(
                repository_root,
                operation,
            ) {
                Ok(inventory) => inventory,
                Err(error) => {
                    last_error = Some(WorkspaceIndexError::Discovery(error));
                    attempt_force_full = true;
                    full_performed = true;
                    continue;
                }
            };
        if verified_inventory != inventory {
            last_error = Some(WorkspaceIndexError::Update(
                "Git source/metadata inventory changed during freshness reconciliation".to_owned(),
            ));
            attempt_force_full = true;
            full_performed = true;
            continue;
        }

        let mut identities_match = true;
        for (path, expected) in &loader.observed {
            operation
                .check()
                .map_err(|_| WorkspaceIndexError::Cancelled)?;
            let absolute = repository_root.join(path.as_str());
            match fs::metadata(&absolute) {
                Ok(metadata) => {
                    if loader.metadata_paths.contains(path) {
                        metrics
                            .metadata_files_inspected
                            .fetch_add(1, Ordering::Relaxed);
                        metrics
                            .metadata_bytes_inspected
                            .fetch_add(metadata.len(), Ordering::Relaxed);
                    } else {
                        metrics.files_inspected.fetch_add(1, Ordering::Relaxed);
                        metrics
                            .source_bytes_inspected
                            .fetch_add(metadata.len(), Ordering::Relaxed);
                    }
                    if FileIdentity::from_metadata(&metadata) != *expected {
                        identities_match = false;
                        break;
                    }
                }
                Err(error) => {
                    last_error = Some(WorkspaceIndexError::Read {
                        path: path.clone(),
                        source: error,
                    });
                    identities_match = false;
                    break;
                }
            }
        }
        if !identities_match || epoch != metrics.event_epoch.load(Ordering::Acquire) {
            // A normal watcher event racing this snapshot does not make the
            // retained cache untrustworthy: the next pass rechecks every
            // identity and rereads only bodies whose identity changed. Full
            // rereads are reserved for explicit watcher/inventory uncertainty.
            // Retain the bodies and identities just observed. A retry still
            // checks them all against the filesystem, but a delayed event
            // whose state was already captured no longer causes a duplicate
            // body read. If the file changed again, the identity comparison
            // below the loader makes that retry read it normally.
            let new_watcher_uncertainty = metrics.watcher_errors.load(Ordering::Acquire)
                > initial_watcher_errors
                || metrics.dropped_watcher_events.load(Ordering::Acquire) > initial_dropped_events;
            if new_watcher_uncertainty {
                attempt_force_full = true;
                full_performed = true;
                retry_cache = None;
            } else {
                let entries = std::mem::take(&mut loader.next);
                drop(loader);
                retry_cache = Some(SourceSnapshotCache {
                    initialized: true,
                    inventory,
                    entries,
                });
                attempt_force_full = false;
            }
            continue;
        }
        let kind = if full_performed {
            ReconciliationKind::Full
        } else if inventory_changed || files_read != 0 {
            ReconciliationKind::Targeted
        } else {
            ReconciliationKind::Noop
        };
        return Ok(StableSourceSnapshot {
            scan,
            event_epoch: epoch,
            covered_generation,
            cache: SourceSnapshotCache {
                initialized: true,
                inventory,
                entries: loader.next,
            },
            kind,
            files_read,
        });
    }
    Err(last_error.unwrap_or_else(|| {
        WorkspaceIndexError::Update(
            "worktree kept changing during freshness reconciliation".to_owned(),
        )
    }))
}

fn desired_watch_directories(
    repository_root: &Path,
    indexed_paths: &[RepoRelativePath],
) -> (BTreeSet<PathBuf>, bool) {
    // FSEvents observes directory trees natively. Register one recursive
    // watch per indexed top-level source root plus a non-recursive repository
    // watch. This avoids both redundant ancestor streams and broad recursive
    // observation of unrelated generated/vendor trees.
    if cfg!(target_os = "macos") {
        let mut desired = BTreeSet::from([repository_root.to_path_buf()]);
        for path in indexed_paths {
            let Some(top_level) = Path::new(path.as_str()).components().next() else {
                continue;
            };
            let directory = repository_root.join(top_level);
            if directory.is_dir() {
                desired.insert(directory);
            }
        }
        let truncated = desired.len() > MAX_WATCHED_DIRECTORIES;
        if truncated {
            desired = desired.into_iter().take(MAX_WATCHED_DIRECTORIES).collect();
        }
        return (desired, truncated);
    }
    let mut desired = BTreeSet::from([repository_root.to_path_buf()]);
    for path in indexed_paths {
        if let Some(parent) = Path::new(path.as_str()).parent() {
            let mut directory = repository_root.join(parent);
            while directory != repository_root {
                if directory.is_dir() {
                    desired.insert(directory.clone());
                }
                let Some(next) = directory.parent() else {
                    break;
                };
                if !next.starts_with(repository_root) {
                    break;
                }
                directory = next.to_path_buf();
            }
        }
    }
    let truncated = desired.len() > MAX_WATCHED_DIRECTORIES;
    if truncated {
        desired = desired.into_iter().take(MAX_WATCHED_DIRECTORIES).collect();
    }
    (desired, truncated)
}

fn refresh_watches(
    watcher: &mut RecommendedWatcher,
    repository_root: &Path,
    indexed_paths: &[RepoRelativePath],
    watched: &mut BTreeSet<PathBuf>,
    metrics: &MetricsState,
    force_reinstall: bool,
) -> Result<bool, WorkspaceIndexError> {
    let (desired, mut degraded) = desired_watch_directories(repository_root, indexed_paths);
    if degraded {
        warn!(
            maximum = MAX_WATCHED_DIRECTORIES,
            "watch directory bound reached; notifications are partial and freshness barriers remain authoritative"
        );
    }
    let removed = if force_reinstall {
        std::mem::take(watched).into_iter().collect::<Vec<_>>()
    } else {
        watched.difference(&desired).cloned().collect::<Vec<_>>()
    };
    for directory in removed {
        if let Err(error) = watcher.unwatch(&directory) {
            metrics.watcher_errors.fetch_add(1, Ordering::Relaxed);
            warn!(path = %directory.display(), %error, "failed to remove filesystem watch");
            degraded = true;
        }
        watched.remove(&directory);
    }
    for directory in desired.difference(watched).cloned().collect::<Vec<_>>() {
        let recursive_mode = if cfg!(target_os = "macos") && directory != repository_root {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        match watcher.watch(&directory, recursive_mode) {
            Ok(()) => {
                watched.insert(directory);
            }
            Err(error) if directory == repository_root => {
                return Err(WorkspaceIndexError::Update(format!(
                    "failed to watch repository root {}: {error}",
                    repository_root.display()
                )));
            }
            Err(error) => {
                metrics.watcher_errors.fetch_add(1, Ordering::Relaxed);
                warn!(path = %directory.display(), %error, "failed to watch source directory");
                degraded = true;
            }
        }
    }
    metrics
        .watched_directories
        .store(watched.len() as u64, Ordering::Relaxed);
    Ok(degraded)
}

fn publish_fresh(
    engine: &WorkspaceEngine,
    graph: Option<&SymbolGraph>,
    status: WorkspaceStatus,
    indexing: &chakra_domain::indexing::IndexingStatus,
) -> Result<bool, String> {
    let started = Instant::now();
    let current = engine.snapshot();
    if graph.is_none()
        && current.freshness() == Freshness::Fresh
        && current.status() == status
        && current.indexing() == indexing
    {
        return Ok(false);
    }
    for _ in 0..MAX_PUBLISH_ATTEMPTS {
        let mut update = engine.begin_update();
        if let Some(graph) = graph {
            update.replace_graph(graph.clone());
        }
        update.set_indexing(indexing.clone());
        update.set_status(status);
        update.set_freshness(Freshness::Fresh);
        match engine.publish(update) {
            Ok(snapshot) => {
                info!(
                    revision = snapshot.revision().0,
                    graph_changed = graph.is_some(),
                    indexing_degraded = indexing.is_degraded(),
                    structurally_incremental = indexing.publication.structurally_incremental,
                    graph_files_reused = indexing.publication.reused_files,
                    graph_files_rebuilt = indexing.publication.rebuilt_files,
                    graph_symbols_reused = indexing.publication.reused_symbols,
                    graph_symbols_rebuilt = indexing.publication.rebuilt_symbols,
                    graph_edges_reused = indexing.publication.reused_edges,
                    graph_edges_rebuilt = indexing.publication.rebuilt_edges,
                    graph_call_sites_reused = indexing.publication.reused_call_sites,
                    graph_call_sites_rebuilt = indexing.publication.rebuilt_call_sites,
                    elapsed_micros = started.elapsed().as_micros(),
                    "live syntax revision publication completed"
                );
                return Ok(true);
            }
            Err(error) => warn!(%error, "retrying conflicted live index publication"),
        }
    }
    Err("live index publication repeatedly conflicted".to_owned())
}

fn mark_stale(engine: &WorkspaceEngine) {
    for _ in 0..MAX_PUBLISH_ATTEMPTS {
        let snapshot = engine.snapshot();
        if snapshot.freshness() == Freshness::Stale {
            return;
        }
        let mut update = engine.begin_update();
        update.set_status(WorkspaceStatus::Stale);
        update.set_freshness(Freshness::Stale);
        if engine.publish(update).is_ok() {
            return;
        }
    }
}

fn mark_failed_reconciliation(engine: &WorkspaceEngine) {
    for _ in 0..MAX_PUBLISH_ATTEMPTS {
        let mut update = engine.begin_update();
        update.set_status(WorkspaceStatus::Degraded);
        update.set_freshness(Freshness::Stale);
        if engine.publish(update).is_ok() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chakra_domain::identity::WorkspaceIdentity;
    use notify::event::{DataChange, ModifyKind, RenameMode};

    #[test]
    fn debounce_has_quiet_and_absolute_bounds_without_sleeping() {
        let start = Instant::now();
        let mut window = DebounceWindow::new(start);
        assert_eq!(window.deadline(), start + DEBOUNCE_QUIET);
        window.observe(start + Duration::from_millis(40));
        assert_eq!(window.deadline(), start + Duration::from_millis(90));
        window.observe(start + DEBOUNCE_MAX);
        assert_eq!(window.deadline(), start + DEBOUNCE_MAX);
    }

    #[test]
    fn event_epoch_advances_only_after_freshness_is_revoked()
    -> Result<(), Box<dyn std::error::Error>> {
        let identity = WorkspaceIdentity::for_primary_worktree(Path::new("."))?;
        let engine = WorkspaceEngine::new(identity);
        let mut update = engine.begin_update();
        update.set_status(WorkspaceStatus::Ready);
        update.set_freshness(Freshness::Fresh);
        engine.publish(update)?;
        let metrics = MetricsState::default();
        let publication_gate = Mutex::new(());

        assert_eq!(
            invalidate_for_event(&engine, &metrics, &publication_gate),
            1
        );
        assert_eq!(metrics.event_epoch.load(Ordering::Acquire), 1);
        assert_eq!(engine.snapshot().freshness(), Freshness::Stale);
        Ok(())
    }

    #[test]
    fn watcher_access_reads_do_not_invalidate_stable_scans() {
        let opened = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Any)));
        let read = Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read)));
        let write = Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write)));
        let modified = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)));

        assert!(!event_may_change_workspace(&opened, &[]));
        assert!(!event_may_change_workspace(&read, &[]));
        assert!(event_may_change_workspace(&write, &[]));
        assert!(event_may_change_workspace(&modified, &[]));
        assert!(event_may_change_workspace(&Event::new(EventKind::Any), &[]));
    }

    #[test]
    fn git_administration_events_are_not_workspace_mutations() {
        let root = PathBuf::from("/worktree");
        let administrative = root.join("linked-admin");
        let event = Event::new(EventKind::Any).add_path(administrative.join("index.lock"));
        assert!(!event_may_change_workspace(&event, &[administrative]));
    }

    #[test]
    fn atomic_save_temp_paths_keep_the_source_hint_targeted()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = PathBuf::from("/worktree");
        let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(root.join("src/item.rs.chakra-replacement"))
            .add_path(root.join("src/item.rs"));

        let (hints, uncertain) = event_hints(&root, &event);

        assert_eq!(hints, [RepoRelativePath::new("src/item.rs")?]);
        assert!(!uncertain);
        Ok(())
    }

    #[test]
    fn full_reconciliation_policy_covers_uncertainty_and_checkpoints() {
        let baseline = ReconciliationPolicy {
            cache_initialized: true,
            watcher_health_degraded: false,
            watcher_error_advanced: false,
            dropped_event_advanced: false,
            uncertain_hint: false,
            reconciliations_since_full: 1,
            full_reconcile_interval: 256,
        };
        assert!(!requires_full_reconciliation(baseline));
        assert!(requires_full_reconciliation(ReconciliationPolicy {
            cache_initialized: false,
            ..baseline
        }));
        assert!(!requires_full_reconciliation(ReconciliationPolicy {
            watcher_health_degraded: true,
            ..baseline
        }));
        assert!(requires_full_reconciliation(ReconciliationPolicy {
            watcher_error_advanced: true,
            ..baseline
        }));
        assert!(requires_full_reconciliation(ReconciliationPolicy {
            dropped_event_advanced: true,
            ..baseline
        }));
        assert!(requires_full_reconciliation(ReconciliationPolicy {
            uncertain_hint: true,
            ..baseline
        }));
        assert!(requires_full_reconciliation(ReconciliationPolicy {
            reconciliations_since_full: 256,
            ..baseline
        }));
    }

    #[test]
    fn event_epoch_sequence_marks_gaps_and_reordering_as_uncertain() {
        let mut latest = 7;
        assert!(!event_epoch_is_non_contiguous(&mut latest, 8));
        assert!(event_epoch_is_non_contiguous(&mut latest, 10));
        assert_eq!(latest, 10);
        assert!(event_epoch_is_non_contiguous(&mut latest, 9));
        assert_eq!(latest, 10);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fsevents_coalesces_nested_sources_into_top_level_watches()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path();
        std::fs::create_dir(root.join("src"))?;
        let paths = [
            RepoRelativePath::new("src/main/java/example/Main.java")?,
            RepoRelativePath::new("src/test/java/example/MainTest.java")?,
        ];

        let (desired, truncated) = desired_watch_directories(root, &paths);

        assert_eq!(
            desired,
            BTreeSet::from([root.to_path_buf(), root.join("src")])
        );
        assert!(!truncated);
        Ok(())
    }

    #[test]
    fn one_completed_generation_releases_all_covered_waiters()
    -> Result<(), Box<dyn std::error::Error>> {
        const WAITERS: u64 = 4;
        let (sender, receiver) = mpsc::sync_channel(WAITERS as usize);
        let shared = Arc::new(BarrierShared {
            state: Mutex::new(BarrierState::default()),
            completed: Condvar::new(),
        });
        let metrics = Arc::new(MetricsState::default());
        let barrier = Arc::new(LiveFreshnessBarrier {
            shared: shared.clone(),
            sender,
            metrics: metrics.clone(),
        });
        let waiters: Vec<_> = (0..WAITERS)
            .map(|_| {
                let barrier = barrier.clone();
                thread::spawn(move || barrier.require_fresh())
            })
            .collect();

        for _ in 0..WAITERS {
            assert!(matches!(receiver.recv()?, WorkerSignal::Barrier));
        }
        assert_eq!(shared.pending_generation()?, (WAITERS, 0));
        shared.complete(WAITERS, Ok(()));
        metrics.record_barrier_completion(WAITERS, 0);
        for waiter in waiters {
            waiter.join().map_err(|_| "waiter panicked")??;
        }
        assert_eq!(metrics.barrier_requests.load(Ordering::Relaxed), WAITERS);
        assert_eq!(
            metrics
                .barrier_generations_completed
                .load(Ordering::Relaxed),
            WAITERS
        );
        assert_eq!(
            metrics.barrier_waiters_coalesced.load(Ordering::Relaxed),
            WAITERS - 1
        );
        Ok(())
    }

    #[test]
    fn later_success_does_not_overwrite_an_earlier_generation_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let shared = BarrierShared {
            state: Mutex::new(BarrierState::default()),
            completed: Condvar::new(),
        };
        let first = shared.register(OperationContext::unbounded())?;
        let second = shared.register(OperationContext::unbounded())?;
        shared.complete(first, Err("first failed".to_owned()));
        shared.complete(second, Ok(()));

        let state = shared.state.lock().map_err(|_| "barrier lock poisoned")?;
        assert_eq!(
            state.outcomes.get(&first),
            Some(&Err("first failed".to_owned()))
        );
        assert_eq!(state.outcomes.get(&second), Some(&Ok(())));
        Ok(())
    }

    #[test]
    fn cancelling_the_last_waiter_cancels_barrier_only_reconciliation()
    -> Result<(), Box<dyn std::error::Error>> {
        let shared = BarrierShared {
            state: Mutex::new(BarrierState::default()),
            completed: Condvar::new(),
        };
        let waiter = OperationContext::unbounded();
        let target = shared.register(waiter.clone())?;
        let (_, _, worker) = shared.begin_barrier_reconciliation()?;
        assert!(worker.check().is_ok());

        waiter.cancel();
        shared.finish_waiter(target);
        assert!(worker.check().is_err());
        assert_eq!(shared.pending_generation()?, (0, 0));
        Ok(())
    }
}
