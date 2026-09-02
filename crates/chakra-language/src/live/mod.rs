//! Bounded filesystem notifications plus deterministic multi-language freshness.

mod barrier;
mod metrics;
mod source_cache;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(test)]
use chakra_domain::indexing::MAX_FILE_INVALIDATION_RECORDS;
use chakra_domain::indexing::{
    CacheHealth, FileInvalidation, FileInvalidationReason, FullReconciliationReason,
    IndexPublicationMetrics, IndexingDiagnostics, LiveQueueState, ReconciliationCounters,
    ReconciliationKind, SYNTAX_FACT_CACHE_DISABLED_REASON,
};
use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;
use chakra_domain::query::DiffScope;
use chakra_domain::revision::Revision;
use chakra_domain::scheduling::WorkClass;
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_engine::{
    DiffWorkspace, FreshnessBarrier, FreshnessBarrierError, SymbolGraph, WorkspaceDiff,
    WorkspaceDiffProvider, WorkspaceEngine, source_layers_for_graph, workspace_graph_layers,
};
#[cfg(not(target_os = "macos"))]
use notify::RecommendedWatcher;
use notify::event::{AccessKind, AccessMode};
#[cfg(target_os = "macos")]
use notify::{Config as NotifyConfig, PollWatcher};
use notify::{ErrorKind as NotifyErrorKind, Event, EventKind, RecursiveMode, Watcher};
use thiserror::Error;
use tracing::{error, info, warn};

use crate::indexer::{
    CommitIndexProvider, IndexOptions, ReconcileReport, WorkspaceIndexError, WorkspaceSourceScan,
    WorkspaceSyntaxIndex, index_commit_with_options, scan_discovered_sources_with_options,
};
use crate::scheduler::{PriorityWorkQueue, QueueFull};

use barrier::{BarrierShared, BarrierState, LiveFreshnessBarrier};
use metrics::{FileInvalidationBatch, MetricsState};
use source_cache::{CachedSourceLoader, FileIdentity, SourceSnapshotCache};

pub use metrics::LiveIndexMetrics;

const EVENT_QUEUE_CAPACITY: usize = 256;
const DEBOUNCE_QUIET: Duration = Duration::from_millis(50);
const DEBOUNCE_MAX: Duration = Duration::from_millis(250);
pub(crate) const FRESHNESS_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const FRESHNESS_CANCELLATION_POLL: Duration = Duration::from_millis(10);
const MAX_STABLE_SCAN_ATTEMPTS: usize = 3;
const MAX_WATCHED_DIRECTORIES: usize = 4_096;
const MAX_PUBLISH_ATTEMPTS: usize = 3;
const MAX_EVENT_HINT_PATHS: usize = 32;
const DEFAULT_FULL_RECONCILE_INTERVAL: u64 = 256;
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Staged entries of one class never exceed the bounded transport channel.
const QUEUE_PER_CLASS_CAPACITY: usize = EVENT_QUEUE_CAPACITY;
/// A scheduling pass stages at most one transport-channel capacity before
/// selecting work. A continuously replenished channel therefore cannot keep
/// already staged work from reaching the aging scheduler.
const MAX_TRANSPORT_DRAIN_PER_PASS: usize = EVENT_QUEUE_CAPACITY;
/// One aging interval: queued background work is promoted one class per
/// interval until it competes with freshness work FIFO (issue #44).
const QUEUE_AGING_AFTER: Duration = Duration::from_secs(1);
#[cfg(target_os = "macos")]
const WATCHER_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[cfg(not(target_os = "macos"))]
type WorkspaceWatcher = RecommendedWatcher;
#[cfg(target_os = "macos")]
type WorkspaceWatcher = PollWatcher;

#[cfg(not(target_os = "macos"))]
fn new_workspace_watcher(
    event_handler: impl notify::EventHandler,
) -> notify::Result<WorkspaceWatcher> {
    notify::recommended_watcher(event_handler)
}

#[cfg(target_os = "macos")]
fn new_workspace_watcher(
    event_handler: impl notify::EventHandler,
) -> notify::Result<WorkspaceWatcher> {
    PollWatcher::new(
        event_handler,
        NotifyConfig::default().with_poll_interval(WATCHER_POLL_INTERVAL),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveIndexOptions {
    /// Successful reconciliations between forced content rereads. Metadata
    /// identities and Git inventory are still verified on every barrier.
    pub full_reconcile_interval: u64,
    /// Upper bound for watcher construction and initial watch registration.
    pub startup_timeout: Duration,
}

impl Default for LiveIndexOptions {
    fn default() -> Self {
        Self {
            full_reconcile_interval: DEFAULT_FULL_RECONCILE_INTERVAL,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
        }
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
    #[error("live index worker did not become ready within {timeout:?}")]
    StartupTimeout { timeout: Duration },
    #[error("workspace freshness owner is already installed")]
    BarrierAlreadyInstalled,
    #[error("full reconciliation interval must be greater than zero")]
    InvalidFullReconcileInterval,
    #[error("live index startup timeout must be greater than zero")]
    InvalidStartupTimeout,
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
        enqueued: Instant,
    },
    /// Deferred periodic full reread: background `Reconciliation`-class work
    /// that must never delay a freshness edit (issue #44).
    Checkpoint {
        enqueued: Instant,
    },
    Barrier {
        enqueued: Instant,
    },
    Shutdown,
}

impl WorkerSignal {
    fn class(&self) -> WorkClass {
        match self {
            // Shutdown bypasses the queue entirely; the class only guards
            // against future callers staging it.
            Self::Filesystem { .. } | Self::Barrier { .. } | Self::Shutdown => {
                WorkClass::FreshnessEdit
            }
            Self::Checkpoint { .. } => WorkClass::Reconciliation,
        }
    }

    fn enqueued(&self) -> Instant {
        match self {
            Self::Filesystem { enqueued, .. }
            | Self::Checkpoint { enqueued }
            | Self::Barrier { enqueued } => *enqueued,
            Self::Shutdown => Instant::now(),
        }
    }
}

/// Live diagnostics owner installed on the engine (issue #43).
///
/// Reports the cross-revision operational picture: reconcile counters,
/// full-reconciliation causes, bounded per-file invalidation records, queue
/// state, and honest cache health. `counters.cold_builds` stays zero here;
/// the engine merges its own publication-observed count when serving
/// `status`.
#[derive(Debug)]
struct LiveIndexDiagnostics {
    metrics: Arc<MetricsState>,
    shared: Arc<BarrierShared>,
}

impl LiveIndexDiagnostics {
    fn index_diagnostics(&self) -> IndexingDiagnostics {
        let metrics = self.metrics.snapshot();
        let (requested, completed) = self.shared.pending_generation().unwrap_or((0, 0));
        IndexingDiagnostics {
            cache: CacheHealth::Disabled {
                reason: SYNTAX_FACT_CACHE_DISABLED_REASON.to_owned(),
            },
            counters: ReconciliationCounters {
                cold_builds: 0,
                no_op_reconciliations: metrics.no_op_reconciliations,
                targeted_reconciliations: metrics.targeted_reconciliations,
                full_reconciliations: metrics.full_reconciliations,
                one_file_edits: self.metrics.one_file_edits.load(Ordering::Relaxed),
                reconciliation_failures: metrics.reconciliation_failures,
                published_revisions: metrics.published_revisions,
            },
            queue: LiveQueueState {
                requested_barrier_generation: requested,
                completed_barrier_generation: completed,
                barrier_requests: metrics.barrier_requests,
                barrier_waiters_coalesced: metrics.barrier_waiters_coalesced,
                watcher_events: metrics.watcher_events,
                dropped_watcher_events: metrics.dropped_watcher_events,
                watcher_errors: metrics.watcher_errors,
                watched_directories: metrics.watched_directories,
                watcher_event_queue_capacity: EVENT_QUEUE_CAPACITY as u64,
                scheduled_work: metrics.queue,
            },
            project_invalidations: self.metrics.project_invalidation_diagnostics(),
            last_reconciliation_kind: metrics.last_reconciliation_kind,
            full_reconciliation_reasons: self.metrics.full_reconciliation_reason_counts(),
            last_full_reconciliation_reasons: self.metrics.last_full_reconciliation_reasons(),
            recent_file_invalidations: self.metrics.recent_file_invalidations(),
            file_invalidation_records: self
                .metrics
                .file_invalidation_records
                .load(Ordering::Relaxed),
        }
    }
}

/// Owner of the watcher, worker cancellation, and live instrumentation.
#[derive(Debug)]
pub struct LiveIndex {
    sender: SyncSender<WorkerSignal>,
    shared: Arc<BarrierShared>,
    metrics: Arc<MetricsState>,
    diagnostics: Arc<LiveIndexDiagnostics>,
    worker: Option<JoinHandle<()>>,
}

impl LiveIndex {
    pub fn metrics(&self) -> LiveIndexMetrics {
        self.metrics.snapshot()
    }

    /// Bounded, typed, source-content-free live indexing diagnostics
    /// (issue #43). `counters.cold_builds` is engine-observed and therefore
    /// zero in this direct snapshot; the `status` query merges it.
    pub fn diagnostics(&self) -> IndexingDiagnostics {
        self.diagnostics.index_diagnostics()
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
    start_live_index_with_options_and_commit_provider(
        repository_root,
        syntax_index,
        engine,
        options,
        None,
    )
}

/// Starts the live owner with repository-scoped commit reuse for `HEAD`
/// transitions. The provider is never consulted for ordinary file edits.
pub fn start_live_index_with_options_and_commit_provider(
    repository_root: PathBuf,
    syntax_index: WorkspaceSyntaxIndex,
    engine: Arc<WorkspaceEngine>,
    options: LiveIndexOptions,
    commit_provider: Option<Arc<dyn CommitIndexProvider>>,
) -> Result<LiveIndex, LiveIndexError> {
    if options.full_reconcile_interval == 0 {
        return Err(LiveIndexError::InvalidFullReconcileInterval);
    }
    if options.startup_timeout.is_zero() {
        return Err(LiveIndexError::InvalidStartupTimeout);
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
                commit_provider,
            );
        })?;

    let worker = await_worker_startup(
        ready_receiver,
        &sender,
        &shared,
        worker,
        options.startup_timeout,
    )?;

    let diagnostics = Arc::new(LiveIndexDiagnostics {
        metrics: metrics.clone(),
        shared: shared.clone(),
    });
    let barrier = Arc::new(LiveFreshnessBarrier {
        shared: shared.clone(),
        sender: sender.clone(),
        metrics: metrics.clone(),
        diagnostics: diagnostics.clone(),
    });
    let engine_barrier: Arc<dyn FreshnessBarrier> = barrier.clone();
    if engine.install_freshness_barrier(engine_barrier).is_err() {
        let mut live = LiveIndex {
            sender,
            shared,
            metrics,
            diagnostics,
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
            diagnostics,
            worker: Some(worker),
        };
        let _ = live.stop_and_join();
        return Err(LiveIndexError::Freshness(error));
    }
    Ok(LiveIndex {
        sender,
        shared,
        metrics,
        diagnostics,
        worker: Some(worker),
    })
}

fn await_worker_startup(
    ready: Receiver<Result<(), String>>,
    sender: &SyncSender<WorkerSignal>,
    shared: &Arc<BarrierShared>,
    worker: JoinHandle<()>,
    timeout: Duration,
) -> Result<JoinHandle<()>, LiveIndexError> {
    let failure = match ready.recv_timeout(timeout) {
        Ok(Ok(())) => return Ok(worker),
        Ok(Err(message)) => LiveIndexError::Startup(message),
        Err(mpsc::RecvTimeoutError::Timeout) => LiveIndexError::StartupTimeout { timeout },
        Err(mpsc::RecvTimeoutError::Disconnected) => LiveIndexError::StartupDisconnected,
    };
    let _ = sender.try_send(WorkerSignal::Shutdown);
    shared.stop();
    worker.join().map_err(|_| LiveIndexError::WorkerPanicked)?;
    Err(failure)
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
    commit_provider: Option<Arc<dyn CommitIndexProvider>>,
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
    let event_handler = move |event: notify::Result<Event>| {
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
            enqueued: Instant::now(),
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                callback_metrics
                    .dropped_watcher_events
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    };
    let watcher = new_workspace_watcher(event_handler);
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
        &shared,
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
    let mut queue = PriorityWorkQueue::new(QUEUE_PER_CLASS_CAPACITY, QUEUE_AGING_AFTER);
    let mut checkpoint_pending = false;
    'outer: loop {
        // Block only when no staged work remains: a queued background
        // checkpoint must run even while the transport channel stays silent.
        if queue.is_empty() {
            let Ok(signal) = receiver.recv() else {
                break;
            };
            if matches!(signal, WorkerSignal::Shutdown) {
                break;
            }
            stage_signal(&mut queue, signal, &metrics);
        }
        for _ in 0..MAX_TRANSPORT_DRAIN_PER_PASS {
            let Ok(signal) = receiver.try_recv() else {
                break;
            };
            if matches!(signal, WorkerSignal::Shutdown) {
                break 'outer;
            }
            stage_signal(&mut queue, signal, &metrics);
        }
        let Some(signal) = queue.pop() else {
            continue;
        };
        // Publish selection before executing it. A freshness reconciliation
        // may wake its waiter from inside `reconcile`, so the query observing
        // that fresh revision must also be able to observe its dequeue and
        // queue-latency sample.
        metrics.publish_queue_metrics(queue.metrics());
        match signal {
            WorkerSignal::Shutdown => break,
            WorkerSignal::Barrier { .. } => {
                if shared
                    .pending_generation()
                    .is_ok_and(|(requested, completed)| requested > completed)
                {
                    let kind = reconcile(
                        &repository_root,
                        commit_provider.as_deref(),
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
                        &BTreeSet::new(),
                        false,
                        false,
                        true,
                    );
                    track_reconcile_outcome(
                        kind,
                        &mut queue,
                        reconciliations_since_full,
                        &options,
                        &mut checkpoint_pending,
                    );
                } else {
                    // The generation counter already completed this demand.
                    queue.note_superseded(WorkClass::FreshnessEdit);
                }
            }
            WorkerSignal::Checkpoint { .. } => {
                checkpoint_pending = false;
                let kind = reconcile(
                    &repository_root,
                    commit_provider.as_deref(),
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
                    &BTreeSet::new(),
                    false,
                    true,
                    false,
                );
                track_reconcile_outcome(
                    kind,
                    &mut queue,
                    reconciliations_since_full,
                    &options,
                    &mut checkpoint_pending,
                );
            }
            WorkerSignal::Filesystem {
                epoch,
                hints,
                uncertain,
                ..
            } => {
                if epoch <= reconciled_event_epoch {
                    // A newer reconcile already covered this epoch: the
                    // staged signal is obsolete freshness work.
                    queue.note_superseded(WorkClass::FreshnessEdit);
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
                        Ok(signal) => {
                            if matches!(signal, WorkerSignal::Shutdown) {
                                shutdown = true;
                            } else {
                                stage_signal(&mut queue, signal, &metrics);
                                for _ in 0..MAX_TRANSPORT_DRAIN_PER_PASS {
                                    let Ok(signal) = receiver.try_recv() else {
                                        break;
                                    };
                                    if matches!(signal, WorkerSignal::Shutdown) {
                                        shutdown = true;
                                        break;
                                    }
                                    stage_signal(&mut queue, signal, &metrics);
                                }
                            }
                            // Keep the bounded quiet window open so an
                            // editor's write/metadata/rename burst is
                            // reconciled as one stable state. Checkpoint
                            // signals stay staged: background work must not
                            // extend or interrupt the freshness window.
                            while let Some(signal) = queue.pop_where(|staged| {
                                !matches!(staged, WorkerSignal::Checkpoint { .. })
                            }) {
                                match signal {
                                    WorkerSignal::Filesystem {
                                        epoch: observed_epoch,
                                        hints: observed,
                                        uncertain: observed_uncertain,
                                        ..
                                    } => {
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
                                    // The generation counter already records
                                    // the waiter; the barrier fires after the
                                    // debounced reconcile below.
                                    WorkerSignal::Barrier { .. } => {}
                                    WorkerSignal::Checkpoint { .. } | WorkerSignal::Shutdown => {}
                                }
                            }
                            if shutdown {
                                break;
                            }
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
                let kind = reconcile(
                    &repository_root,
                    commit_provider.as_deref(),
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
                    &hints,
                    uncertain,
                    false,
                    false,
                );
                track_reconcile_outcome(
                    kind,
                    &mut queue,
                    reconciliations_since_full,
                    &options,
                    &mut checkpoint_pending,
                );
            }
        }

        // A barrier signal may have been coalesced because the bounded queue
        // was full. The generation counter is the durable source of demand.
        if shared
            .pending_generation()
            .is_ok_and(|(requested, completed)| requested > completed)
        {
            let kind = reconcile(
                &repository_root,
                commit_provider.as_deref(),
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
                &BTreeSet::new(),
                false,
                false,
                true,
            );
            track_reconcile_outcome(
                kind,
                &mut queue,
                reconciliations_since_full,
                &options,
                &mut checkpoint_pending,
            );
        }
        metrics.publish_queue_metrics(queue.metrics());
    }
    // Shutdown cancels every staged item so queued work never leaks
    // unaccounted when the owner stops.
    let _ = queue.retain(|_| false);
    metrics.publish_queue_metrics(queue.metrics());
    shared.stop();
    info!("live syntax index worker stopped");
}

/// Stages one transported signal into the priority queue. Shutdown bypasses
/// the queue at the call sites so it can never sit behind staged work.
///
/// Backpressure: a full freshness class rejects with `QueueFull`. Rejected
/// barrier demand survives in the durable generation counter; rejected
/// filesystem evidence is lost, so the dropped-event counter advances and the
/// next reconciliation distrusts retained bodies — the same honest
/// degradation as a full transport channel.
fn stage_signal(
    queue: &mut PriorityWorkQueue<WorkerSignal>,
    signal: WorkerSignal,
    metrics: &MetricsState,
) {
    let is_filesystem = matches!(signal, WorkerSignal::Filesystem { .. });
    let class = signal.class();
    let enqueued = signal.enqueued();
    if let Err(QueueFull { .. }) = queue.push(class, signal, enqueued)
        && is_filesystem
    {
        metrics
            .dropped_watcher_events
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Checkpoint bookkeeping after one reconcile. A full reread — however it
/// was triggered — makes every queued checkpoint obsolete, so it is cancelled
/// out of the queue. When the interval expires, the periodic full reread is
/// staged as `Reconciliation`-class background work that freshness edits
/// overtake, instead of inflating the next freshness reconcile. A failed
/// reconcile does not re-stage the checkpoint immediately: the next event
/// retries it, mirroring the pre-checkpoint behavior and avoiding a hot
/// retry loop on a persistent failure.
fn track_reconcile_outcome(
    kind: Option<ReconciliationKind>,
    queue: &mut PriorityWorkQueue<WorkerSignal>,
    reconciliations_since_full: u64,
    options: &LiveIndexOptions,
    checkpoint_pending: &mut bool,
) {
    if kind == Some(ReconciliationKind::Full) {
        queue.retain(|signal| !matches!(signal, WorkerSignal::Checkpoint { .. }));
        *checkpoint_pending = false;
    }
    if kind.is_some()
        && !*checkpoint_pending
        && reconciliations_since_full >= options.full_reconcile_interval
    {
        let now = Instant::now();
        let staged = queue.push(
            WorkClass::Reconciliation,
            WorkerSignal::Checkpoint { enqueued: now },
            now,
        );
        *checkpoint_pending = staged.is_ok();
    }
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
    commit_provider: Option<&dyn CommitIndexProvider>,
    syntax_index: &mut WorkspaceSyntaxIndex,
    engine: &WorkspaceEngine,
    watcher: &mut WorkspaceWatcher,
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
    hints: &BTreeSet<RepoRelativePath>,
    uncertain_hint: bool,
    checkpoint: bool,
    cancel_when_unobserved: bool,
) -> Option<ReconciliationKind> {
    let pending = shared.pending_generation().unwrap_or((0, 0));
    let (generation, completed_before, operation) = if cancel_when_unobserved {
        let Ok(reconciliation) = shared.begin_barrier_reconciliation() else {
            return None;
        };
        reconciliation
    } else {
        (pending.0, pending.1, OperationContext::unbounded())
    };
    let watcher_errors = metrics.watcher_errors.load(Ordering::Acquire);
    let dropped_events = metrics.dropped_watcher_events.load(Ordering::Acquire);
    let watcher_error_advanced = watcher_errors > *reconciled_watcher_errors;
    let dropped_event_advanced = dropped_events > *reconciled_dropped_events;
    let mut full_reasons = full_reconciliation_reasons(ReconciliationPolicy {
        cache_initialized: source_cache.initialized,
        watcher_health_degraded: *watcher_degraded,
        watcher_error_advanced,
        dropped_event_advanced,
        uncertain_hint,
    });
    // A checkpoint is explicit background `Reconciliation`-class work: it
    // rereads every body on its own schedule instead of silently inflating a
    // freshness-triggered reconcile when the interval expires (issue #44).
    if checkpoint {
        full_reasons.push(FullReconciliationReason::PeriodicCheckpoint);
    }
    let force_full = !full_reasons.is_empty();
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
                full_reasons.clone(),
                &operation,
            )?;
            let ReconcileReport {
                mut graph,
                metrics: reconcile_metrics,
                next_index,
                mut indexing,
                project_model,
                dependency_impact,
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
                    shared,
                    force_reinstall_watches,
                )?;
            }
            let status = if *watcher_degraded || indexing.is_degraded() {
                WorkspaceStatus::Degraded
            } else {
                WorkspaceStatus::Ready
            };

            let current = engine.snapshot();
            let candidate_graph = graph.as_ref().unwrap_or_else(|| current.graph());
            let diff = match diff_for_graph(
                repository_root,
                current.revision().next(),
                candidate_graph,
                &operation,
            ) {
                Ok(diff) => diff,
                Err(error) => {
                    operation
                        .check()
                        .map_err(|_| WorkspaceIndexError::Cancelled)?;
                    warn!(%error, "retrying unstable worktree layer composition");
                    continue;
                }
            };
            let commit_changed = diff.scope.base_commit != current.layers().commit_snapshot.commit;
            let commit = if commit_changed {
                let index_options =
                    IndexOptions::new(syntax_index.budgets(), operation.cancellation())?;
                let candidate = match commit_provider {
                    Some(provider) => provider.commit_index(
                        repository_root,
                        &current.identity().repository,
                        diff.scope.base_commit.as_deref(),
                        index_options,
                    ),
                    None => index_commit_with_options(
                        repository_root,
                        diff.scope.base_commit.as_deref(),
                        index_options,
                    ),
                };
                match candidate {
                    Ok(commit) => Some(commit),
                    Err(error) => {
                        operation
                            .check()
                            .map_err(|_| WorkspaceIndexError::Cancelled)?;
                        warn!(%error, "retrying changed commit snapshot construction");
                        continue;
                    }
                }
            } else {
                None
            };
            let (commit_files, commit_bytes) = commit.as_ref().map_or_else(
                || {
                    (
                        current.layers().commit_snapshot.source_files,
                        current.layers().commit_snapshot.source_bytes,
                    )
                },
                |commit| (commit.source_files, commit.source_bytes),
            );
            let mut layers = workspace_graph_layers(commit_files, commit_bytes, &diff);
            layers.commit_snapshot.reuse = commit.as_ref().map_or_else(
                || current.layers().commit_snapshot.reuse.clone(),
                |commit| commit.reuse.clone(),
            );
            let ownership_only_graph = graph.is_none() && current.layers() != &layers;
            if graph.is_some() || ownership_only_graph {
                let commit_graph = commit
                    .as_ref()
                    .map_or_else(|| current.commit_graph(), |commit| &commit.graph);
                let source_layers = source_layers_for_graph(commit_graph, candidate_graph);
                graph = Some(candidate_graph.clone().with_source_layers(source_layers));
                if ownership_only_graph {
                    mark_publication_reused(&mut indexing.publication);
                }
            }

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
            let verified_diff = match diff_for_graph(
                repository_root,
                current.revision().next(),
                graph.as_ref().unwrap_or_else(|| current.graph()),
                &operation,
            ) {
                Ok(diff) => diff,
                Err(error) => {
                    operation
                        .check()
                        .map_err(|_| WorkspaceIndexError::Cancelled)?;
                    warn!(%error, "retrying worktree layer verification");
                    continue;
                }
            };
            if verified_diff != diff {
                continue;
            }
            let provider_inputs = next_index.as_ref().map_or_else(
                || syntax_index.provider_inputs(),
                |next| next.provider_inputs(),
            );
            let published = publish_fresh(
                engine,
                FreshPublication {
                    graph: graph.as_ref(),
                    commit_graph: commit.as_ref().map(|commit| &commit.graph),
                    layers: &layers,
                    status,
                    indexing: &indexing,
                    provider_inputs,
                    project_model: project_model.as_ref(),
                },
            )
            .map_err(WorkspaceIndexError::Update)?;
            if let Some(next_index) = next_index {
                *syntax_index = next_index;
            }
            let invalidations = diff_file_invalidations(source_cache, &stable.cache);
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
            metrics.record_file_invalidations(invalidations);
            metrics.record_project_impact(dependency_impact.as_ref());
            if stable.kind == ReconciliationKind::Full {
                metrics.record_full_reconciliation_reasons(&stable.full_reasons);
            }
            if published {
                metrics.published_revisions.fetch_add(1, Ordering::Relaxed);
            }
            *reconciled_event_epoch = stable.event_epoch;
            metrics.record_barrier_completion(stable.covered_generation, completed_before);
            info!(
                kind = ?stable.kind,
                hinted_paths = hints.len(),
                force_full,
                full_reasons = ?stable.full_reasons,
                files_read = stable.files_read,
                covered_generation = stable.covered_generation,
                dependency_impacted_units = dependency_impact
                    .as_ref()
                    .map_or(0, |impact| impact.changes.len() as u64 + impact.changes_omitted),
                "freshness reconciliation completed"
            );
            return Ok((stable.covered_generation, stable.kind));
        }
        Err(WorkspaceIndexError::Update(
            "worktree changed before fresh revision publication".to_owned(),
        ))
    })();
    if watch_set_dirty {
        *watcher_degraded = true;
    }
    match result {
        Ok((completed_generation, kind)) => {
            shared.complete(completed_generation, Ok(()));
            Some(kind)
        }
        Err(WorkspaceIndexError::Cancelled) => {
            shared.abandon_worker();
            info!("freshness reconciliation cancelled before publication");
            None
        }
        Err(error) => {
            metrics
                .reconciliation_failures
                .fetch_add(1, Ordering::Relaxed);
            mark_failed_reconciliation(engine);
            let message = error.to_string();
            error!(%error, "live syntax reconciliation failed");
            shared.complete(generation, Err(message));
            None
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
}

/// Returns every typed entry-point reason a reconciliation must escalate to
/// a full content reread. An empty vector means targeted/identity-verified
/// reconciliation remains trustworthy (issues #40 and #43).
fn full_reconciliation_reasons(policy: ReconciliationPolicy) -> Vec<FullReconciliationReason> {
    let _watcher_health_degraded = policy.watcher_health_degraded;
    let mut reasons = Vec::new();
    if !policy.cache_initialized {
        reasons.push(FullReconciliationReason::ColdStart);
    }
    if policy.watcher_error_advanced {
        reasons.push(FullReconciliationReason::WatcherError);
    }
    if policy.dropped_event_advanced {
        reasons.push(FullReconciliationReason::WatcherEventMissed);
    }
    if policy.uncertain_hint {
        reasons.push(FullReconciliationReason::UncertainEventHints);
    }
    reasons
}

/// Per-file invalidation records between two retained source snapshots.
///
/// Returns nothing before the first initialized snapshot: the cold-start
/// reconciliation would otherwise flood the bounded window with one `Added`
/// record per discovered file. Content bytes are compared only to classify a
/// changed identity and never leave the worker. The intermediate batch keeps
/// only the newest records so diagnostics cannot allocate once per repository
/// file during a broad invalidation.
fn diff_file_invalidations(
    previous: &SourceSnapshotCache,
    next: &SourceSnapshotCache,
) -> FileInvalidationBatch {
    if !previous.initialized {
        return FileInvalidationBatch::default();
    }
    let mut invalidations = FileInvalidationBatch::default();
    for (path, cached) in &next.entries {
        match previous.entries.get(path) {
            None => invalidations.push(FileInvalidation {
                path: path.clone(),
                reason: FileInvalidationReason::Added,
            }),
            Some(before) if before.identity != cached.identity => {
                let reason = if before.source == cached.source {
                    FileInvalidationReason::MetadataChanged
                } else {
                    FileInvalidationReason::ContentChanged
                };
                invalidations.push(FileInvalidation {
                    path: path.clone(),
                    reason,
                });
            }
            Some(_) => {}
        }
    }
    for path in previous.entries.keys() {
        if !next.entries.contains_key(path) {
            invalidations.push(FileInvalidation {
                path: path.clone(),
                reason: FileInvalidationReason::Removed,
            });
        }
    }
    for (path, identity) in &next.metadata {
        match previous.metadata.get(path) {
            None => invalidations.push(FileInvalidation {
                path: path.clone(),
                reason: FileInvalidationReason::Added,
            }),
            Some(before) if before != identity => invalidations.push(FileInvalidation {
                path: path.clone(),
                reason: FileInvalidationReason::MetadataChanged,
            }),
            Some(_) => {}
        }
    }
    for path in previous.metadata.keys() {
        if !next.metadata.contains_key(path) {
            invalidations.push(FileInvalidation {
                path: path.clone(),
                reason: FileInvalidationReason::Removed,
            });
        }
    }
    invalidations
}

struct StableSourceSnapshot {
    scan: WorkspaceSourceScan,
    event_epoch: u64,
    covered_generation: u64,
    cache: SourceSnapshotCache,
    kind: ReconciliationKind,
    /// Typed causes when `kind` is `Full` (issues #40 and #43).
    full_reasons: Vec<FullReconciliationReason>,
    files_read: u64,
}

#[allow(clippy::too_many_arguments)]
fn stable_scan(
    repository_root: &Path,
    syntax_index: &WorkspaceSyntaxIndex,
    metrics: &MetricsState,
    shared: &BarrierShared,
    previous: &SourceSnapshotCache,
    force_full: bool,
    full_reasons: Vec<FullReconciliationReason>,
    operation: &OperationContext,
) -> Result<StableSourceSnapshot, WorkspaceIndexError> {
    let mut last_error = None;
    let mut attempt_force_full = force_full;
    let mut full_performed = force_full;
    let mut full_reasons = full_reasons;
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
                push_full_reconciliation_reason(
                    &mut full_reasons,
                    FullReconciliationReason::ScanInstability,
                );
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
                push_full_reconciliation_reason(
                    &mut full_reasons,
                    FullReconciliationReason::ScanInstability,
                );
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
                    push_full_reconciliation_reason(
                        &mut full_reasons,
                        FullReconciliationReason::ScanInstability,
                    );
                    continue;
                }
            };
        if verified_inventory != inventory {
            last_error = Some(WorkspaceIndexError::Update(
                "Git source/metadata inventory changed during freshness reconciliation".to_owned(),
            ));
            attempt_force_full = true;
            full_performed = true;
            push_full_reconciliation_reason(
                &mut full_reasons,
                FullReconciliationReason::ScanInstability,
            );
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
                push_full_reconciliation_reason(
                    &mut full_reasons,
                    FullReconciliationReason::ScanInstability,
                );
                retry_cache = None;
            } else {
                let entries = std::mem::take(&mut loader.next);
                let metadata = loader.metadata_identities();
                drop(loader);
                retry_cache = Some(SourceSnapshotCache {
                    initialized: true,
                    inventory,
                    entries,
                    metadata,
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
        let metadata = loader.metadata_identities();
        return Ok(StableSourceSnapshot {
            scan,
            event_epoch: epoch,
            covered_generation,
            cache: SourceSnapshotCache {
                initialized: true,
                inventory,
                entries: loader.next,
                metadata,
            },
            kind,
            full_reasons,
            files_read,
        });
    }
    Err(last_error.unwrap_or_else(|| {
        WorkspaceIndexError::Update(
            "worktree kept changing during freshness reconciliation".to_owned(),
        )
    }))
}

fn push_full_reconciliation_reason(
    reasons: &mut Vec<FullReconciliationReason>,
    reason: FullReconciliationReason,
) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn desired_watch_directories(
    repository_root: &Path,
    indexed_paths: &[RepoRelativePath],
) -> (BTreeSet<PathBuf>, bool) {
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
    watcher: &mut WorkspaceWatcher,
    repository_root: &Path,
    indexed_paths: &[RepoRelativePath],
    watched: &mut BTreeSet<PathBuf>,
    metrics: &MetricsState,
    shared: &BarrierShared,
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
        if shared.is_stopped() {
            return Err(WorkspaceIndexError::Cancelled);
        }
        match watcher.unwatch(&directory) {
            Ok(()) => {}
            Err(error) if absent_watch_is_already_removed(&error) => {}
            Err(error) => {
                metrics.watcher_errors.fetch_add(1, Ordering::Relaxed);
                warn!(path = %directory.display(), %error, "failed to remove filesystem watch");
                degraded = true;
            }
        }
        watched.remove(&directory);
    }
    for directory in desired.difference(watched).cloned().collect::<Vec<_>>() {
        if shared.is_stopped() {
            return Err(WorkspaceIndexError::Cancelled);
        }
        match watcher.watch(&directory, RecursiveMode::NonRecursive) {
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

fn absent_watch_is_already_removed(error: &notify::Error) -> bool {
    matches!(
        &error.kind,
        NotifyErrorKind::WatchNotFound | NotifyErrorKind::PathNotFound
    )
}

fn diff_for_graph(
    repository_root: &Path,
    revision: Revision,
    graph: &SymbolGraph,
    operation: &OperationContext,
) -> Result<WorkspaceDiff, WorkspaceIndexError> {
    let workspace = DiffWorkspace::from_graph_with_context(
        repository_root.to_path_buf(),
        revision,
        DiffScope::Worktree,
        graph,
        operation,
    )
    .map_err(|error| WorkspaceIndexError::Update(error.to_string()))?;
    chakra_git::GitWorkspaceDiff
        .diff_with_context(workspace, operation)
        .map_err(|error| WorkspaceIndexError::Update(error.to_string()))
}

struct FreshPublication<'a> {
    graph: Option<&'a SymbolGraph>,
    commit_graph: Option<&'a SymbolGraph>,
    layers: &'a chakra_domain::composition::WorkspaceGraphLayers,
    status: WorkspaceStatus,
    indexing: &'a chakra_domain::indexing::IndexingStatus,
    provider_inputs: &'a [chakra_engine::ProviderInput],
    project_model: Option<&'a chakra_domain::project::ProjectModel>,
}

fn mark_publication_reused(publication: &mut IndexPublicationMetrics) {
    publication.structurally_incremental = true;
    publication.reused_files = publication
        .reused_files
        .saturating_add(publication.rebuilt_files);
    publication.rebuilt_files = 0;
    publication.reused_source_bytes = publication
        .reused_source_bytes
        .saturating_add(publication.rebuilt_source_bytes);
    publication.rebuilt_source_bytes = 0;
    publication.reused_symbols = publication
        .reused_symbols
        .saturating_add(publication.rebuilt_symbols);
    publication.rebuilt_symbols = 0;
    publication.reused_edges = publication
        .reused_edges
        .saturating_add(publication.rebuilt_edges);
    publication.rebuilt_edges = 0;
    publication.reused_call_sites = publication
        .reused_call_sites
        .saturating_add(publication.rebuilt_call_sites);
    publication.rebuilt_call_sites = 0;
}

fn publish_fresh(
    engine: &WorkspaceEngine,
    candidate: FreshPublication<'_>,
) -> Result<bool, String> {
    let FreshPublication {
        graph,
        commit_graph,
        layers,
        status,
        indexing,
        provider_inputs,
        project_model,
    } = candidate;
    let started = Instant::now();
    let current = engine.snapshot();
    if graph.is_none()
        && commit_graph.is_none()
        && current.layers() == layers
        && project_model.is_none()
        && current.freshness() == Freshness::Fresh
        && current.status() == status
        && current.indexing() == indexing
        && current.provider_inputs_match(provider_inputs)
    {
        return Ok(false);
    }
    for _ in 0..MAX_PUBLISH_ATTEMPTS {
        let mut update = engine.begin_update();
        if let Some(graph) = graph {
            update.replace_graph(graph.clone());
        }
        if let Some(commit_graph) = commit_graph {
            update.set_graph_layers(commit_graph.clone(), layers.clone());
        } else {
            update.set_layers(layers.clone());
        }
        if let Some(model) = project_model {
            update.set_project_model(model.clone());
        }
        update.set_indexing(indexing.clone());
        update.set_provider_inputs(provider_inputs.iter().cloned());
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
    fn startup_timeout_cancels_and_joins_the_owned_worker() {
        let (sender, _receiver) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let shared = Arc::new(BarrierShared {
            state: Mutex::new(BarrierState::default()),
            completed: Condvar::new(),
        });
        let worker_shared = shared.clone();
        let active = Arc::new(AtomicU64::new(0));
        let worker_active = active.clone();
        let worker = thread::spawn(move || {
            let _ready_sender = ready_sender;
            worker_active.fetch_add(1, Ordering::SeqCst);
            while !worker_shared.is_stopped() {
                thread::yield_now();
            }
            worker_active.fetch_sub(1, Ordering::SeqCst);
        });

        let result = await_worker_startup(
            ready_receiver,
            &sender,
            &shared,
            worker,
            Duration::from_millis(10),
        );

        assert!(matches!(result, Err(LiveIndexError::StartupTimeout { .. })));
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(shared.is_stopped());
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
    fn full_reconciliation_policy_covers_uncertainty() {
        let baseline = ReconciliationPolicy {
            cache_initialized: true,
            watcher_health_degraded: false,
            watcher_error_advanced: false,
            dropped_event_advanced: false,
            uncertain_hint: false,
        };
        assert!(full_reconciliation_reasons(baseline).is_empty());
        assert_eq!(
            full_reconciliation_reasons(ReconciliationPolicy {
                cache_initialized: false,
                ..baseline
            }),
            vec![FullReconciliationReason::ColdStart]
        );
        assert!(
            full_reconciliation_reasons(ReconciliationPolicy {
                watcher_health_degraded: true,
                ..baseline
            })
            .is_empty()
        );
        assert_eq!(
            full_reconciliation_reasons(ReconciliationPolicy {
                watcher_error_advanced: true,
                ..baseline
            }),
            vec![FullReconciliationReason::WatcherError]
        );
        assert_eq!(
            full_reconciliation_reasons(ReconciliationPolicy {
                dropped_event_advanced: true,
                ..baseline
            }),
            vec![FullReconciliationReason::WatcherEventMissed]
        );
        assert_eq!(
            full_reconciliation_reasons(ReconciliationPolicy {
                uncertain_hint: true,
                ..baseline
            }),
            vec![FullReconciliationReason::UncertainEventHints]
        );
        assert_eq!(
            full_reconciliation_reasons(ReconciliationPolicy {
                watcher_error_advanced: true,
                dropped_event_advanced: true,
                ..baseline
            }),
            vec![
                FullReconciliationReason::WatcherError,
                FullReconciliationReason::WatcherEventMissed,
            ],
            "simultaneous causes must not be collapsed to one priority reason"
        );
    }

    #[test]
    fn staged_freshness_edit_overtakes_an_older_queued_checkpoint()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut queue = PriorityWorkQueue::new(QUEUE_PER_CLASS_CAPACITY, QUEUE_AGING_AFTER);
        let checkpoint_enqueued = Instant::now();
        queue.push(
            WorkClass::Reconciliation,
            WorkerSignal::Checkpoint {
                enqueued: checkpoint_enqueued,
            },
            checkpoint_enqueued,
        )?;
        let edit_enqueued = Instant::now();
        queue.push(
            WorkClass::FreshnessEdit,
            WorkerSignal::Filesystem {
                epoch: 7,
                hints: Vec::new(),
                uncertain: false,
                enqueued: edit_enqueued,
            },
            edit_enqueued,
        )?;

        assert!(matches!(
            queue.pop(),
            Some(WorkerSignal::Filesystem { epoch: 7, .. })
        ));
        assert!(matches!(queue.pop(), Some(WorkerSignal::Checkpoint { .. })));
        let metrics = queue.metrics();
        assert_eq!(metrics.for_class(WorkClass::FreshnessEdit).dequeued, 1);
        assert_eq!(metrics.for_class(WorkClass::Reconciliation).dequeued, 1);
        assert_eq!(
            metrics.for_class(WorkClass::FreshnessEdit).latency.samples,
            1
        );
        Ok(())
    }

    #[test]
    fn interval_expiry_stages_one_checkpoint_and_full_reconcile_cancels_it() {
        let options = LiveIndexOptions {
            full_reconcile_interval: 2,
            ..LiveIndexOptions::default()
        };
        let mut queue = PriorityWorkQueue::new(QUEUE_PER_CLASS_CAPACITY, QUEUE_AGING_AFTER);
        let mut checkpoint_pending = false;

        // Below the interval a targeted reconcile stages nothing.
        track_reconcile_outcome(
            Some(ReconciliationKind::Targeted),
            &mut queue,
            1,
            &options,
            &mut checkpoint_pending,
        );
        assert!(!checkpoint_pending);
        assert!(queue.is_empty());

        // At the interval a targeted reconcile stages background work once.
        track_reconcile_outcome(
            Some(ReconciliationKind::Targeted),
            &mut queue,
            2,
            &options,
            &mut checkpoint_pending,
        );
        assert!(checkpoint_pending);
        track_reconcile_outcome(
            Some(ReconciliationKind::Targeted),
            &mut queue,
            3,
            &options,
            &mut checkpoint_pending,
        );
        assert_eq!(queue.len(), 1, "checkpoint staging must deduplicate");

        // A full reconcile — however triggered — cancels the staged
        // checkpoint instead of letting it reread every body again.
        track_reconcile_outcome(
            Some(ReconciliationKind::Full),
            &mut queue,
            0,
            &options,
            &mut checkpoint_pending,
        );
        assert!(!checkpoint_pending);
        assert!(queue.is_empty());
        assert_eq!(
            queue
                .metrics()
                .for_class(WorkClass::Reconciliation)
                .cancelled,
            1
        );
    }

    #[test]
    fn invalidation_batch_bounds_intermediate_storage_while_counting_all_records()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut batch = FileInvalidationBatch::default();
        let records = MAX_FILE_INVALIDATION_RECORDS as u64 + 4;
        for index in 0..records {
            batch.push(FileInvalidation {
                path: RepoRelativePath::new(format!("src/file-{index}.rs"))?,
                reason: FileInvalidationReason::ContentChanged,
            });
        }

        assert_eq!(batch.records, records);
        assert_eq!(batch.recent.len(), MAX_FILE_INVALIDATION_RECORDS);
        assert_eq!(
            batch.recent.front().map(|record| record.path.as_str()),
            Some("src/file-4.rs")
        );
        Ok(())
    }

    #[test]
    fn failed_reconcile_does_not_restage_a_checkpoint() {
        let options = LiveIndexOptions {
            full_reconcile_interval: 1,
            ..LiveIndexOptions::default()
        };
        let mut queue = PriorityWorkQueue::new(QUEUE_PER_CLASS_CAPACITY, QUEUE_AGING_AFTER);
        let mut checkpoint_pending = false;

        track_reconcile_outcome(None, &mut queue, 5, &options, &mut checkpoint_pending);
        assert!(!checkpoint_pending);
        assert!(queue.is_empty());
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

    #[test]
    fn absent_watches_are_idempotent_cleanup_not_backend_failures() {
        assert!(absent_watch_is_already_removed(
            &notify::Error::watch_not_found()
        ));
        assert!(absent_watch_is_already_removed(
            &notify::Error::path_not_found()
        ));
        assert!(!absent_watch_is_already_removed(&notify::Error::generic(
            "backend failure"
        )));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_workspace_watcher_is_polling() {
        assert_eq!(WorkspaceWatcher::kind(), notify::WatcherKind::PollWatcher);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_root_churn_does_not_recursively_retain_ignored_file_descriptors()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let ignored = workspace.path().join("target/corpus/deep");
        std::fs::create_dir_all(&ignored)?;
        for index in 0..256 {
            std::fs::write(ignored.join(format!("ignored-{index}.txt")), b"ignored")?;
        }

        let (event_sender, event_receiver) = mpsc::sync_channel(8);
        let mut watcher = new_workspace_watcher(move |event: notify::Result<Event>| {
            if event.is_ok() {
                let _ = event_sender.try_send(());
            }
        })?;
        watcher.watch(workspace.path(), RecursiveMode::NonRecursive)?;
        let descriptors_before = std::fs::read_dir("/dev/fd")?.count();

        std::fs::create_dir(workspace.path().join("branch-switch"))?;
        event_receiver.recv_timeout(WATCHER_POLL_INTERVAL + Duration::from_secs(2))?;
        let observation_deadline = Instant::now() + Duration::from_millis(500);
        let mut maximum_descriptors = descriptors_before;
        while Instant::now() < observation_deadline {
            maximum_descriptors = maximum_descriptors.max(std::fs::read_dir("/dev/fd")?.count());
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            maximum_descriptors.saturating_sub(descriptors_before) < 64,
            "non-recursive root churn retained unexpected descriptors: before={descriptors_before}, maximum={maximum_descriptors}"
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_watcher_uses_bounded_non_recursive_source_directories()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path();
        std::fs::create_dir_all(root.join("src/main/java/example"))?;
        std::fs::create_dir_all(root.join("src/test/java/example"))?;
        let paths = [
            RepoRelativePath::new("src/main/java/example/Main.java")?,
            RepoRelativePath::new("src/test/java/example/MainTest.java")?,
        ];

        let (desired, truncated) = desired_watch_directories(root, &paths);

        assert_eq!(
            desired,
            BTreeSet::from([
                root.to_path_buf(),
                root.join("src"),
                root.join("src/main"),
                root.join("src/main/java"),
                root.join("src/main/java/example"),
                root.join("src/test"),
                root.join("src/test/java"),
                root.join("src/test/java/example"),
            ])
        );
        assert!(!truncated);
        Ok(())
    }
}
