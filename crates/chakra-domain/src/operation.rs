//! Adapter-neutral cooperative cancellation and execution deadlines.
//!
//! Long-running synchronous work receives an [`OperationContext`] instead of
//! transport-specific cancellation types. Clones share cancellation while
//! retaining the same absolute deadline, so the context can cross the MCP,
//! query, freshness, Git, and language-provider boundaries without reversing
//! dependency direction.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;

/// A cheap, cloneable cancellation flag shared by one owned operation.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Why cooperative work must stop before producing a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum OperationAbort {
    #[error("operation was cancelled by its caller")]
    Cancelled,
    #[error("operation exceeded its execution deadline")]
    DeadlineExceeded,
}

/// Cancellation and an optional absolute deadline for one operation.
#[derive(Debug, Clone)]
pub struct OperationContext {
    cancellation: CancellationToken,
    deadline: Option<Instant>,
}

impl Default for OperationContext {
    fn default() -> Self {
        Self::unbounded()
    }
}

impl OperationContext {
    /// A context that only stops when explicitly cancelled.
    pub fn unbounded() -> Self {
        Self {
            cancellation: CancellationToken::default(),
            deadline: None,
        }
    }

    /// Builds an unbounded operation around an existing cancellation owner.
    /// Indexing adapters use this compatibility bridge when their public
    /// configuration predates end-to-end execution deadlines.
    pub fn from_cancellation(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            deadline: None,
        }
    }

    /// A context with a deadline relative to now.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            cancellation: CancellationToken::default(),
            deadline: Instant::now().checked_add(timeout),
        }
    }

    /// Derives a context with the same cancellation owner and no later than
    /// `timeout` from now. This lets an adapter retain a stricter local bound
    /// without extending the caller's end-to-end deadline.
    pub fn bounded_by(&self, timeout: Duration) -> Self {
        let local = Instant::now().checked_add(timeout);
        let deadline = match (self.deadline, local) {
            (Some(caller), Some(local)) => Some(caller.min(local)),
            (caller @ Some(_), None) => caller,
            (None, local) => local,
        };
        Self {
            cancellation: self.cancellation.clone(),
            deadline,
        }
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn check(&self) -> Result<(), OperationAbort> {
        if self.cancellation.is_cancelled() {
            return Err(OperationAbort::Cancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(OperationAbort::DeadlineExceeded);
        }
        Ok(())
    }

    /// Remaining time, optionally capped for bounded polling. An unbounded
    /// operation returns `cap`; an expired operation returns its typed cause.
    pub fn poll_timeout(&self, cap: Duration) -> Result<Duration, OperationAbort> {
        self.check()?;
        Ok(self
            .deadline
            .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            .map_or(cap, |remaining| remaining.min(cap)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_and_deadline_is_not_extended() {
        let context = OperationContext::with_timeout(Duration::from_secs(1));
        let derived = context.bounded_by(Duration::from_secs(2));
        assert_eq!(derived.deadline(), context.deadline());
        derived.cancel();
        assert_eq!(context.check(), Err(OperationAbort::Cancelled));
    }

    #[test]
    fn zero_timeout_is_expired() {
        let context = OperationContext::with_timeout(Duration::ZERO);
        assert_eq!(context.check(), Err(OperationAbort::DeadlineExceeded));
    }
}
