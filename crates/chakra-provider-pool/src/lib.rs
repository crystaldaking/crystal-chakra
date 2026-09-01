//! Bounded orchestration for optional precise language providers.
//!
//! One pool owns process-global lifecycle policy while each adapter instance
//! continues to own one worktree's protocol worker and child process.
//! Providers start from the exact query workspace, are routed by disjoint
//! language capabilities, and may be reclaimed only while they have no
//! in-flight query.

mod config;

pub use config::{
    ProviderPoolConfig, ProviderPoolConfigError, ProviderRegistration, ProviderStartError,
};

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chakra_domain::identity::{WorkspaceId, WorkspaceIdentity};
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::query::{
    ProviderFallbackCause, ProviderMetrics, ProviderOrchestrationMetrics, ProviderProgress,
    ProviderQueueLatencyByPriority, WorkspaceProviderOrchestrationMetrics,
};
use chakra_domain::revision::Revision;
use chakra_domain::scheduling::QueueLatencyStats;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{
    PreciseProvider, PreciseQueryRequest, PreciseQueryResult, ProviderRequestPriority,
    ProviderShutdownError, ProviderWorkspace,
};
use thiserror::Error;

const ADMISSION_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderPoolShutdownError {
    #[error("provider-pool reaper panicked")]
    ReaperPanicked,
    #[error("provider-pool shutdown timed out with {running_queries} running queries")]
    RunningQueries { running_queries: usize },
    #[error("provider shutdown failed: {message}")]
    Provider { message: String },
    #[error("multiple provider-pool shutdown failures: {messages:?}")]
    Multiple { messages: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderPoolWorkspaceError {
    #[error("provider pool is shut down")]
    Stopped,
    #[error("workspace identity {workspace} conflicts with an existing provider-pool binding")]
    IdentityConflict { workspace: WorkspaceId },
}

#[derive(Debug)]
pub struct ProviderPool {
    inner: Arc<PoolInner>,
    reaper: Mutex<Option<JoinHandle<()>>>,
}

impl ProviderPool {
    pub fn start(
        config: ProviderPoolConfig,
        registrations: Vec<ProviderRegistration>,
    ) -> Result<Self, ProviderPoolConfigError> {
        validate(&config, &registrations)?;
        let registrations: Vec<_> = registrations.into_iter().map(Arc::new).collect();
        let inner = Arc::new(PoolInner {
            config,
            registrations,
            slots: Mutex::new(Vec::new()),
            state: Mutex::new(PoolState::default()),
            changed: Condvar::new(),
            stopped: AtomicBool::new(false),
        });
        let reaper_inner = inner.clone();
        let reaper = thread::Builder::new()
            .name("chakra-provider-reaper".to_owned())
            .spawn(move || reaper_loop(reaper_inner))
            .map_err(|error| ProviderPoolConfigError::ThreadSpawn(error.to_string()))?;
        Ok(Self {
            inner,
            reaper: Mutex::new(Some(reaper)),
        })
    }

    /// Lazy provider handles bound to exactly one materialized worktree.
    pub fn providers_for(
        &self,
        workspace: &WorkspaceIdentity,
    ) -> Result<Vec<Arc<dyn PreciseProvider>>, ProviderPoolWorkspaceError> {
        if self.inner.stopped.load(Ordering::Acquire) {
            return Err(ProviderPoolWorkspaceError::Stopped);
        }
        let mut slots = lock(&self.inner.slots);
        if self.inner.stopped.load(Ordering::Acquire) {
            return Err(ProviderPoolWorkspaceError::Stopped);
        }
        let existing: Vec<_> = slots
            .iter()
            .filter(|slot| slot.workspace.workspace == workspace.workspace)
            .cloned()
            .collect();
        if !existing.is_empty() {
            if existing.iter().any(|slot| slot.workspace != *workspace) {
                return Err(ProviderPoolWorkspaceError::IdentityConflict {
                    workspace: workspace.workspace.clone(),
                });
            }
            return Ok(self.inner.wrappers(existing));
        }
        let created: Vec<_> = self
            .inner
            .registrations
            .iter()
            .map(|registration| {
                Arc::new(ProviderSlot {
                    workspace: workspace.clone(),
                    registration: registration.clone(),
                    runtime: Mutex::new(SlotRuntime::default()),
                    changed: Condvar::new(),
                })
            })
            .collect();
        slots.extend(created.iter().cloned());
        Ok(self.inner.wrappers(created))
    }

    pub fn metrics(&self) -> ProviderOrchestrationMetrics {
        self.inner.metrics_for(None)
    }

    /// Stops admission, waits boundedly for admitted work, then joins the
    /// reaper and every provider-owned worker/process.
    pub fn shutdown(&self) -> Result<(), ProviderPoolShutdownError> {
        self.inner.stopped.store(true, Ordering::Release);
        self.inner.changed.notify_all();
        let slots = self.inner.slots();
        for slot in &slots {
            slot.changed.notify_all();
        }

        let deadline = Instant::now().checked_add(self.inner.config.shutdown_timeout);
        let mut failures = Vec::new();
        let mut state = lock(&self.inner.state);
        while state.running_queries > 0 {
            let Some(deadline) = deadline else {
                break;
            };
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let (next, _) = wait_timeout(&self.inner.changed, state, remaining);
            state = next;
        }
        if state.running_queries > 0 {
            failures.push(ProviderPoolShutdownError::RunningQueries {
                running_queries: state.running_queries,
            });
        }
        drop(state);

        if let Some(reaper) = lock(&self.reaper).take()
            && reaper.join().is_err()
        {
            failures.push(ProviderPoolShutdownError::ReaperPanicked);
        }
        for slot in slots {
            if let Err(error) = self.inner.evict_provider(&slot, EvictionCause::Shutdown) {
                failures.push(ProviderPoolShutdownError::Provider {
                    message: error.to_string(),
                });
            }
        }
        match failures.len() {
            0 => Ok(()),
            1 => Err(failures.remove(0)),
            _ => Err(ProviderPoolShutdownError::Multiple {
                messages: failures
                    .into_iter()
                    .map(|failure| failure.to_string())
                    .collect(),
            }),
        }
    }
}

impl Drop for ProviderPool {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct PoolInner {
    config: ProviderPoolConfig,
    registrations: Vec<Arc<ProviderRegistration>>,
    slots: Mutex<Vec<Arc<ProviderSlot>>>,
    state: Mutex<PoolState>,
    changed: Condvar,
    stopped: AtomicBool,
}

impl fmt::Debug for PoolInner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PoolInner")
            .field("config", &self.config)
            .field("registrations", &self.registrations)
            .field("slots", &lock(&self.slots).len())
            .field("stopped", &self.stopped.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct PoolState {
    active_providers: usize,
    reserved_memory_bytes: u64,
    workspace_usage: HashMap<WorkspaceId, WorkspaceUsage>,
    running_queries: usize,
    waiters: Vec<Waiter>,
    next_sequence: u64,
    activations: u64,
    activation_failures: u64,
    activation_timeouts: u64,
    idle_shutdowns: u64,
    resource_evictions: u64,
    shutdown_failures: u64,
    saturated_queries: u64,
    queue_timeouts: u64,
    cancelled_queries: u64,
    queue_latency: [QueueLatencyStats; ProviderRequestPriority::COUNT],
}

#[derive(Debug, Default)]
struct WorkspaceUsage {
    active_providers: usize,
    reserved_memory_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct Waiter {
    sequence: u64,
    priority: ProviderRequestPriority,
    enqueued: Instant,
    /// Admission-based aging: every admission while this waiter is queued
    /// raises it one level, capped at `Interactive` (issue #44 fairness). The
    /// cap is what converges: already-top waiters cannot rise further, so a
    /// `Background` waiter reaches top rank after two admissions. Once tied,
    /// bounded FIFO sequence ordering prevents newer interactive arrivals
    /// from overtaking it; older queued work may still precede it.
    boost: u8,
}

impl Waiter {
    fn effective_rank(&self) -> u8 {
        (self.priority.index() as u8)
            .saturating_add(self.boost)
            .min((ProviderRequestPriority::COUNT - 1) as u8)
    }
}

struct ProviderSlot {
    workspace: WorkspaceIdentity,
    registration: Arc<ProviderRegistration>,
    runtime: Mutex<SlotRuntime>,
    changed: Condvar,
}

impl fmt::Debug for ProviderSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderSlot")
            .field("workspace", &self.workspace.workspace)
            .field("registration", &self.registration)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct SlotRuntime {
    provider: Option<Arc<dyn PreciseProvider>>,
    activating: bool,
    in_flight: usize,
    last_used: Instant,
    consecutive_failures: u32,
    retry_after: Option<Instant>,
    last_error: Option<String>,
}

impl Default for SlotRuntime {
    fn default() -> Self {
        Self {
            provider: None,
            activating: false,
            in_flight: 0,
            last_used: Instant::now(),
            consecutive_failures: 0,
            retry_after: None,
            last_error: None,
        }
    }
}

#[derive(Debug)]
struct PooledProvider {
    inner: Arc<PoolInner>,
    slot: Arc<ProviderSlot>,
}

impl PreciseProvider for PooledProvider {
    fn name(&self) -> &'static str {
        self.slot.registration.name
    }

    fn supports(&self, language: Language) -> bool {
        self.slot.registration.languages.contains(&language)
    }

    fn supports_path(
        &self,
        language: Language,
        path: &chakra_domain::location::RepoRelativePath,
    ) -> bool {
        self.slot.registration.supports_path(language, path)
    }

    fn state_for(&self, revision: Revision) -> ProviderState {
        if self.inner.stopped.load(Ordering::Acquire) {
            return ProviderState::Degraded;
        }
        let runtime = lock(&self.slot.runtime);
        if runtime.last_error.is_some() && runtime.provider.is_some() {
            ProviderState::Degraded
        } else if let Some(provider) = &runtime.provider {
            provider.state_for(revision)
        } else if runtime.activating {
            ProviderState::Initializing
        } else if runtime
            .retry_after
            .is_some_and(|retry| retry > Instant::now())
        {
            ProviderState::Degraded
        } else {
            ProviderState::Dormant
        }
    }

    fn last_error(&self) -> Option<String> {
        let runtime = lock(&self.slot.runtime);
        runtime.last_error.clone().or_else(|| {
            runtime
                .provider
                .as_ref()
                .and_then(|provider| provider.last_error())
        })
    }

    fn progress(&self) -> Option<ProviderProgress> {
        lock(&self.slot.runtime)
            .provider
            .as_ref()
            .and_then(|provider| provider.progress())
    }

    fn metrics(&self) -> Option<ProviderMetrics> {
        let metrics = lock(&self.slot.runtime)
            .provider
            .as_ref()
            .and_then(|provider| provider.metrics())
            .unwrap_or_default();
        Some(metrics)
    }

    fn orchestration_metrics(&self) -> Option<ProviderOrchestrationMetrics> {
        Some(self.inner.metrics_for(Some(&self.slot.workspace.workspace)))
    }

    fn query_wait_budget(&self) -> Option<Duration> {
        Some(
            self.inner
                .config
                .query_queue_timeout
                .saturating_add(self.slot.registration.additional_wait_budget),
        )
    }

    fn shutdown(&self) -> Result<(), ProviderShutdownError> {
        let _ = self
            .inner
            .evict_provider(&self.slot, EvictionCause::Shutdown)?;
        Ok(())
    }

    fn enrich_with_context(
        &self,
        request: PreciseQueryRequest,
        operation: &OperationContext,
    ) -> PreciseQueryResult {
        let revision = request.workspace.revision;
        if !self.supports(request.symbol.language)
            || request.workspace.repository_root != self.slot.workspace.root
        {
            return PreciseQueryResult::unavailable_because(
                revision,
                ProviderState::Degraded,
                ProviderFallbackCause::ActivationFailed,
            );
        }
        let _query = match self.inner.admit(request.priority, operation) {
            Ok(permit) => permit,
            Err(AdmissionFailure::Stopped) => {
                return PreciseQueryResult::unavailable_because(
                    revision,
                    ProviderState::Degraded,
                    ProviderFallbackCause::ProviderStopped,
                );
            }
            Err(AdmissionFailure::Saturated) => {
                return PreciseQueryResult::unavailable_because(
                    revision,
                    ProviderState::CatchingUp,
                    ProviderFallbackCause::QueueSaturated,
                );
            }
            Err(AdmissionFailure::QueueTimeout) => {
                return PreciseQueryResult::unavailable_because(
                    revision,
                    ProviderState::CatchingUp,
                    ProviderFallbackCause::QueueTimedOut,
                );
            }
            Err(AdmissionFailure::Cancelled) => {
                return PreciseQueryResult::unavailable_because(
                    revision,
                    ProviderState::CatchingUp,
                    ProviderFallbackCause::Cancelled,
                );
            }
        };
        let provider = match self
            .inner
            .activate(&self.slot, &request.workspace, operation)
        {
            Ok(provider) => provider,
            Err(ActivationFailure::Stopped) => {
                return PreciseQueryResult::unavailable_because(
                    revision,
                    ProviderState::Degraded,
                    ProviderFallbackCause::ProviderStopped,
                );
            }
            Err(ActivationFailure::StartFailed) => {
                return PreciseQueryResult::unavailable_because(
                    revision,
                    ProviderState::Degraded,
                    ProviderFallbackCause::ActivationFailed,
                );
            }
            Err(ActivationFailure::Capacity) => {
                return PreciseQueryResult::unavailable_because(
                    revision,
                    ProviderState::CatchingUp,
                    ProviderFallbackCause::ActivationCapacity,
                );
            }
            Err(ActivationFailure::Cancelled) => {
                return PreciseQueryResult::unavailable_because(
                    revision,
                    ProviderState::CatchingUp,
                    ProviderFallbackCause::Cancelled,
                );
            }
            Err(ActivationFailure::TimedOut) => {
                return PreciseQueryResult::unavailable_because(
                    revision,
                    ProviderState::CatchingUp,
                    ProviderFallbackCause::ActivationTimedOut,
                );
            }
        };
        if let Err(abort) = operation.check() {
            self.inner.record_activation_abort(abort);
            let cause = match abort {
                OperationAbort::Cancelled => ProviderFallbackCause::Cancelled,
                OperationAbort::DeadlineExceeded => ProviderFallbackCause::ActivationTimedOut,
            };
            return PreciseQueryResult::unavailable_because(
                revision,
                ProviderState::CatchingUp,
                cause,
            );
        }
        provider.provider.enrich_with_context(request, operation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionFailure {
    Stopped,
    Saturated,
    QueueTimeout,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationFailure {
    Stopped,
    Capacity,
    Cancelled,
    TimedOut,
    StartFailed,
}

impl PoolInner {
    fn slots(&self) -> Vec<Arc<ProviderSlot>> {
        lock(&self.slots).clone()
    }

    fn wrappers(self: &Arc<Self>, slots: Vec<Arc<ProviderSlot>>) -> Vec<Arc<dyn PreciseProvider>> {
        slots
            .into_iter()
            .map(|slot| {
                Arc::new(PooledProvider {
                    inner: self.clone(),
                    slot,
                }) as Arc<dyn PreciseProvider>
            })
            .collect()
    }

    fn admit(
        self: &Arc<Self>,
        priority: ProviderRequestPriority,
        operation: &OperationContext,
    ) -> Result<QueryPermit, AdmissionFailure> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(AdmissionFailure::Stopped);
        }
        let queue_operation = operation.bounded_by(self.config.query_queue_timeout);
        if let Err(abort) = queue_operation.check() {
            let mut state = lock(&self.state);
            return match abort {
                OperationAbort::Cancelled => {
                    state.cancelled_queries = state.cancelled_queries.saturating_add(1);
                    Err(AdmissionFailure::Cancelled)
                }
                OperationAbort::DeadlineExceeded => {
                    state.queue_timeouts = state.queue_timeouts.saturating_add(1);
                    Err(AdmissionFailure::QueueTimeout)
                }
            };
        }
        let mut state = lock(&self.state);
        if state.running_queries < self.config.max_concurrent_queries && state.waiters.is_empty() {
            state.running_queries += 1;
            return Ok(QueryPermit {
                inner: self.clone(),
            });
        }
        if state.waiters.len() >= self.config.max_queued_queries {
            state.saturated_queries = state.saturated_queries.saturating_add(1);
            return Err(AdmissionFailure::Saturated);
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        state.waiters.push(Waiter {
            sequence,
            priority,
            enqueued: Instant::now(),
            boost: 0,
        });
        loop {
            if self.stopped.load(Ordering::Acquire) {
                remove_waiter(&mut state.waiters, sequence);
                return Err(AdmissionFailure::Stopped);
            }
            match queue_operation.check() {
                Ok(()) => {}
                Err(OperationAbort::Cancelled) => {
                    remove_waiter(&mut state.waiters, sequence);
                    state.cancelled_queries = state.cancelled_queries.saturating_add(1);
                    self.changed.notify_all();
                    return Err(AdmissionFailure::Cancelled);
                }
                Err(OperationAbort::DeadlineExceeded) => {
                    remove_waiter(&mut state.waiters, sequence);
                    state.queue_timeouts = state.queue_timeouts.saturating_add(1);
                    self.changed.notify_all();
                    return Err(AdmissionFailure::QueueTimeout);
                }
            }
            let is_next = best_waiter(&state.waiters).is_some_and(|waiter| waiter == sequence);
            if is_next && state.running_queries < self.config.max_concurrent_queries {
                if let Some(admitted) = remove_waiter(&mut state.waiters, sequence) {
                    state.queue_latency[admitted.priority.index()]
                        .record(admitted.enqueued.elapsed());
                }
                age_waiters(&mut state.waiters);
                state.running_queries += 1;
                return Ok(QueryPermit {
                    inner: self.clone(),
                });
            }
            let wait = queue_operation
                .poll_timeout(ADMISSION_POLL_INTERVAL)
                .unwrap_or(Duration::ZERO);
            let (next, _) = wait_timeout(&self.changed, state, wait);
            state = next;
        }
    }

    fn activate(
        self: &Arc<Self>,
        slot: &Arc<ProviderSlot>,
        workspace: &ProviderWorkspace,
        operation: &OperationContext,
    ) -> Result<ProviderLease, ActivationFailure> {
        loop {
            operation
                .check()
                .map_err(|abort| self.record_activation_abort(abort))?;
            if self.stopped.load(Ordering::Acquire) {
                return Err(ActivationFailure::Stopped);
            }
            let mut runtime = lock(&slot.runtime);
            if runtime.provider.is_some() && runtime.last_error.is_some() {
                return Err(ActivationFailure::StartFailed);
            }
            if let Some(provider) = runtime.provider.clone() {
                runtime.in_flight += 1;
                runtime.last_used = Instant::now();
                return Ok(ProviderLease {
                    inner: self.clone(),
                    slot: slot.clone(),
                    provider,
                });
            }
            if runtime
                .retry_after
                .is_some_and(|retry| retry > Instant::now())
            {
                return Err(ActivationFailure::StartFailed);
            }
            if runtime.activating {
                let wait = operation
                    .poll_timeout(ADMISSION_POLL_INTERVAL)
                    .map_err(|abort| self.record_activation_abort(abort))?;
                let (next, _) = wait_timeout(&slot.changed, runtime, wait);
                drop(next);
                continue;
            }
            runtime.activating = true;
            drop(runtime);
            break;
        }

        if let Err(failure) = self.reserve_capacity(slot) {
            let mut runtime = lock(&slot.runtime);
            runtime.activating = false;
            runtime.last_error = Some("provider pool has no reclaimable capacity".to_owned());
            slot.changed.notify_all();
            return Err(failure);
        }

        let started = (slot.registration.factory)(workspace.clone(), operation);
        let mut runtime = lock(&slot.runtime);
        runtime.activating = false;
        match started {
            Ok(provider)
                if provider.name() == slot.registration.name
                    && slot
                        .registration
                        .languages
                        .iter()
                        .all(|language| provider.supports(*language)) =>
            {
                if self.stopped.load(Ordering::Acquire) {
                    drop(runtime);
                    match provider.shutdown() {
                        Ok(()) => self.release_reservation(slot),
                        Err(error) => self.retain_after_shutdown_failure(
                            slot,
                            provider,
                            format!(
                                "provider activated after pool stopped; shutdown failed: {error}"
                            ),
                            false,
                        ),
                    }
                    slot.changed.notify_all();
                    return Err(ActivationFailure::Stopped);
                }
                runtime.provider = Some(provider.clone());
                runtime.in_flight = 1;
                runtime.last_used = Instant::now();
                runtime.consecutive_failures = 0;
                runtime.retry_after = None;
                runtime.last_error = None;
                let mut state = lock(&self.state);
                state.activations = state.activations.saturating_add(1);
                drop(state);
                slot.changed.notify_all();
                Ok(ProviderLease {
                    inner: self.clone(),
                    slot: slot.clone(),
                    provider,
                })
            }
            Ok(provider) => {
                const MESSAGE: &str = "provider factory returned an incompatible adapter";
                runtime.activating = true;
                drop(runtime);
                match provider.shutdown() {
                    Ok(()) => {
                        let mut runtime = lock(&slot.runtime);
                        runtime.activating = false;
                        self.record_activation_failure(slot, &mut runtime, MESSAGE.to_owned());
                    }
                    Err(error) => self.retain_after_shutdown_failure(
                        slot,
                        provider,
                        format!("{MESSAGE}; shutdown failed: {error}"),
                        true,
                    ),
                }
                Err(ActivationFailure::StartFailed)
            }
            Err(error) => {
                if let Some(abort) = error.abort() {
                    runtime.last_error = None;
                    self.release_reservation(slot);
                    slot.changed.notify_all();
                    return Err(self.record_activation_abort(abort));
                }
                self.record_activation_failure(slot, &mut runtime, error.to_string());
                Err(ActivationFailure::StartFailed)
            }
        }
    }

    fn record_activation_failure(
        &self,
        slot: &Arc<ProviderSlot>,
        runtime: &mut SlotRuntime,
        message: String,
    ) {
        self.apply_activation_backoff(runtime, message);
        self.release_reservation(slot);
        let mut state = lock(&self.state);
        state.activation_failures = state.activation_failures.saturating_add(1);
        slot.changed.notify_all();
    }

    fn apply_activation_backoff(&self, runtime: &mut SlotRuntime, message: String) {
        runtime.consecutive_failures = runtime.consecutive_failures.saturating_add(1);
        let exponent = runtime.consecutive_failures.saturating_sub(1).min(31);
        let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
        let delay = self
            .config
            .activation_backoff_base
            .saturating_mul(multiplier)
            .min(self.config.activation_backoff_max);
        runtime.retry_after = Instant::now().checked_add(delay);
        runtime.last_error = Some(message);
    }

    fn retain_after_shutdown_failure(
        &self,
        slot: &Arc<ProviderSlot>,
        provider: Arc<dyn PreciseProvider>,
        message: String,
        activation_failed: bool,
    ) {
        let mut runtime = lock(&slot.runtime);
        runtime.provider = Some(provider);
        runtime.activating = false;
        runtime.last_used = Instant::now();
        if activation_failed {
            self.apply_activation_backoff(&mut runtime, message);
        } else {
            runtime.last_error = Some(message);
        }
        slot.changed.notify_all();
        drop(runtime);
        let mut state = lock(&self.state);
        state.shutdown_failures = state.shutdown_failures.saturating_add(1);
        if activation_failed {
            state.activation_failures = state.activation_failures.saturating_add(1);
        }
    }

    fn record_activation_abort(&self, abort: OperationAbort) -> ActivationFailure {
        let mut state = lock(&self.state);
        match abort {
            OperationAbort::Cancelled => {
                state.cancelled_queries = state.cancelled_queries.saturating_add(1);
                ActivationFailure::Cancelled
            }
            OperationAbort::DeadlineExceeded => {
                state.activation_timeouts = state.activation_timeouts.saturating_add(1);
                ActivationFailure::TimedOut
            }
        }
    }

    fn reserve_capacity(&self, slot: &Arc<ProviderSlot>) -> Result<(), ActivationFailure> {
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return Err(ActivationFailure::Stopped);
            }
            let reservation = slot.registration.reserved_memory_bytes;
            let mut state = lock(&self.state);
            let global_active_available = state.active_providers < self.config.max_active_providers;
            let global_memory_available = state.reserved_memory_bytes.saturating_add(reservation)
                <= self.config.max_reserved_memory_bytes;
            let (workspace_active, workspace_memory) = state
                .workspace_usage
                .get(&slot.workspace.workspace)
                .map(|usage| (usage.active_providers, usage.reserved_memory_bytes))
                .unwrap_or_default();
            let workspace_active_available =
                workspace_active < self.config.max_active_providers_per_workspace;
            let workspace_memory_available = workspace_memory.saturating_add(reservation)
                <= self.config.max_reserved_memory_bytes_per_workspace;
            if global_active_available
                && global_memory_available
                && workspace_active_available
                && workspace_memory_available
            {
                state.active_providers += 1;
                state.reserved_memory_bytes =
                    state.reserved_memory_bytes.saturating_add(reservation);
                let usage = state
                    .workspace_usage
                    .entry(slot.workspace.workspace.clone())
                    .or_default();
                usage.active_providers += 1;
                usage.reserved_memory_bytes =
                    usage.reserved_memory_bytes.saturating_add(reservation);
                return Ok(());
            }
            let workspace_limited = !workspace_active_available || !workspace_memory_available;
            drop(state);
            let workspace_filter = workspace_limited.then_some(&slot.workspace.workspace);
            let Some(victim) = self.oldest_evictable_slot(slot, workspace_filter) else {
                let mut state = lock(&self.state);
                state.saturated_queries = state.saturated_queries.saturating_add(1);
                return Err(ActivationFailure::Capacity);
            };
            match self.evict_provider(&victim, EvictionCause::Resource) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => {
                    let mut state = lock(&self.state);
                    state.saturated_queries = state.saturated_queries.saturating_add(1);
                    return Err(ActivationFailure::Capacity);
                }
            }
        }
    }

    fn oldest_evictable_slot(
        &self,
        excluded: &Arc<ProviderSlot>,
        workspace: Option<&WorkspaceId>,
    ) -> Option<Arc<ProviderSlot>> {
        self.slots()
            .into_iter()
            .filter(|slot| !Arc::ptr_eq(slot, excluded))
            .filter(|slot| workspace.is_none_or(|workspace| slot.workspace.workspace == *workspace))
            .filter_map(|slot| {
                let runtime = lock(&slot.runtime);
                (runtime.provider.is_some() && runtime.in_flight == 0 && !runtime.activating)
                    .then_some((slot.clone(), runtime.last_used))
            })
            .min_by_key(|(_, last_used)| *last_used)
            .map(|(slot, _)| slot)
    }

    fn evict_provider(
        &self,
        slot: &Arc<ProviderSlot>,
        cause: EvictionCause,
    ) -> Result<bool, ProviderShutdownError> {
        let mut runtime = lock(&slot.runtime);
        if cause != EvictionCause::Shutdown && (runtime.in_flight > 0 || runtime.activating) {
            return Ok(false);
        }
        let Some(provider) = runtime.provider.take() else {
            return Ok(false);
        };
        runtime.activating = true;
        drop(runtime);

        if let Err(error) = provider.shutdown() {
            let mut runtime = lock(&slot.runtime);
            runtime.provider = Some(provider);
            runtime.activating = false;
            runtime.last_error = Some(format!("provider shutdown failed: {error}"));
            slot.changed.notify_all();
            let mut state = lock(&self.state);
            state.shutdown_failures = state.shutdown_failures.saturating_add(1);
            return Err(error);
        }

        let mut runtime = lock(&slot.runtime);
        runtime.activating = false;
        runtime.last_used = Instant::now();
        runtime.last_error = None;
        slot.changed.notify_all();
        drop(runtime);
        self.release_reservation(slot);
        let mut state = lock(&self.state);
        match cause {
            EvictionCause::Idle => {
                state.idle_shutdowns = state.idle_shutdowns.saturating_add(1);
            }
            EvictionCause::Resource => {
                state.resource_evictions = state.resource_evictions.saturating_add(1);
            }
            EvictionCause::Shutdown => {}
        }
        Ok(true)
    }

    fn release_reservation(&self, slot: &Arc<ProviderSlot>) {
        let reservation = slot.registration.reserved_memory_bytes;
        let mut state = lock(&self.state);
        state.active_providers = state.active_providers.saturating_sub(1);
        state.reserved_memory_bytes = state.reserved_memory_bytes.saturating_sub(reservation);
        let remove_usage = if let Some(usage) =
            state.workspace_usage.get_mut(&slot.workspace.workspace)
        {
            usage.active_providers = usage.active_providers.saturating_sub(1);
            usage.reserved_memory_bytes = usage.reserved_memory_bytes.saturating_sub(reservation);
            usage.active_providers == 0 && usage.reserved_memory_bytes == 0
        } else {
            false
        };
        if remove_usage {
            state.workspace_usage.remove(&slot.workspace.workspace);
        }
        self.changed.notify_all();
    }

    fn metrics_for(&self, workspace: Option<&WorkspaceId>) -> ProviderOrchestrationMetrics {
        let slots = self.slots();
        let mut configured_workspaces: Vec<_> = slots
            .iter()
            .map(|slot| slot.workspace.workspace.as_str())
            .collect();
        configured_workspaces.sort_unstable();
        configured_workspaces.dedup();
        let state = lock(&self.state);
        let workspace_metrics = workspace.map(|workspace| {
            let usage = state.workspace_usage.get(workspace);
            WorkspaceProviderOrchestrationMetrics {
                active_providers: usage.map_or(0, |usage| usage.active_providers as u64),
                max_active_providers: self.config.max_active_providers_per_workspace as u64,
                reserved_memory_bytes: usage.map_or(0, |usage| usage.reserved_memory_bytes),
                max_reserved_memory_bytes: self.config.max_reserved_memory_bytes_per_workspace,
            }
        });
        ProviderOrchestrationMetrics {
            configured_providers: self.registrations.len() as u64,
            configured_workspaces: configured_workspaces.len() as u64,
            active_providers: state.active_providers as u64,
            max_active_providers: self.config.max_active_providers as u64,
            reserved_memory_bytes: state.reserved_memory_bytes,
            max_reserved_memory_bytes: self.config.max_reserved_memory_bytes,
            running_queries: state.running_queries as u64,
            queued_queries: state.waiters.len() as u64,
            max_concurrent_queries: self.config.max_concurrent_queries as u64,
            max_queued_queries: self.config.max_queued_queries as u64,
            activations: state.activations,
            activation_failures: state.activation_failures,
            activation_timeouts: state.activation_timeouts,
            idle_shutdowns: state.idle_shutdowns,
            resource_evictions: state.resource_evictions,
            shutdown_failures: state.shutdown_failures,
            saturated_queries: state.saturated_queries,
            queue_timeouts: state.queue_timeouts,
            cancelled_queries: state.cancelled_queries,
            queue_latency_by_priority: ProviderQueueLatencyByPriority {
                background: state.queue_latency[ProviderRequestPriority::Background.index()],
                normal: state.queue_latency[ProviderRequestPriority::Normal.index()],
                interactive: state.queue_latency[ProviderRequestPriority::Interactive.index()],
            },
            workspace: workspace_metrics,
        }
    }
}

#[derive(Debug)]
struct QueryPermit {
    inner: Arc<PoolInner>,
}

impl Drop for QueryPermit {
    fn drop(&mut self) {
        let mut state = lock(&self.inner.state);
        state.running_queries = state.running_queries.saturating_sub(1);
        self.inner.changed.notify_all();
    }
}

#[derive(Debug)]
struct ProviderLease {
    inner: Arc<PoolInner>,
    slot: Arc<ProviderSlot>,
    provider: Arc<dyn PreciseProvider>,
}

impl Drop for ProviderLease {
    fn drop(&mut self) {
        let mut runtime = lock(&self.slot.runtime);
        runtime.in_flight = runtime.in_flight.saturating_sub(1);
        runtime.last_used = Instant::now();
        self.slot.changed.notify_all();
        self.inner.changed.notify_all();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvictionCause {
    Idle,
    Resource,
    Shutdown,
}

fn validate(
    config: &ProviderPoolConfig,
    registrations: &[ProviderRegistration],
) -> Result<(), ProviderPoolConfigError> {
    if config.max_active_providers == 0
        || config.max_active_providers_per_workspace == 0
        || config.max_reserved_memory_bytes == 0
        || config.max_reserved_memory_bytes_per_workspace == 0
        || config.max_concurrent_queries == 0
        || config.max_queued_queries == 0
        || config.query_queue_timeout.is_zero()
        || config.idle_timeout.is_zero()
        || config.idle_poll_interval.is_zero()
        || config.activation_backoff_base.is_zero()
        || config.activation_backoff_max.is_zero()
        || config.shutdown_timeout.is_zero()
    {
        return Err(ProviderPoolConfigError::ZeroBound);
    }
    if config.activation_backoff_max < config.activation_backoff_base {
        return Err(ProviderPoolConfigError::InvalidBackoff);
    }
    for (index, registration) in registrations.iter().enumerate() {
        if registration.languages.is_empty() {
            return Err(ProviderPoolConfigError::NoLanguages {
                provider: registration.name.to_owned(),
            });
        }
        if registration.reserved_memory_bytes == 0 {
            return Err(ProviderPoolConfigError::ZeroReservation {
                provider: registration.name.to_owned(),
            });
        }
        if registration.reserved_memory_bytes > config.max_reserved_memory_bytes {
            return Err(ProviderPoolConfigError::ReservationExceedsPool {
                provider: registration.name.to_owned(),
                reserved: registration.reserved_memory_bytes,
                maximum: config.max_reserved_memory_bytes,
            });
        }
        if registration.reserved_memory_bytes > config.max_reserved_memory_bytes_per_workspace {
            return Err(ProviderPoolConfigError::ReservationExceedsWorkspace {
                provider: registration.name.to_owned(),
                reserved: registration.reserved_memory_bytes,
                maximum: config.max_reserved_memory_bytes_per_workspace,
            });
        }
        for (language_index, language) in registration.languages.iter().enumerate() {
            if registration.languages[..language_index].contains(language) {
                return Err(ProviderPoolConfigError::DuplicateLanguage {
                    provider: registration.name.to_owned(),
                    language: *language,
                });
            }
        }
        for previous in &registrations[..index] {
            if previous.name == registration.name {
                return Err(ProviderPoolConfigError::DuplicateProvider {
                    provider: registration.name.to_owned(),
                });
            }
            if let Some(language) = registration
                .languages
                .iter()
                .find(|language| previous.languages.contains(language))
            {
                return Err(ProviderPoolConfigError::LanguageConflict {
                    language: *language,
                    first: previous.name.to_owned(),
                    second: registration.name.to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn reaper_loop(inner: Arc<PoolInner>) {
    while !inner.stopped.load(Ordering::Acquire) {
        let state = lock(&inner.state);
        let (state, _) = wait_timeout(&inner.changed, state, inner.config.idle_poll_interval);
        drop(state);
        if inner.stopped.load(Ordering::Acquire) {
            break;
        }
        for slot in inner.slots() {
            let should_evict = {
                let runtime = lock(&slot.runtime);
                runtime.provider.is_some()
                    && runtime.in_flight == 0
                    && !runtime.activating
                    && runtime.last_used.elapsed() >= inner.config.idle_timeout
            };
            if should_evict {
                // The slot retains the provider reservation and last error;
                // a later reaper pass or final shutdown retries cleanup.
                let _ = inner.evict_provider(&slot, EvictionCause::Idle);
            }
        }
    }
}

fn best_waiter(waiters: &[Waiter]) -> Option<u64> {
    waiters
        .iter()
        .max_by(|left, right| {
            left.effective_rank()
                .cmp(&right.effective_rank())
                .then_with(|| right.sequence.cmp(&left.sequence))
        })
        .map(|waiter| waiter.sequence)
}

fn age_waiters(waiters: &mut [Waiter]) {
    for waiter in waiters {
        waiter.boost = waiter.boost.saturating_add(1);
    }
}

fn remove_waiter(waiters: &mut Vec<Waiter>, sequence: u64) -> Option<Waiter> {
    let index = waiters
        .iter()
        .position(|waiter| waiter.sequence == sequence)?;
    Some(waiters.swap_remove(index))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn wait_timeout<'a, T>(
    condvar: &Condvar,
    guard: std::sync::MutexGuard<'a, T>,
    timeout: Duration,
) -> (std::sync::MutexGuard<'a, T>, bool) {
    match condvar.wait_timeout(guard, timeout) {
        Ok((guard, result)) => (guard, result.timed_out()),
        Err(poisoned) => {
            let (guard, result) = poisoned.into_inner();
            (guard, result.timed_out())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::mpsc;

    use chakra_domain::identity::RepositoryId;
    use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
    use chakra_engine::{CallHierarchyDirections, ProviderDocument, ProviderSymbol};

    use super::*;

    type TestError = Box<dyn Error + Send + Sync>;

    #[derive(Debug)]
    struct FakeProvider {
        name: &'static str,
        languages: Vec<Language>,
        simultaneous: Option<Arc<Barrier>>,
        active_queries: Arc<AtomicUsize>,
        max_active_queries: Arc<AtomicUsize>,
        order: Arc<Mutex<Vec<String>>>,
        gate: Arc<(Mutex<bool>, Condvar)>,
        shutdowns: Arc<AtomicUsize>,
        stopped: AtomicBool,
    }

    impl PreciseProvider for FakeProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supports(&self, language: Language) -> bool {
            self.languages.contains(&language)
        }

        fn state_for(&self, _revision: Revision) -> ProviderState {
            if self.stopped.load(Ordering::Acquire) {
                ProviderState::Degraded
            } else {
                ProviderState::Ready
            }
        }

        fn shutdown(&self) -> Result<(), ProviderShutdownError> {
            if !self.stopped.swap(true, Ordering::AcqRel) {
                self.shutdowns.fetch_add(1, Ordering::AcqRel);
            }
            Ok(())
        }

        fn enrich_with_context(
            &self,
            request: PreciseQueryRequest,
            operation: &OperationContext,
        ) -> PreciseQueryResult {
            let current = self.active_queries.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_active_queries.fetch_max(current, Ordering::AcqRel);
            lock(&self.order).push(request.symbol.name.clone());
            if let Some(barrier) = &self.simultaneous {
                barrier.wait();
            }
            if request.symbol.name == "hold" {
                let (released, changed) = &*self.gate;
                let mut released = lock(released);
                while !*released {
                    if operation.check().is_err() {
                        self.active_queries.fetch_sub(1, Ordering::AcqRel);
                        return PreciseQueryResult::unavailable(
                            request.workspace.revision,
                            ProviderState::CatchingUp,
                        );
                    }
                    let (next, _) = wait_timeout(changed, released, ADMISSION_POLL_INTERVAL);
                    released = next;
                }
            }
            self.active_queries.fetch_sub(1, Ordering::AcqRel);
            PreciseQueryResult {
                revision: request.workspace.revision,
                state: ProviderState::Ready,
                fallback_cause: None,
                incoming: Vec::new(),
                outgoing: Vec::new(),
                incoming_truncated: false,
                outgoing_truncated: false,
            }
        }
    }

    #[derive(Debug)]
    struct FailsFirstShutdownProvider {
        name: &'static str,
        attempts: Arc<AtomicUsize>,
        queries: Arc<AtomicUsize>,
    }

    impl PreciseProvider for FailsFirstShutdownProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supports(&self, language: Language) -> bool {
            language == Language::Rust
        }

        fn state_for(&self, _revision: Revision) -> ProviderState {
            ProviderState::Ready
        }

        fn shutdown(&self) -> Result<(), ProviderShutdownError> {
            if self.attempts.fetch_add(1, Ordering::AcqRel) == 0 {
                Err(ProviderShutdownError::new("synthetic cleanup failure"))
            } else {
                Ok(())
            }
        }

        fn enrich_with_context(
            &self,
            request: PreciseQueryRequest,
            _operation: &OperationContext,
        ) -> PreciseQueryResult {
            self.queries.fetch_add(1, Ordering::AcqRel);
            PreciseQueryResult {
                revision: request.workspace.revision,
                state: ProviderState::Ready,
                fallback_cause: None,
                incoming: Vec::new(),
                outgoing: Vec::new(),
                incoming_truncated: false,
                outgoing_truncated: false,
            }
        }
    }

    #[derive(Clone)]
    struct FakeControls {
        simultaneous: Option<Arc<Barrier>>,
        active_queries: Arc<AtomicUsize>,
        max_active_queries: Arc<AtomicUsize>,
        order: Arc<Mutex<Vec<String>>>,
        gate: Arc<(Mutex<bool>, Condvar)>,
        activations: Arc<AtomicUsize>,
        activation_roots: Arc<Mutex<Vec<PathBuf>>>,
        shutdowns: Arc<AtomicUsize>,
    }

    impl FakeControls {
        fn new(simultaneous: Option<Arc<Barrier>>) -> Self {
            Self {
                simultaneous,
                active_queries: Arc::new(AtomicUsize::new(0)),
                max_active_queries: Arc::new(AtomicUsize::new(0)),
                order: Arc::new(Mutex::new(Vec::new())),
                gate: Arc::new((Mutex::new(false), Condvar::new())),
                activations: Arc::new(AtomicUsize::new(0)),
                activation_roots: Arc::new(Mutex::new(Vec::new())),
                shutdowns: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn registration(
            &self,
            name: &'static str,
            languages: Vec<Language>,
            reservation: u64,
        ) -> ProviderRegistration {
            let controls = self.clone();
            ProviderRegistration::new(
                name,
                languages.clone(),
                reservation,
                move |workspace, _operation| {
                    controls.activations.fetch_add(1, Ordering::AcqRel);
                    lock(&controls.activation_roots).push(workspace.repository_root);
                    Ok(Arc::new(FakeProvider {
                        name,
                        languages: languages.clone(),
                        simultaneous: controls.simultaneous.clone(),
                        active_queries: controls.active_queries.clone(),
                        max_active_queries: controls.max_active_queries.clone(),
                        order: controls.order.clone(),
                        gate: controls.gate.clone(),
                        shutdowns: controls.shutdowns.clone(),
                        stopped: AtomicBool::new(false),
                    }) as Arc<dyn PreciseProvider>)
                },
            )
        }

        fn release(&self) {
            let (released, changed) = &*self.gate;
            *lock(released) = true;
            changed.notify_all();
        }
    }

    fn request(
        language: Language,
        name: &str,
        priority: ProviderRequestPriority,
    ) -> Result<PreciseQueryRequest, TestError> {
        request_in(&std::env::current_dir()?, language, name, priority)
    }

    fn request_in(
        root: &Path,
        language: Language,
        name: &str,
        priority: ProviderRequestPriority,
    ) -> Result<PreciseQueryRequest, TestError> {
        let path = match language {
            Language::Rust => "src/lib.rs",
            Language::Php => "src/index.php",
            Language::TypeScript => "src/index.ts",
            Language::Python => "src/index.py",
            Language::JavaScript => "src/index.js",
            Language::Java => "src/Main.java",
            Language::CSharp => "src/Program.cs",
            Language::Shell => "src/main.sh",
            Language::Cpp => "src/main.cpp",
            Language::Hcl => "main.tf",
            Language::Go => "main.go",
        };
        Ok(PreciseQueryRequest {
            workspace: ProviderWorkspace::from_documents(
                root.to_path_buf(),
                Revision(7),
                vec![
                    ProviderDocument {
                        path: RepoRelativePath::new("src/lib.rs")?,
                        source: Arc::from("pub fn rust_target() {}\n"),
                        language: Language::Rust,
                    },
                    ProviderDocument {
                        path: RepoRelativePath::new("src/index.ts")?,
                        source: Arc::from("export function tsTarget() {}\n"),
                        language: Language::TypeScript,
                    },
                    ProviderDocument {
                        path: RepoRelativePath::new("src/index.py")?,
                        source: Arc::from("def python_target():\n    pass\n"),
                        language: Language::Python,
                    },
                ],
            ),
            symbol: ProviderSymbol {
                name: name.to_owned(),
                declaration: SourceRange::new(
                    RepoRelativePath::new(path)?,
                    TextPosition::new(1, 1)?,
                    TextPosition::new(1, 2)?,
                )?,
                language,
            },
            directions: CallHierarchyDirections {
                incoming: true,
                outgoing: true,
            },
            limit: 20,
            priority,
        })
    }

    fn provider_for(
        providers: &[Arc<dyn PreciseProvider>],
        language: Language,
    ) -> Result<Arc<dyn PreciseProvider>, TestError> {
        providers
            .iter()
            .find(|provider| provider.supports(language))
            .cloned()
            .ok_or_else(|| format!("provider for {language:?} missing").into())
    }

    fn providers(pool: &ProviderPool) -> Result<Vec<Arc<dyn PreciseProvider>>, TestError> {
        let identity = WorkspaceIdentity::for_primary_worktree(&std::env::current_dir()?)?;
        Ok(pool.providers_for(&identity)?)
    }

    fn worktree_identities(
        first: &Path,
        second: &Path,
    ) -> Result<(WorkspaceIdentity, WorkspaceIdentity), TestError> {
        let repository = RepositoryId::from_stable_key("test-shared-git-object-database")?;
        Ok((
            WorkspaceIdentity::for_repository(first, repository.clone())?,
            WorkspaceIdentity::for_repository(second, repository)?,
        ))
    }

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> Result<(), TestError> {
        let deadline = Instant::now() + timeout;
        while !condition() {
            if Instant::now() >= deadline {
                return Err("condition did not become true before timeout".into());
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    #[test]
    fn path_filter_skips_ineligible_documents_without_activation() -> Result<(), TestError> {
        let controls = FakeControls::new(None);
        let registration = controls
            .registration("terraform-ls", vec![Language::Hcl], 10)
            .with_path_filter(|language, path| {
                language == Language::Hcl && !path.as_str().ends_with(".tf.json")
            });
        let pool = ProviderPool::start(ProviderPoolConfig::default(), vec![registration])?;
        let provider = provider_for(&providers(&pool)?, Language::Hcl)?;

        assert!(provider.supports_path(Language::Hcl, &RepoRelativePath::new("main.tf")?));
        assert!(!provider.supports_path(
            Language::Hcl,
            &RepoRelativePath::new("generated/main.tf.json")?
        ));
        assert_eq!(controls.activations.load(Ordering::Acquire), 0);
        pool.shutdown()?;
        Ok(())
    }

    #[test]
    fn starts_three_polyglot_providers_lazily_within_budgets() -> Result<(), TestError> {
        let barrier = Arc::new(Barrier::new(4));
        let controls = FakeControls::new(Some(barrier.clone()));
        let pool = ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 3,
                max_reserved_memory_bytes: 30,
                max_concurrent_queries: 3,
                ..ProviderPoolConfig::default()
            },
            vec![
                controls.registration("rust", vec![Language::Rust], 10),
                controls.registration("vtsls", vec![Language::TypeScript], 10),
                controls.registration("pyright", vec![Language::Python], 10),
            ],
        )?;
        let providers = providers(&pool)?;
        assert!(
            providers
                .iter()
                .all(|provider| provider.state_for(Revision(7)) == ProviderState::Dormant)
        );

        let mut workers = Vec::new();
        for language in [Language::Rust, Language::TypeScript, Language::Python] {
            let provider = provider_for(&providers, language)?;
            let query = request(language, "target", ProviderRequestPriority::Interactive)?;
            workers.push(thread::spawn(move || provider.enrich(query)));
        }
        barrier.wait();
        for worker in workers {
            assert_eq!(
                worker.join().map_err(|_| "provider query panicked")?.state,
                ProviderState::Ready
            );
        }

        assert_eq!(controls.max_active_queries.load(Ordering::Acquire), 3);
        assert_eq!(controls.activations.load(Ordering::Acquire), 3);
        let metrics = pool.metrics();
        assert_eq!(metrics.active_providers, 3);
        assert_eq!(metrics.reserved_memory_bytes, 30);
        assert_eq!(metrics.activations, 3);
        pool.shutdown()?;
        assert_eq!(controls.shutdowns.load(Ordering::Acquire), 3);
        Ok(())
    }

    #[test]
    fn provider_handles_reject_cross_worktree_requests() -> Result<(), TestError> {
        let first_root = tempfile::tempdir()?;
        let second_root = tempfile::tempdir()?;
        let (first, second) = worktree_identities(first_root.path(), second_root.path())?;
        let controls = FakeControls::new(None);
        let pool = ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 2,
                max_active_providers_per_workspace: 1,
                max_reserved_memory_bytes: 20,
                max_reserved_memory_bytes_per_workspace: 10,
                ..ProviderPoolConfig::default()
            },
            vec![controls.registration("rust", vec![Language::Rust], 10)],
        )?;
        let first_provider = provider_for(&pool.providers_for(&first)?, Language::Rust)?;
        let second_provider = provider_for(&pool.providers_for(&second)?, Language::Rust)?;

        let rejected = first_provider.enrich(request_in(
            &second.root,
            Language::Rust,
            "wrong-worktree",
            ProviderRequestPriority::Interactive,
        )?);
        assert_eq!(rejected.state, ProviderState::Degraded);
        assert_eq!(
            rejected.fallback_cause,
            Some(ProviderFallbackCause::ActivationFailed)
        );
        assert_eq!(controls.activations.load(Ordering::Acquire), 0);

        assert_eq!(
            first_provider
                .enrich(request_in(
                    &first.root,
                    Language::Rust,
                    "first",
                    ProviderRequestPriority::Interactive,
                )?)
                .state,
            ProviderState::Ready
        );
        assert_eq!(
            second_provider
                .enrich(request_in(
                    &second.root,
                    Language::Rust,
                    "second",
                    ProviderRequestPriority::Interactive,
                )?)
                .state,
            ProviderState::Ready
        );
        assert_eq!(pool.metrics().active_providers, 2);
        assert!(pool.metrics().workspace.is_none());
        let first_metrics = first_provider
            .orchestration_metrics()
            .ok_or("workspace-bound pool metrics missing")?;
        assert_eq!(first_metrics.configured_workspaces, 2);
        assert_eq!(
            first_metrics.workspace,
            Some(WorkspaceProviderOrchestrationMetrics {
                active_providers: 1,
                max_active_providers: 1,
                reserved_memory_bytes: 10,
                max_reserved_memory_bytes: 10,
            })
        );
        assert_eq!(
            lock(&controls.activation_roots).as_slice(),
            [first.root.clone(), second.root.clone()]
        );

        let conflicting = WorkspaceIdentity {
            root: first_root.path().to_path_buf(),
            ..second.clone()
        };
        assert!(matches!(
            pool.providers_for(&conflicting),
            Err(ProviderPoolWorkspaceError::IdentityConflict { workspace })
                if workspace == second.workspace
        ));
        pool.shutdown()?;
        assert!(matches!(
            pool.providers_for(&first),
            Err(ProviderPoolWorkspaceError::Stopped)
        ));
        Ok(())
    }

    #[test]
    fn per_workspace_provider_count_limit_evicts_only_within_the_limited_worktree()
    -> Result<(), TestError> {
        let first_root = tempfile::tempdir()?;
        let second_root = tempfile::tempdir()?;
        let (first, second) = worktree_identities(first_root.path(), second_root.path())?;
        let controls = FakeControls::new(None);
        let pool = ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 4,
                max_active_providers_per_workspace: 1,
                max_reserved_memory_bytes: 40,
                max_reserved_memory_bytes_per_workspace: 20,
                ..ProviderPoolConfig::default()
            },
            vec![
                controls.registration("rust", vec![Language::Rust], 10),
                controls.registration("pyright", vec![Language::Python], 10),
            ],
        )?;
        let first_providers = pool.providers_for(&first)?;
        let second_providers = pool.providers_for(&second)?;

        for (workspace, providers) in [(&first, &first_providers), (&second, &second_providers)] {
            for language in [Language::Rust, Language::Python] {
                assert_eq!(
                    provider_for(providers, language)?
                        .enrich(request_in(
                            &workspace.root,
                            language,
                            "target",
                            ProviderRequestPriority::Normal,
                        )?)
                        .state,
                    ProviderState::Ready
                );
            }
        }

        let metrics = pool.metrics();
        assert_eq!(metrics.active_providers, 2);
        assert_eq!(metrics.reserved_memory_bytes, 20);
        assert_eq!(metrics.resource_evictions, 2);
        assert_eq!(controls.shutdowns.load(Ordering::Acquire), 2);
        pool.shutdown()?;
        assert_eq!(controls.shutdowns.load(Ordering::Acquire), 4);
        Ok(())
    }

    #[test]
    fn per_workspace_memory_limit_does_not_evict_another_worktree() -> Result<(), TestError> {
        let first_root = tempfile::tempdir()?;
        let second_root = tempfile::tempdir()?;
        let (first, second) = worktree_identities(first_root.path(), second_root.path())?;
        let controls = FakeControls::new(None);
        let pool = ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 4,
                max_active_providers_per_workspace: 2,
                max_reserved_memory_bytes: 40,
                max_reserved_memory_bytes_per_workspace: 10,
                ..ProviderPoolConfig::default()
            },
            vec![
                controls.registration("rust", vec![Language::Rust], 10),
                controls.registration("pyright", vec![Language::Python], 10),
            ],
        )?;
        let first_providers = pool.providers_for(&first)?;
        let second_providers = pool.providers_for(&second)?;
        let second_rust = provider_for(&second_providers, Language::Rust)?;

        assert_eq!(
            second_rust
                .enrich(request_in(
                    &second.root,
                    Language::Rust,
                    "second-rust",
                    ProviderRequestPriority::Normal,
                )?)
                .state,
            ProviderState::Ready
        );
        for language in [Language::Rust, Language::Python] {
            assert_eq!(
                provider_for(&first_providers, language)?
                    .enrich(request_in(
                        &first.root,
                        language,
                        "first",
                        ProviderRequestPriority::Normal,
                    )?)
                    .state,
                ProviderState::Ready
            );
        }

        assert_eq!(
            second_rust
                .enrich(request_in(
                    &second.root,
                    Language::Rust,
                    "second-still-warm",
                    ProviderRequestPriority::Normal,
                )?)
                .state,
            ProviderState::Ready
        );
        assert_eq!(controls.activations.load(Ordering::Acquire), 3);
        assert_eq!(controls.shutdowns.load(Ordering::Acquire), 1);
        let metrics = pool.metrics();
        assert_eq!(metrics.active_providers, 2);
        assert_eq!(metrics.reserved_memory_bytes, 20);
        assert_eq!(metrics.resource_evictions, 1);
        pool.shutdown()?;
        assert_eq!(controls.shutdowns.load(Ordering::Acquire), 3);
        Ok(())
    }

    #[test]
    fn simultaneous_languages_and_worktrees_share_global_admission_safely() -> Result<(), TestError>
    {
        let first_root = tempfile::tempdir()?;
        let second_root = tempfile::tempdir()?;
        let (first, second) = worktree_identities(first_root.path(), second_root.path())?;
        let barrier = Arc::new(Barrier::new(5));
        let controls = FakeControls::new(Some(barrier.clone()));
        let pool = ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 4,
                max_active_providers_per_workspace: 2,
                max_reserved_memory_bytes: 40,
                max_reserved_memory_bytes_per_workspace: 20,
                max_concurrent_queries: 4,
                ..ProviderPoolConfig::default()
            },
            vec![
                controls.registration("rust", vec![Language::Rust], 10),
                controls.registration("pyright", vec![Language::Python], 10),
            ],
        )?;
        let mut workers = Vec::new();
        for workspace in [&first, &second] {
            let providers = pool.providers_for(workspace)?;
            for language in [Language::Rust, Language::Python] {
                let provider = provider_for(&providers, language)?;
                let query = request_in(
                    &workspace.root,
                    language,
                    "simultaneous",
                    ProviderRequestPriority::Interactive,
                )?;
                workers.push(thread::spawn(move || provider.enrich(query)));
            }
        }
        barrier.wait();
        for worker in workers {
            assert_eq!(
                worker.join().map_err(|_| "provider query panicked")?.state,
                ProviderState::Ready
            );
        }
        assert_eq!(controls.max_active_queries.load(Ordering::Acquire), 4);
        assert_eq!(pool.metrics().active_providers, 4);
        let mut roots = lock(&controls.activation_roots).clone();
        roots.sort();
        let mut expected_roots = vec![
            first.root.clone(),
            first.root,
            second.root.clone(),
            second.root,
        ];
        expected_roots.sort();
        assert_eq!(roots, expected_roots);
        pool.shutdown()?;
        Ok(())
    }

    #[test]
    fn higher_priority_waiter_runs_first_and_full_queue_falls_back() -> Result<(), TestError> {
        let controls = FakeControls::new(None);
        let pool = Arc::new(ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 1,
                max_reserved_memory_bytes: 10,
                max_concurrent_queries: 1,
                max_queued_queries: 2,
                query_queue_timeout: Duration::from_secs(2),
                ..ProviderPoolConfig::default()
            },
            vec![controls.registration("rust", vec![Language::Rust], 10)],
        )?);
        let provider = provider_for(&providers(&pool)?, Language::Rust)?;
        let holding_provider = provider.clone();
        let holding = thread::spawn(move || -> Result<_, TestError> {
            Ok(holding_provider.enrich(request(
                Language::Rust,
                "hold",
                ProviderRequestPriority::Normal,
            )?))
        });
        wait_until(Duration::from_secs(1), || {
            pool.metrics().running_queries == 1
        })?;

        let background_provider = provider.clone();
        let background = thread::spawn(move || -> Result<_, TestError> {
            Ok(background_provider.enrich(request(
                Language::Rust,
                "background",
                ProviderRequestPriority::Background,
            )?))
        });
        let interactive_provider = provider.clone();
        let interactive = thread::spawn(move || -> Result<_, TestError> {
            Ok(interactive_provider.enrich(request(
                Language::Rust,
                "interactive",
                ProviderRequestPriority::Interactive,
            )?))
        });
        wait_until(Duration::from_secs(1), || {
            pool.metrics().queued_queries == 2
        })?;
        let saturated = provider.enrich(request(
            Language::Rust,
            "saturated",
            ProviderRequestPriority::Interactive,
        )?);
        assert_eq!(saturated.state, ProviderState::CatchingUp);
        assert_eq!(
            saturated.fallback_cause,
            Some(ProviderFallbackCause::QueueSaturated)
        );
        controls.release();

        assert_eq!(
            holding.join().map_err(|_| "holding query panicked")??.state,
            ProviderState::Ready
        );
        assert_eq!(
            interactive
                .join()
                .map_err(|_| "interactive query panicked")??
                .state,
            ProviderState::Ready
        );
        assert_eq!(
            background
                .join()
                .map_err(|_| "background query panicked")??
                .state,
            ProviderState::Ready
        );
        assert_eq!(
            lock(&controls.order).as_slice(),
            ["hold", "interactive", "background"]
        );
        assert_eq!(pool.metrics().saturated_queries, 1);
        let latency = pool.metrics().queue_latency_by_priority;
        assert_eq!(latency.interactive.samples, 1);
        assert_eq!(latency.background.samples, 1);
        assert_eq!(latency.normal.samples, 0);
        pool.shutdown()?;
        Ok(())
    }

    #[test]
    fn admission_aging_promotes_background_to_top_rank_after_two_admissions() {
        let enqueued = Instant::now();
        let background = Waiter {
            sequence: 0,
            priority: ProviderRequestPriority::Background,
            enqueued,
            boost: 0,
        };
        let mut waiters = vec![background];

        // First interactive arrival still wins: no aging has accrued yet.
        waiters.push(Waiter {
            sequence: 1,
            ..waiter_with(ProviderRequestPriority::Interactive)
        });
        assert_eq!(best_waiter(&waiters), Some(1));
        remove_waiter(&mut waiters, 1);
        age_waiters(&mut waiters);

        // After one admission the background waiter is effectively Normal and
        // still yields to a fresh interactive request.
        waiters.push(Waiter {
            sequence: 2,
            ..waiter_with(ProviderRequestPriority::Interactive)
        });
        assert_eq!(best_waiter(&waiters), Some(2));
        remove_waiter(&mut waiters, 2);
        age_waiters(&mut waiters);

        // After two admissions it ties the top class and its older sequence
        // wins over a freshly queued interactive request. Older queued work
        // may still precede it, but new interactive load cannot starve it.
        waiters.push(Waiter {
            sequence: 3,
            ..waiter_with(ProviderRequestPriority::Interactive)
        });
        assert_eq!(best_waiter(&waiters), Some(0));
    }

    fn waiter_with(priority: ProviderRequestPriority) -> Waiter {
        Waiter {
            sequence: 0,
            priority,
            enqueued: Instant::now(),
            boost: 0,
        }
    }

    #[test]
    fn queued_cancellation_is_observable_and_does_not_reach_provider() -> Result<(), TestError> {
        let controls = FakeControls::new(None);
        let pool = Arc::new(ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 1,
                max_reserved_memory_bytes: 10,
                max_concurrent_queries: 1,
                max_queued_queries: 1,
                query_queue_timeout: Duration::from_secs(2),
                ..ProviderPoolConfig::default()
            },
            vec![controls.registration("rust", vec![Language::Rust], 10)],
        )?);
        let provider = provider_for(&providers(&pool)?, Language::Rust)?;
        let holding_provider = provider.clone();
        let holding = thread::spawn(move || -> Result<_, TestError> {
            Ok(holding_provider.enrich(request(
                Language::Rust,
                "hold",
                ProviderRequestPriority::Normal,
            )?))
        });
        wait_until(Duration::from_secs(1), || {
            pool.metrics().running_queries == 1
        })?;

        let operation = OperationContext::unbounded();
        let queued_operation = operation.clone();
        let queued_provider = provider.clone();
        let queued = thread::spawn(move || {
            let query = request(
                Language::Rust,
                "cancelled",
                ProviderRequestPriority::Interactive,
            )?;
            Ok::<_, TestError>(queued_provider.enrich_with_context(query, &queued_operation))
        });
        wait_until(Duration::from_secs(1), || {
            pool.metrics().queued_queries == 1
        })?;
        operation.cancel();
        let cancelled = queued
            .join()
            .map_err(|_| "cancelled query panicked")?
            .map_err(|error| error.to_string())?;
        assert_eq!(cancelled.state, ProviderState::CatchingUp);
        assert_eq!(
            cancelled.fallback_cause,
            Some(ProviderFallbackCause::Cancelled)
        );
        assert_eq!(pool.metrics().cancelled_queries, 1);
        assert_eq!(lock(&controls.order).as_slice(), ["hold"]);
        controls.release();
        assert_eq!(
            holding.join().map_err(|_| "holding query panicked")??.state,
            ProviderState::Ready
        );
        pool.shutdown()?;
        Ok(())
    }

    #[test]
    fn queued_query_times_out_with_typed_fallback_without_reaching_provider()
    -> Result<(), TestError> {
        let controls = FakeControls::new(None);
        let pool = Arc::new(ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 1,
                max_reserved_memory_bytes: 10,
                max_concurrent_queries: 1,
                max_queued_queries: 1,
                query_queue_timeout: Duration::from_millis(30),
                ..ProviderPoolConfig::default()
            },
            vec![controls.registration("rust", vec![Language::Rust], 10)],
        )?);
        let provider = provider_for(&providers(&pool)?, Language::Rust)?;
        let holding_provider = provider.clone();
        let holding = thread::spawn(move || -> Result<_, TestError> {
            Ok(holding_provider.enrich(request(
                Language::Rust,
                "hold",
                ProviderRequestPriority::Normal,
            )?))
        });
        wait_until(Duration::from_secs(1), || {
            pool.metrics().running_queries == 1
        })?;

        let timed_out = provider.enrich(request(
            Language::Rust,
            "timed-out",
            ProviderRequestPriority::Interactive,
        )?);
        assert_eq!(timed_out.state, ProviderState::CatchingUp);
        assert_eq!(
            timed_out.fallback_cause,
            Some(ProviderFallbackCause::QueueTimedOut)
        );
        assert_eq!(pool.metrics().queue_timeouts, 1);
        assert_eq!(lock(&controls.order).as_slice(), ["hold"]);
        controls.release();
        assert_eq!(
            holding.join().map_err(|_| "holding query panicked")??.state,
            ProviderState::Ready
        );
        pool.shutdown()?;
        Ok(())
    }

    #[test]
    fn resource_and_idle_reclamation_shutdown_only_inactive_providers() -> Result<(), TestError> {
        let controls = FakeControls::new(None);
        controls.release();
        let pool = ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 1,
                max_reserved_memory_bytes: 10,
                idle_timeout: Duration::from_millis(30),
                idle_poll_interval: Duration::from_millis(5),
                ..ProviderPoolConfig::default()
            },
            vec![
                controls.registration("rust", vec![Language::Rust], 10),
                controls.registration("pyright", vec![Language::Python], 10),
            ],
        )?;
        let providers = providers(&pool)?;
        assert_eq!(
            provider_for(&providers, Language::Rust)?
                .enrich(request(
                    Language::Rust,
                    "rust",
                    ProviderRequestPriority::Normal,
                )?)
                .state,
            ProviderState::Ready
        );
        assert_eq!(
            provider_for(&providers, Language::Python)?
                .enrich(request(
                    Language::Python,
                    "python",
                    ProviderRequestPriority::Normal,
                )?)
                .state,
            ProviderState::Ready
        );
        assert_eq!(pool.metrics().resource_evictions, 1);
        assert_eq!(controls.shutdowns.load(Ordering::Acquire), 1);
        wait_until(Duration::from_secs(1), || {
            pool.metrics().active_providers == 0
        })?;
        assert_eq!(pool.metrics().idle_shutdowns, 1);
        assert_eq!(controls.shutdowns.load(Ordering::Acquire), 2);

        assert_eq!(
            provider_for(&providers, Language::Rust)?
                .enrich(request(
                    Language::Rust,
                    "rust-again",
                    ProviderRequestPriority::Normal,
                )?)
                .state,
            ProviderState::Ready
        );
        assert_eq!(controls.activations.load(Ordering::Acquire), 3);
        pool.shutdown()?;
        assert_eq!(controls.shutdowns.load(Ordering::Acquire), 3);
        Ok(())
    }

    #[test]
    fn activation_failure_uses_backoff_before_retrying() -> Result<(), TestError> {
        let attempts = Arc::new(AtomicU64::new(0));
        let factory_attempts = attempts.clone();
        let pool = ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 1,
                max_reserved_memory_bytes: 10,
                activation_backoff_base: Duration::from_millis(30),
                activation_backoff_max: Duration::from_millis(30),
                ..ProviderPoolConfig::default()
            },
            vec![ProviderRegistration::new(
                "rust",
                vec![Language::Rust],
                10,
                move |_workspace, _operation| {
                    factory_attempts.fetch_add(1, Ordering::AcqRel);
                    Err(ProviderStartError::new("start failed"))
                },
            )],
        )?;
        let provider = provider_for(&providers(&pool)?, Language::Rust)?;
        let first = provider.enrich(request(
            Language::Rust,
            "first",
            ProviderRequestPriority::Normal,
        )?);
        assert_eq!(first.state, ProviderState::Degraded);
        assert_eq!(
            first.fallback_cause,
            Some(ProviderFallbackCause::ActivationFailed)
        );
        assert_eq!(
            provider
                .enrich(request(
                    Language::Rust,
                    "second",
                    ProviderRequestPriority::Normal,
                )?)
                .state,
            ProviderState::Degraded
        );
        assert_eq!(attempts.load(Ordering::Acquire), 1);
        thread::sleep(Duration::from_millis(40));
        let _ = provider.enrich(request(
            Language::Rust,
            "third",
            ProviderRequestPriority::Normal,
        )?);
        assert_eq!(attempts.load(Ordering::Acquire), 2);
        assert_eq!(pool.metrics().activation_failures, 2);
        pool.shutdown()?;
        Ok(())
    }

    #[test]
    fn rejects_zero_bounds_and_ambiguous_registrations() -> Result<(), TestError> {
        let controls = FakeControls::new(None);
        assert!(matches!(
            ProviderPool::start(
                ProviderPoolConfig {
                    max_active_providers: 0,
                    ..ProviderPoolConfig::default()
                },
                Vec::new(),
            ),
            Err(ProviderPoolConfigError::ZeroBound)
        ));
        assert!(matches!(
            ProviderPool::start(
                ProviderPoolConfig {
                    max_reserved_memory_bytes: 20,
                    max_reserved_memory_bytes_per_workspace: 5,
                    ..ProviderPoolConfig::default()
                },
                vec![controls.registration("too-large-locally", vec![Language::Python], 10)],
            ),
            Err(ProviderPoolConfigError::ReservationExceedsWorkspace {
                reserved: 10,
                maximum: 5,
                ..
            })
        ));
        assert!(matches!(
            ProviderPool::start(
                ProviderPoolConfig::default(),
                vec![controls.registration(
                    "duplicate-language",
                    vec![Language::Rust, Language::Rust],
                    10,
                )],
            ),
            Err(ProviderPoolConfigError::DuplicateLanguage {
                language: Language::Rust,
                ..
            })
        ));
        assert!(matches!(
            ProviderPool::start(
                ProviderPoolConfig::default(),
                vec![
                    controls.registration("first", vec![Language::Rust], 10),
                    controls.registration("second", vec![Language::Rust], 10),
                ],
            ),
            Err(ProviderPoolConfigError::LanguageConflict {
                language: Language::Rust,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn cancellation_during_activation_releases_reservation_without_backoff() -> Result<(), TestError>
    {
        let (entered_sender, entered_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let release_receiver = Arc::new(Mutex::new(release_receiver));
        let factory_release = release_receiver.clone();
        let pool = Arc::new(ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 1,
                max_reserved_memory_bytes: 10,
                ..ProviderPoolConfig::default()
            },
            vec![ProviderRegistration::new(
                "rust",
                vec![Language::Rust],
                10,
                move |_workspace, operation| {
                    entered_sender
                        .send(())
                        .map_err(|error| ProviderStartError::new(error.to_string()))?;
                    lock(&factory_release)
                        .recv()
                        .map_err(|error| ProviderStartError::new(error.to_string()))?;
                    operation.check().map_err(ProviderStartError::from)?;
                    Err(ProviderStartError::new(
                        "activation unexpectedly continued after cancellation",
                    ))
                },
            )],
        )?);
        let provider = provider_for(&providers(&pool)?, Language::Rust)?;
        let operation = OperationContext::unbounded();
        let query_operation = operation.clone();
        let query = request(
            Language::Rust,
            "cancel-activation",
            ProviderRequestPriority::Interactive,
        )?;
        let worker = thread::spawn(move || provider.enrich_with_context(query, &query_operation));
        entered_receiver.recv_timeout(Duration::from_secs(1))?;
        operation.cancel();
        release_sender.send(())?;
        let result = worker
            .join()
            .map_err(|_| "activation cancellation query panicked")?;
        assert_eq!(result.state, ProviderState::CatchingUp);
        assert_eq!(
            result.fallback_cause,
            Some(ProviderFallbackCause::Cancelled)
        );
        let metrics = pool.metrics();
        assert_eq!(metrics.active_providers, 0);
        assert_eq!(metrics.reserved_memory_bytes, 0);
        assert_eq!(metrics.cancelled_queries, 1);
        assert_eq!(metrics.activation_failures, 0);
        pool.shutdown()?;
        Ok(())
    }

    #[test]
    fn failed_eviction_shutdown_retains_capacity_until_cleanup_retry() -> Result<(), TestError> {
        let shutdown_attempts = Arc::new(AtomicUsize::new(0));
        let factory_attempts = shutdown_attempts.clone();
        let queries = Arc::new(AtomicUsize::new(0));
        let factory_queries = queries.clone();
        let controls = FakeControls::new(None);
        controls.release();
        let pool = ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 1,
                max_reserved_memory_bytes: 10,
                ..ProviderPoolConfig::default()
            },
            vec![
                ProviderRegistration::new(
                    "rust",
                    vec![Language::Rust],
                    10,
                    move |_workspace, _operation| {
                        Ok(Arc::new(FailsFirstShutdownProvider {
                            name: "rust",
                            attempts: factory_attempts.clone(),
                            queries: factory_queries.clone(),
                        }) as Arc<dyn PreciseProvider>)
                    },
                ),
                controls.registration("pyright", vec![Language::Python], 10),
            ],
        )?;
        let providers = providers(&pool)?;
        let rust = provider_for(&providers, Language::Rust)?;
        let python = provider_for(&providers, Language::Python)?;
        assert_eq!(
            rust.enrich(request(
                Language::Rust,
                "rust",
                ProviderRequestPriority::Normal,
            )?)
            .state,
            ProviderState::Ready
        );

        let capacity = python.enrich(request(
            Language::Python,
            "python",
            ProviderRequestPriority::Normal,
        )?);
        assert_eq!(capacity.state, ProviderState::CatchingUp);
        assert_eq!(
            capacity.fallback_cause,
            Some(ProviderFallbackCause::ActivationCapacity)
        );
        assert_eq!(rust.state_for(Revision(7)), ProviderState::Degraded);
        assert!(
            rust.last_error()
                .is_some_and(|error| error.contains("synthetic cleanup failure"))
        );
        let metrics = pool.metrics();
        assert_eq!(metrics.active_providers, 1);
        assert_eq!(metrics.reserved_memory_bytes, 10);
        assert_eq!(metrics.shutdown_failures, 1);
        assert_eq!(controls.activations.load(Ordering::Acquire), 0);

        PreciseProvider::shutdown(rust.as_ref())?;
        assert_eq!(pool.metrics().active_providers, 0);
        assert_eq!(
            python
                .enrich(request(
                    Language::Python,
                    "python-retry",
                    ProviderRequestPriority::Normal,
                )?)
                .state,
            ProviderState::Ready
        );
        assert_eq!(controls.activations.load(Ordering::Acquire), 1);
        pool.shutdown()?;
        Ok(())
    }

    #[test]
    fn incompatible_factory_cleanup_failure_retains_capacity_and_blocks_reuse()
    -> Result<(), TestError> {
        let shutdown_attempts = Arc::new(AtomicUsize::new(0));
        let provider_queries = Arc::new(AtomicUsize::new(0));
        let factory_activations = Arc::new(AtomicUsize::new(0));
        let factory_shutdown_attempts = shutdown_attempts.clone();
        let factory_provider_queries = provider_queries.clone();
        let activations = factory_activations.clone();
        let pool = ProviderPool::start(
            ProviderPoolConfig {
                max_active_providers: 1,
                max_reserved_memory_bytes: 10,
                ..ProviderPoolConfig::default()
            },
            vec![ProviderRegistration::new(
                "rust",
                vec![Language::Rust],
                10,
                move |_workspace, _operation| {
                    activations.fetch_add(1, Ordering::AcqRel);
                    Ok(Arc::new(FailsFirstShutdownProvider {
                        name: "wrong-provider",
                        attempts: factory_shutdown_attempts.clone(),
                        queries: factory_provider_queries.clone(),
                    }) as Arc<dyn PreciseProvider>)
                },
            )],
        )?;
        let provider = provider_for(&providers(&pool)?, Language::Rust)?;

        let first = provider.enrich(request(
            Language::Rust,
            "first",
            ProviderRequestPriority::Normal,
        )?);
        assert_eq!(first.state, ProviderState::Degraded);
        assert_eq!(
            first.fallback_cause,
            Some(ProviderFallbackCause::ActivationFailed)
        );
        let metrics = pool.metrics();
        assert_eq!(metrics.active_providers, 1);
        assert_eq!(metrics.reserved_memory_bytes, 10);
        assert_eq!(metrics.activation_failures, 1);
        assert_eq!(metrics.shutdown_failures, 1);

        let second = provider.enrich(request(
            Language::Rust,
            "second",
            ProviderRequestPriority::Normal,
        )?);
        assert_eq!(second.state, ProviderState::Degraded);
        assert_eq!(factory_activations.load(Ordering::Acquire), 1);
        assert_eq!(provider_queries.load(Ordering::Acquire), 0);

        PreciseProvider::shutdown(provider.as_ref())?;
        assert_eq!(shutdown_attempts.load(Ordering::Acquire), 2);
        assert_eq!(pool.metrics().active_providers, 0);
        assert_eq!(pool.metrics().reserved_memory_bytes, 0);
        pool.shutdown()?;
        Ok(())
    }
}
