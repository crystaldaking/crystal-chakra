//! Bounded filesystem notifications plus deterministic multi-language freshness.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_engine::{FreshnessBarrier, FreshnessBarrierError, SymbolGraph, WorkspaceEngine};
use notify::event::{AccessKind, AccessMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tracing::{error, info, warn};

use crate::indexer::{
    ReconcileMetrics, ReconcileReport, WorkspaceIndexError, WorkspaceSources, WorkspaceSyntaxIndex,
    scan_repository_sources,
};

const EVENT_QUEUE_CAPACITY: usize = 256;
const DEBOUNCE_QUIET: Duration = Duration::from_millis(50);
const DEBOUNCE_MAX: Duration = Duration::from_millis(250);
const FRESHNESS_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STABLE_SCAN_ATTEMPTS: usize = 3;
const MAX_WATCHED_DIRECTORIES: usize = 4_096;
const MAX_PUBLISH_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveIndexMetrics {
    pub reconciliations: u64,
    pub reconciliation_failures: u64,
    pub published_revisions: u64,
    pub files_scanned: u64,
    pub files_reparsed: u64,
    pub relationship_files_recomputed: u64,
    pub unchanged_files: u64,
    pub created_files: u64,
    pub modified_files: u64,
    pub deleted_files: u64,
    pub syntax_error_files: u64,
    pub watcher_events: u64,
    pub dropped_watcher_events: u64,
    pub watcher_errors: u64,
    pub watched_directories: u64,
}

#[derive(Debug, Default)]
struct MetricsState {
    reconciliations: AtomicU64,
    reconciliation_failures: AtomicU64,
    published_revisions: AtomicU64,
    files_scanned: AtomicU64,
    files_reparsed: AtomicU64,
    relationship_files_recomputed: AtomicU64,
    unchanged_files: AtomicU64,
    created_files: AtomicU64,
    modified_files: AtomicU64,
    deleted_files: AtomicU64,
    syntax_error_files: AtomicU64,
    watcher_events: AtomicU64,
    dropped_watcher_events: AtomicU64,
    watcher_errors: AtomicU64,
    watched_directories: AtomicU64,
    event_epoch: AtomicU64,
}

impl MetricsState {
    fn snapshot(&self) -> LiveIndexMetrics {
        let load = |value: &AtomicU64| value.load(Ordering::Relaxed);
        LiveIndexMetrics {
            reconciliations: load(&self.reconciliations),
            reconciliation_failures: load(&self.reconciliation_failures),
            published_revisions: load(&self.published_revisions),
            files_scanned: load(&self.files_scanned),
            files_reparsed: load(&self.files_reparsed),
            relationship_files_recomputed: load(&self.relationship_files_recomputed),
            unchanged_files: load(&self.unchanged_files),
            created_files: load(&self.created_files),
            modified_files: load(&self.modified_files),
            deleted_files: load(&self.deleted_files),
            syntax_error_files: load(&self.syntax_error_files),
            watcher_events: load(&self.watcher_events),
            dropped_watcher_events: load(&self.dropped_watcher_events),
            watcher_errors: load(&self.watcher_errors),
            watched_directories: load(&self.watched_directories),
        }
    }

    fn record_reconcile(&self, metrics: ReconcileMetrics) {
        self.reconciliations.fetch_add(1, Ordering::Relaxed);
        self.files_scanned
            .fetch_add(metrics.scanned_files, Ordering::Relaxed);
        self.files_reparsed
            .fetch_add(metrics.reparsed_files, Ordering::Relaxed);
        self.relationship_files_recomputed
            .fetch_add(metrics.relationship_files_recomputed, Ordering::Relaxed);
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
    #[error(transparent)]
    Freshness(#[from] FreshnessBarrierError),
    #[error("live index worker panicked")]
    WorkerPanicked,
}

#[derive(Debug, Clone, Copy)]
enum WorkerSignal {
    Filesystem(u64),
    Barrier,
    Shutdown,
}

#[derive(Debug, Default)]
struct BarrierState {
    requested: u64,
    completed: u64,
    last_error: Option<String>,
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
        Ok((state.requested, state.completed))
    }

    fn complete(&self, generation: u64, result: Result<(), String>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.completed = state.completed.max(generation);
        state.last_error = result.err();
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
        let target = {
            let mut state = self
                .shared
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
            state.requested
        };
        match self.sender.try_send(WorkerSignal::Barrier) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {
                self.shared.stop();
                return Err(FreshnessBarrierError::new("live index worker disconnected"));
            }
        }

        let deadline = Instant::now() + FRESHNESS_TIMEOUT;
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| FreshnessBarrierError::new("freshness state lock is poisoned"))?;
        loop {
            if state.completed >= target {
                return state.last_error.as_ref().map_or(Ok(()), |message| {
                    Err(FreshnessBarrierError::new(message.clone()))
                });
            }
            if state.shutdown {
                return Err(FreshnessBarrierError::new("live index worker stopped"));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                self.metrics
                    .reconciliation_failures
                    .fetch_add(1, Ordering::Relaxed);
                return Err(FreshnessBarrierError::new(
                    "timed out waiting for filesystem reconciliation",
                ));
            };
            let (next, timeout) = self
                .shared
                .completed
                .wait_timeout(state, remaining)
                .map_err(|_| FreshnessBarrierError::new("freshness state lock is poisoned"))?;
            state = next;
            if timeout.timed_out() && state.completed < target {
                self.metrics
                    .reconciliation_failures
                    .fetch_add(1, Ordering::Relaxed);
                return Err(FreshnessBarrierError::new(
                    "timed out waiting for filesystem reconciliation",
                ));
            }
        }
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
) {
    let callback_sender = sender.clone();
    let callback_metrics = metrics.clone();
    let callback_engine = engine.clone();
    let callback_publication_gate = publication_gate.clone();
    let watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
        match event {
            Ok(event) => {
                callback_metrics
                    .watcher_events
                    .fetch_add(1, Ordering::Relaxed);
                if !event_may_change_workspace(&event) {
                    return;
                }
            }
            Err(error) => {
                callback_metrics
                    .watcher_errors
                    .fetch_add(1, Ordering::Relaxed);
                warn!(%error, "filesystem watcher reported an error");
            }
        }
        let epoch = invalidate_for_event(
            &callback_engine,
            &callback_metrics,
            &callback_publication_gate,
        );
        match callback_sender.try_send(WorkerSignal::Filesystem(epoch)) {
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
    let mut watcher_degraded = match refresh_watches(
        &mut watcher,
        &repository_root,
        &syntax_index,
        &mut watched,
        &metrics,
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
                    );
                }
            }
            WorkerSignal::Filesystem(epoch) => {
                if epoch <= reconciled_event_epoch {
                    continue;
                }
                mark_stale(&engine);
                let mut window = DebounceWindow::new(Instant::now());
                let mut shutdown = false;
                loop {
                    let deadline = window.deadline();
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    match receiver.recv_timeout(remaining) {
                        Ok(WorkerSignal::Filesystem(_)) => window.observe(Instant::now()),
                        Ok(WorkerSignal::Barrier) => break,
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
            );
        }
    }
    shared.stop();
    info!("live syntax index worker stopped");
}

fn event_may_change_workspace(event: &Event) -> bool {
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
) {
    let generation = shared
        .pending_generation()
        .map_or(0, |(requested, _)| requested);
    let result = (|| {
        for _ in 0..MAX_STABLE_SCAN_ATTEMPTS {
            let (sources, event_epoch) = stable_scan(repository_root, metrics)?;
            let ReconcileReport {
                graph,
                metrics: reconcile_metrics,
                next_index,
            } = syntax_index.reconcile_sources(sources)?;
            let watch_index = next_index.as_ref().unwrap_or(syntax_index);
            *watcher_degraded =
                refresh_watches(watcher, repository_root, watch_index, watched, metrics)?;
            let status = if *watcher_degraded {
                WorkspaceStatus::Degraded
            } else {
                WorkspaceStatus::Ready
            };

            let _publication = match publication_gate.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if metrics.event_epoch.load(Ordering::Acquire) != event_epoch {
                continue;
            }
            let published = publish_fresh(engine, graph.as_ref(), status)
                .map_err(WorkspaceIndexError::Update)?;
            if let Some(next_index) = next_index {
                *syntax_index = next_index;
            }
            metrics.record_reconcile(reconcile_metrics);
            if published {
                metrics.published_revisions.fetch_add(1, Ordering::Relaxed);
            }
            *reconciled_event_epoch = event_epoch;
            return Ok(());
        }
        Err(WorkspaceIndexError::Update(
            "worktree changed before fresh revision publication".to_owned(),
        ))
    })();
    match result {
        Ok(()) => shared.complete(generation, Ok(())),
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

fn stable_scan(
    repository_root: &Path,
    metrics: &MetricsState,
) -> Result<(WorkspaceSources, u64), WorkspaceIndexError> {
    let mut last_error = None;
    for _ in 0..MAX_STABLE_SCAN_ATTEMPTS {
        let epoch = metrics.event_epoch.load(Ordering::Acquire);
        let first = match scan_repository_sources(repository_root) {
            Ok(sources) => sources,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let second = match scan_repository_sources(repository_root) {
            Ok(sources) => sources,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        if first == second && epoch == metrics.event_epoch.load(Ordering::Acquire) {
            return Ok((second, epoch));
        }
    }
    Err(last_error.unwrap_or_else(|| {
        WorkspaceIndexError::Update(
            "worktree kept changing during freshness reconciliation".to_owned(),
        )
    }))
}

fn desired_watch_directories(
    repository_root: &Path,
    syntax_index: &WorkspaceSyntaxIndex,
) -> (BTreeSet<PathBuf>, bool) {
    let mut desired = BTreeSet::from([repository_root.to_path_buf()]);
    for path in syntax_index.paths() {
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
    syntax_index: &WorkspaceSyntaxIndex,
    watched: &mut BTreeSet<PathBuf>,
    metrics: &MetricsState,
) -> Result<bool, WorkspaceIndexError> {
    let (desired, mut degraded) = desired_watch_directories(repository_root, syntax_index);
    if degraded {
        metrics.watcher_errors.fetch_add(1, Ordering::Relaxed);
        warn!(
            maximum = MAX_WATCHED_DIRECTORIES,
            "watch directory bound reached; freshness barriers remain authoritative"
        );
    }
    for directory in watched.difference(&desired).cloned().collect::<Vec<_>>() {
        if let Err(error) = watcher.unwatch(&directory) {
            metrics.watcher_errors.fetch_add(1, Ordering::Relaxed);
            warn!(path = %directory.display(), %error, "failed to remove filesystem watch");
            degraded = true;
        }
        watched.remove(&directory);
    }
    for directory in desired.difference(watched).cloned().collect::<Vec<_>>() {
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
) -> Result<bool, String> {
    let current = engine.snapshot();
    if graph.is_none() && current.freshness() == Freshness::Fresh && current.status() == status {
        return Ok(false);
    }
    for _ in 0..MAX_PUBLISH_ATTEMPTS {
        let mut update = engine.begin_update();
        if let Some(graph) = graph {
            update.replace_graph(graph.clone());
        }
        update.set_status(status);
        update.set_freshness(Freshness::Fresh);
        match engine.publish(update) {
            Ok(_) => return Ok(true),
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
    use notify::event::{DataChange, ModifyKind};

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

        assert!(!event_may_change_workspace(&opened));
        assert!(!event_may_change_workspace(&read));
        assert!(event_may_change_workspace(&write));
        assert!(event_may_change_workspace(&modified));
        assert!(event_may_change_workspace(&Event::new(EventKind::Any)));
    }
}
