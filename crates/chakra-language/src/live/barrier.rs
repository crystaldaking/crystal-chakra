use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};

use chakra_domain::operation::OperationContext;
use chakra_engine::{FreshnessBarrier, FreshnessBarrierError};

use super::metrics::MetricsState;
use super::{FRESHNESS_CANCELLATION_POLL, FRESHNESS_TIMEOUT, WorkerSignal};

#[derive(Debug, Default)]
pub(super) struct BarrierState {
    requested: u64,
    completed: u64,
    waiters: BTreeMap<u64, OperationContext>,
    outcomes: BTreeMap<u64, Result<(), String>>,
    worker_operation: Option<OperationContext>,
    shutdown: bool,
}

#[derive(Debug)]
pub(super) struct BarrierShared {
    pub(super) state: Mutex<BarrierState>,
    pub(super) completed: Condvar,
}

impl BarrierShared {
    pub(super) fn is_stopped(&self) -> bool {
        self.state.lock().map_or(true, |state| state.shutdown)
    }

    pub(super) fn pending_generation(&self) -> Result<(u64, u64), String> {
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

    pub(super) fn register(
        &self,
        operation: OperationContext,
    ) -> Result<u64, FreshnessBarrierError> {
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

    pub(super) fn begin_barrier_reconciliation(
        &self,
    ) -> Result<(u64, u64, OperationContext), String> {
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

    pub(super) fn complete(&self, generation: u64, result: Result<(), String>) {
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

    pub(super) fn finish_waiter(&self, target: u64) {
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

    pub(super) fn abandon_worker(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.worker_operation = None;
        self.completed.notify_all();
    }

    pub(super) fn stop(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.shutdown = true;
        self.completed.notify_all();
    }
}

#[derive(Debug)]
pub(super) struct LiveFreshnessBarrier {
    pub(super) shared: Arc<BarrierShared>,
    pub(super) sender: SyncSender<WorkerSignal>,
    pub(super) metrics: Arc<MetricsState>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

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
