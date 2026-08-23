//! Adapter handle: owns the worker thread, the command channel, and the
//! published observability state, and implements [`PreciseProvider`] by
//! delegating language-specific facts to the hooks.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chakra_domain::operation::OperationContext;
use chakra_domain::query::{ProviderMetrics, ProviderProgress};
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{
    PreciseProvider, PreciseQueryRequest, PreciseQueryResult, ProviderShutdownError,
    ProviderWorkspace,
};
use crossbeam_channel::{SendTimeoutError, Sender, bounded};

use crate::ProviderHooks;
use crate::state::{ProviderCommand, SharedState};
use crate::worker::WorkerCore;

/// Resolved provider process invocation (program plus arguments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCommandSpec {
    pub program: std::ffi::OsString,
    pub args: Vec<std::ffi::OsString>,
}

/// Process and bounded-wait settings shared by all worker-backed providers.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub command: ProviderCommandSpec,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub barrier_timeout: Duration,
    pub query_wait_timeout: Duration,
    pub restart_base_delay: Duration,
    pub restart_max_delay: Duration,
    pub command_capacity: usize,
    pub max_message_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("provider command capacity and message size bound must be non-zero")]
    InvalidCapacity,
    #[error("provider startup, request, and barrier timeouts must be non-zero")]
    InvalidTimeout,
    #[error("failed to spawn the provider owner thread: {0}")]
    ThreadSpawn(#[source] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerShutdownError {
    #[error("provider owner thread panicked")]
    WorkerPanicked,
    #[error("provider owner lock is poisoned")]
    LockPoisoned,
}

/// Owned provider process and worker lifecycle, parameterized by typed
/// language hooks.
pub struct ProviderHandle<H: ProviderHooks> {
    commands: Sender<ProviderCommand>,
    shared: Arc<Mutex<SharedState>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    stopped: AtomicBool,
    force_stop: Arc<AtomicBool>,
    config: WorkerConfig,
    hooks: Arc<H>,
}

impl<H: ProviderHooks> fmt::Debug for ProviderHandle<H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderHandle")
            .field("name", &self.hooks.name())
            .field("state", &self.state_snapshot())
            .field("stopped", &self.stopped.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl<H: ProviderHooks> ProviderHandle<H> {
    /// Starts the owner thread. A missing or failing provider process does not
    /// fail Chakra startup: the handle transitions to `Degraded` and later
    /// queries retain syntax results (ADR-0006/0013).
    pub fn start(
        initial_workspace: ProviderWorkspace,
        config: WorkerConfig,
        hooks: H,
    ) -> Result<Arc<Self>, StartError> {
        if config.command_capacity == 0 || config.max_message_bytes == 0 {
            return Err(StartError::InvalidCapacity);
        }
        if config.startup_timeout.is_zero()
            || config.request_timeout.is_zero()
            || config.barrier_timeout.is_zero()
            || config.query_wait_timeout.is_zero()
        {
            return Err(StartError::InvalidTimeout);
        }
        let (commands, receiver) = bounded(config.command_capacity);
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let force_stop = Arc::new(AtomicBool::new(false));
        let hooks = Arc::new(hooks);
        let worker_shared = shared.clone();
        let worker_stop = force_stop.clone();
        let worker_config = config.clone();
        let worker_hooks = hooks.clone();
        let worker = thread::Builder::new()
            .name(format!("chakra-{}", hooks.name()))
            .spawn(move || {
                WorkerCore::new(
                    receiver,
                    worker_shared,
                    worker_stop,
                    worker_config,
                    worker_hooks,
                    initial_workspace,
                )
                .run();
            })
            .map_err(StartError::ThreadSpawn)?;
        Ok(Arc::new(Self {
            commands,
            shared,
            worker: Mutex::new(Some(worker)),
            stopped: AtomicBool::new(false),
            force_stop,
            config,
            hooks,
        }))
    }

    pub fn last_error(&self) -> Option<String> {
        self.shared
            .lock()
            .ok()
            .and_then(|state| state.last_error.clone())
    }

    pub fn progress(&self) -> Option<ProviderProgress> {
        self.shared
            .lock()
            .ok()
            .and_then(|state| state.progress.clone())
    }

    pub fn metrics(&self) -> Option<ProviderMetrics> {
        self.shared.lock().ok().map(|state| state.metrics.clone())
    }

    /// Idempotent cooperative shutdown followed by joining the owned worker.
    /// The owned process group is terminated, so no provider child remains.
    pub fn shutdown(&self) -> Result<(), WorkerShutdownError> {
        self.stopped.store(true, Ordering::Release);
        self.force_stop.store(true, Ordering::Release);
        let worker = self
            .worker
            .lock()
            .map_err(|_| WorkerShutdownError::LockPoisoned)?
            .take();
        if let Some(worker) = worker {
            worker
                .join()
                .map_err(|_| WorkerShutdownError::WorkerPanicked)?;
        }
        Ok(())
    }

    fn state_snapshot(&self) -> (ProviderState, Option<Revision>, u64) {
        self.shared
            .lock()
            .map_or((ProviderState::Degraded, None, 0), |state| {
                (state.state, state.synced_revision, state.provider_epoch)
            })
    }
}

impl<H: ProviderHooks> PreciseProvider for ProviderHandle<H> {
    fn name(&self) -> &'static str {
        self.hooks.name()
    }

    fn supports(&self, language: Language) -> bool {
        self.hooks.synchronizes(language)
    }

    fn state_for(&self, revision: Revision) -> ProviderState {
        if self.stopped.load(Ordering::Acquire) {
            return ProviderState::Degraded;
        }
        let (state, synced_revision, _) = self.state_snapshot();
        match state {
            ProviderState::Ready if synced_revision == Some(revision) => ProviderState::Ready,
            ProviderState::Ready | ProviderState::CatchingUp => ProviderState::CatchingUp,
            other => other,
        }
    }

    fn last_error(&self) -> Option<String> {
        self.last_error()
    }

    fn progress(&self) -> Option<ProviderProgress> {
        self.progress()
    }

    fn metrics(&self) -> Option<ProviderMetrics> {
        self.metrics()
    }

    fn query_wait_budget(&self) -> Option<Duration> {
        Some(self.config.query_wait_timeout)
    }

    fn shutdown(&self) -> Result<(), ProviderShutdownError> {
        ProviderHandle::shutdown(self)
            .map_err(|error| ProviderShutdownError::new(error.to_string()))
    }

    fn enrich(&self, request: PreciseQueryRequest) -> PreciseQueryResult {
        self.enrich_with_context(request, &OperationContext::unbounded())
    }

    fn enrich_with_context(
        &self,
        request: PreciseQueryRequest,
        operation: &OperationContext,
    ) -> PreciseQueryResult {
        let revision = request.workspace.revision;
        if self.stopped.load(Ordering::Acquire) {
            return PreciseQueryResult::unavailable(revision, ProviderState::Degraded);
        }
        let provider_operation = operation.bounded_by(self.config.query_wait_timeout);
        if provider_operation.check().is_err() {
            return PreciseQueryResult::unavailable(revision, ProviderState::CatchingUp);
        }
        let (sender, receiver) = bounded(1);
        let queue_operation = provider_operation.bounded_by(self.config.barrier_timeout);
        let command_operation = if self.hooks.cold_start_outlives_caller_wait() {
            operation.clone()
        } else {
            provider_operation.clone()
        };
        let mut command = ProviderCommand::Enrich {
            request: Box::new(request),
            operation: command_operation,
            response: sender,
        };
        loop {
            let Ok(wait) = queue_operation.poll_timeout(Duration::from_millis(10)) else {
                return PreciseQueryResult::unavailable(revision, ProviderState::CatchingUp);
            };
            match self.commands.send_timeout(command, wait) {
                Ok(()) => break,
                Err(SendTimeoutError::Timeout(returned)) => command = returned,
                Err(SendTimeoutError::Disconnected(_)) => {
                    return PreciseQueryResult::unavailable(revision, ProviderState::Degraded);
                }
            }
        }
        loop {
            let Ok(poll) = provider_operation.poll_timeout(Duration::from_millis(10)) else {
                return PreciseQueryResult::unavailable(revision, ProviderState::CatchingUp);
            };
            match receiver.recv_timeout(poll) {
                Ok(result) => return result,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    return PreciseQueryResult::unavailable(revision, ProviderState::Degraded);
                }
            }
        }
    }
}

impl<H: ProviderHooks> Drop for ProviderHandle<H> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
