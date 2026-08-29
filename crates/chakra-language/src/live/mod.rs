//! Bounded filesystem notifications plus deterministic multi-language freshness.

mod barrier;
mod metrics;
mod source_cache;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_engine::{FreshnessBarrier, FreshnessBarrierError, SymbolGraph, WorkspaceEngine};
use notify::event::{AccessKind, AccessMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tracing::{error, info, warn};

use crate::indexer::{
    IndexOptions, ReconcileReport, WorkspaceIndexError, WorkspaceSourceScan, WorkspaceSyntaxIndex,
    scan_discovered_sources_with_options,
};

use barrier::{BarrierShared, BarrierState, LiveFreshnessBarrier};
use metrics::MetricsState;
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
pub(crate) enum WorkerSignal {
    Filesystem {
        epoch: u64,
        hints: Vec<chakra_domain::location::RepoRelativePath>,
        uncertain: bool,
    },
    Barrier,
    Shutdown,
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
            );
        })?;

    let worker = await_worker_startup(
        ready_receiver,
        &sender,
        &shared,
        worker,
        options.startup_timeout,
    )?;

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
                    shared,
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
            let provider_inputs = next_index.as_ref().map_or_else(
                || syntax_index.provider_inputs(),
                |next| next.provider_inputs(),
            );
            let published =
                publish_fresh(engine, graph.as_ref(), status, &indexing, provider_inputs)
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
        if let Err(error) = watcher.unwatch(&directory) {
            metrics.watcher_errors.fetch_add(1, Ordering::Relaxed);
            warn!(path = %directory.display(), %error, "failed to remove filesystem watch");
            degraded = true;
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

fn publish_fresh(
    engine: &WorkspaceEngine,
    graph: Option<&SymbolGraph>,
    status: WorkspaceStatus,
    indexing: &chakra_domain::indexing::IndexingStatus,
    provider_inputs: &[chakra_engine::ProviderInput],
) -> Result<bool, String> {
    let started = Instant::now();
    let current = engine.snapshot();
    if graph.is_none()
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
    use std::sync::atomic::AtomicU64;

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
    fn macos_recommended_watcher_is_kqueue() {
        assert_eq!(RecommendedWatcher::kind(), notify::WatcherKind::Kqueue);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn kqueue_uses_bounded_non_recursive_source_directories()
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
