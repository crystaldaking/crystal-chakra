//! Bounded orchestration for optional precise language providers.
//!
//! The pool owns lifecycle policy while each adapter continues to own its
//! protocol worker and child process. Providers start from the exact query
//! workspace, are routed by disjoint language capabilities, and may be
//! reclaimed only while they have no in-flight query.

mod config;

pub use config::{
    ProviderPoolConfig, ProviderPoolConfigError, ProviderRegistration, ProviderStartError,
};

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::query::{
    ProviderFallbackCause, ProviderMetrics, ProviderOrchestrationMetrics, ProviderProgress,
};
use chakra_domain::revision::Revision;
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
        let inner = Arc::new(PoolInner {
            config,
            slots: registrations
                .into_iter()
                .map(|registration| {
                    Arc::new(ProviderSlot {
                        registration,
                        runtime: Mutex::new(SlotRuntime::default()),
                        changed: Condvar::new(),
                    })
                })
                .collect(),
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

    /// Lazy provider handles suitable for installation in `WorkspaceEngine`.
    pub fn providers(&self) -> Vec<Arc<dyn PreciseProvider>> {
        self.inner
            .slots
            .iter()
            .enumerate()
            .map(|(index, _)| {
                Arc::new(PooledProvider {
                    inner: self.inner.clone(),
                    slot_index: index,
                }) as Arc<dyn PreciseProvider>
            })
            .collect()
    }

    pub fn metrics(&self) -> ProviderOrchestrationMetrics {
        self.inner.metrics()
    }

    /// Stops admission, waits boundedly for admitted work, then joins the
    /// reaper and every provider-owned worker/process.
    pub fn shutdown(&self) -> Result<(), ProviderPoolShutdownError> {
        self.inner.stopped.store(true, Ordering::Release);
        self.inner.changed.notify_all();
        for slot in &self.inner.slots {
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
        for slot_index in 0..self.inner.slots.len() {
            if let Err(error) = self
                .inner
                .evict_provider(slot_index, EvictionCause::Shutdown)
            {
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
    slots: Vec<Arc<ProviderSlot>>,
    state: Mutex<PoolState>,
    changed: Condvar,
    stopped: AtomicBool,
}

impl fmt::Debug for PoolInner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PoolInner")
            .field("config", &self.config)
            .field("slots", &self.slots)
            .field("stopped", &self.stopped.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct PoolState {
    active_providers: usize,
    reserved_memory_bytes: u64,
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
}

#[derive(Debug, Clone, Copy)]
struct Waiter {
    sequence: u64,
    priority: ProviderRequestPriority,
}

struct ProviderSlot {
    registration: ProviderRegistration,
    runtime: Mutex<SlotRuntime>,
    changed: Condvar,
}

impl fmt::Debug for ProviderSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderSlot")
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
    slot_index: usize,
}

impl PreciseProvider for PooledProvider {
    fn name(&self) -> &'static str {
        self.slot().registration.name
    }

    fn supports(&self, language: Language) -> bool {
        self.slot().registration.languages.contains(&language)
    }

    fn state_for(&self, revision: Revision) -> ProviderState {
        if self.inner.stopped.load(Ordering::Acquire) {
            return ProviderState::Degraded;
        }
        let runtime = lock(&self.slot().runtime);
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
        let runtime = lock(&self.slot().runtime);
        runtime.last_error.clone().or_else(|| {
            runtime
                .provider
                .as_ref()
                .and_then(|provider| provider.last_error())
        })
    }

    fn progress(&self) -> Option<ProviderProgress> {
        lock(&self.slot().runtime)
            .provider
            .as_ref()
            .and_then(|provider| provider.progress())
    }

    fn metrics(&self) -> Option<ProviderMetrics> {
        let mut metrics = lock(&self.slot().runtime)
            .provider
            .as_ref()
            .and_then(|provider| provider.metrics())
            .unwrap_or_default();
        metrics.orchestration = Some(self.inner.metrics());
        Some(metrics)
    }

    fn query_wait_budget(&self) -> Option<Duration> {
        Some(
            self.inner
                .config
                .query_queue_timeout
                .saturating_add(self.slot().registration.additional_wait_budget),
        )
    }

    fn shutdown(&self) -> Result<(), ProviderShutdownError> {
        let _ = self
            .inner
            .evict_provider(self.slot_index, EvictionCause::Shutdown)?;
        Ok(())
    }

    fn enrich_with_context(
        &self,
        request: PreciseQueryRequest,
        operation: &OperationContext,
    ) -> PreciseQueryResult {
        let revision = request.workspace.revision;
        if !self.supports(request.symbol.language) {
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
            .activate(self.slot_index, &request.workspace, operation)
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

impl PooledProvider {
    fn slot(&self) -> &Arc<ProviderSlot> {
        &self.inner.slots[self.slot_index]
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
        state.waiters.push(Waiter { sequence, priority });
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
                remove_waiter(&mut state.waiters, sequence);
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
        slot_index: usize,
        workspace: &ProviderWorkspace,
        operation: &OperationContext,
    ) -> Result<ProviderLease, ActivationFailure> {
        let slot = &self.slots[slot_index];
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
                    slot_index,
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

        if let Err(failure) = self.reserve_capacity(slot_index) {
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
                        Ok(()) => self.release_reservation(slot_index),
                        Err(error) => self.retain_after_shutdown_failure(
                            slot_index,
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
                    slot_index,
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
                        self.record_activation_failure(
                            slot_index,
                            &mut runtime,
                            MESSAGE.to_owned(),
                        );
                    }
                    Err(error) => self.retain_after_shutdown_failure(
                        slot_index,
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
                    self.release_reservation(slot_index);
                    slot.changed.notify_all();
                    return Err(self.record_activation_abort(abort));
                }
                self.record_activation_failure(slot_index, &mut runtime, error.to_string());
                Err(ActivationFailure::StartFailed)
            }
        }
    }

    fn record_activation_failure(
        &self,
        slot_index: usize,
        runtime: &mut SlotRuntime,
        message: String,
    ) {
        self.apply_activation_backoff(runtime, message);
        self.release_reservation(slot_index);
        let mut state = lock(&self.state);
        state.activation_failures = state.activation_failures.saturating_add(1);
        self.slots[slot_index].changed.notify_all();
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
        slot_index: usize,
        provider: Arc<dyn PreciseProvider>,
        message: String,
        activation_failed: bool,
    ) {
        let slot = &self.slots[slot_index];
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

    fn reserve_capacity(&self, slot_index: usize) -> Result<(), ActivationFailure> {
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return Err(ActivationFailure::Stopped);
            }
            let reservation = self.slots[slot_index].registration.reserved_memory_bytes;
            let mut state = lock(&self.state);
            let active_available = state.active_providers < self.config.max_active_providers;
            let memory_available = state.reserved_memory_bytes.saturating_add(reservation)
                <= self.config.max_reserved_memory_bytes;
            if active_available && memory_available {
                state.active_providers += 1;
                state.reserved_memory_bytes =
                    state.reserved_memory_bytes.saturating_add(reservation);
                return Ok(());
            }
            drop(state);
            let Some(victim) = self.oldest_evictable_slot(slot_index) else {
                let mut state = lock(&self.state);
                state.saturated_queries = state.saturated_queries.saturating_add(1);
                return Err(ActivationFailure::Capacity);
            };
            match self.evict_provider(victim, EvictionCause::Resource) {
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

    fn oldest_evictable_slot(&self, excluded: usize) -> Option<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != excluded)
            .filter_map(|(index, slot)| {
                let runtime = lock(&slot.runtime);
                (runtime.provider.is_some() && runtime.in_flight == 0 && !runtime.activating)
                    .then_some((index, runtime.last_used))
            })
            .min_by_key(|(_, last_used)| *last_used)
            .map(|(index, _)| index)
    }

    fn evict_provider(
        &self,
        slot_index: usize,
        cause: EvictionCause,
    ) -> Result<bool, ProviderShutdownError> {
        let slot = &self.slots[slot_index];
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
        self.release_reservation(slot_index);
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

    fn release_reservation(&self, slot_index: usize) {
        let reservation = self.slots[slot_index].registration.reserved_memory_bytes;
        let mut state = lock(&self.state);
        state.active_providers = state.active_providers.saturating_sub(1);
        state.reserved_memory_bytes = state.reserved_memory_bytes.saturating_sub(reservation);
        self.changed.notify_all();
    }

    fn metrics(&self) -> ProviderOrchestrationMetrics {
        let state = lock(&self.state);
        ProviderOrchestrationMetrics {
            configured_providers: self.slots.len() as u64,
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
    slot_index: usize,
    provider: Arc<dyn PreciseProvider>,
}

impl Drop for ProviderLease {
    fn drop(&mut self) {
        let slot = &self.inner.slots[self.slot_index];
        let mut runtime = lock(&slot.runtime);
        runtime.in_flight = runtime.in_flight.saturating_sub(1);
        runtime.last_used = Instant::now();
        slot.changed.notify_all();
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
        || config.max_reserved_memory_bytes == 0
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
        for slot_index in 0..inner.slots.len() {
            let should_evict = {
                let runtime = lock(&inner.slots[slot_index].runtime);
                runtime.provider.is_some()
                    && runtime.in_flight == 0
                    && !runtime.activating
                    && runtime.last_used.elapsed() >= inner.config.idle_timeout
            };
            if should_evict {
                // The slot retains the provider reservation and last error;
                // a later reaper pass or final shutdown retries cleanup.
                let _ = inner.evict_provider(slot_index, EvictionCause::Idle);
            }
        }
    }
}

fn best_waiter(waiters: &[Waiter]) -> Option<u64> {
    waiters
        .iter()
        .max_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| right.sequence.cmp(&left.sequence))
        })
        .map(|waiter| waiter.sequence)
}

fn remove_waiter(waiters: &mut Vec<Waiter>, sequence: u64) {
    if let Some(index) = waiters
        .iter()
        .position(|waiter| waiter.sequence == sequence)
    {
        waiters.swap_remove(index);
    }
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
    use std::path::PathBuf;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::mpsc;

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
                move |_workspace, _operation| {
                    controls.activations.fetch_add(1, Ordering::AcqRel);
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
        };
        Ok(PreciseQueryRequest {
            workspace: ProviderWorkspace::from_documents(
                PathBuf::from("."),
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
        let providers = pool.providers();
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
        let provider = provider_for(&pool.providers(), Language::Rust)?;
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
        pool.shutdown()?;
        Ok(())
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
        let provider = provider_for(&pool.providers(), Language::Rust)?;
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
        let provider = provider_for(&pool.providers(), Language::Rust)?;
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
        let providers = pool.providers();
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
        let provider = provider_for(&pool.providers(), Language::Rust)?;
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
        let provider = provider_for(&pool.providers(), Language::Rust)?;
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
        let providers = pool.providers();
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
        let provider = provider_for(&pool.providers(), Language::Rust)?;

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
