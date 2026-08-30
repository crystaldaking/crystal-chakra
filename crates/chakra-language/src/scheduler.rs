//! Bounded priority queue with explicit fairness for scheduled indexing work
//! (issue #44).
//!
//! The queue is the worker-side staging area behind a bounded transport
//! channel: producers never touch it directly, so it needs no internal
//! locking. The owning worker publishes `metrics()` into shared
//! instrumentation once per scheduling pass.
//!
//! Fairness uses **aging**: an entry waiting at least `aging_after` is
//! promoted one class per elapsed interval, capped at the most urgent class,
//! where it competes FIFO by sequence. Aging was chosen over a guaranteed
//! per-class share because it needs no admission counters, keeps interactive
//! bursts strictly first while they arrive, and bounds the worst-case wait of
//! any queued item to `(rank gap) * aging_after` before it competes with the
//! top class.
//!
//! Cancellation is explicit: `retain` removes obsolete queued work (for
//! example a reconciliation checkpoint superseded by a full freshness
//! reconcile) and `note_superseded` marks an entry that was already obsolete
//! when dequeued, so discarded work is always visible in metrics.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use chakra_domain::scheduling::{WorkClass, WorkQueueMetrics};

/// Typed backpressure reason returned when one class exceeds its bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("priority class {class:?} reached its queue bound")]
pub struct QueueFull {
    pub class: WorkClass,
}

struct Entry<T> {
    item: T,
    class: WorkClass,
    enqueued: Instant,
    sequence: u64,
}

/// Multi-class FIFO queue. Items within one class keep arrival order;
/// `pop` selects the most urgent class, applying aging promotion.
pub struct PriorityWorkQueue<T> {
    classes: [VecDeque<Entry<T>>; WorkClass::COUNT],
    per_class_capacity: usize,
    aging_after: Duration,
    next_sequence: u64,
    metrics: WorkQueueMetrics,
}

impl<T> std::fmt::Debug for PriorityWorkQueue<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PriorityWorkQueue")
            .field("per_class_capacity", &self.per_class_capacity)
            .field("aging_after", &self.aging_after)
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl<T> PriorityWorkQueue<T> {
    pub fn new(per_class_capacity: usize, aging_after: Duration) -> Self {
        Self {
            classes: std::array::from_fn(|_| VecDeque::new()),
            per_class_capacity,
            aging_after,
            next_sequence: 0,
            metrics: WorkQueueMetrics::default(),
        }
    }

    pub fn metrics(&self) -> WorkQueueMetrics {
        self.metrics
    }

    /// Admits one item into its class. A full class is a typed rejection, not
    /// silent growth: the caller decides how to degrade (drop with a counter,
    /// retry later, or fall back to a durable demand signal).
    pub fn push(&mut self, class: WorkClass, item: T, enqueued: Instant) -> Result<(), QueueFull> {
        let backlog = &mut self.classes[class.index()];
        if backlog.len() >= self.per_class_capacity {
            let counters = self.metrics.for_class_mut(class);
            counters.rejected = counters.rejected.saturating_add(1);
            return Err(QueueFull { class });
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        backlog.push_back(Entry {
            item,
            class,
            enqueued,
            sequence,
        });
        let counters = self.metrics.for_class_mut(class);
        counters.enqueued = counters.enqueued.saturating_add(1);
        Ok(())
    }

    /// Effective urgency of an entry after aging: one class per full
    /// `aging_after` waited, capped at the most urgent class.
    fn effective_rank(&self, entry: &Entry<T>) -> u8 {
        let base = (WorkClass::COUNT - 1 - entry.class.index()) as u8;
        if self.aging_after.is_zero() {
            return base;
        }
        let waited = entry.enqueued.elapsed().as_nanos();
        let intervals = u8::try_from(waited / self.aging_after.as_nanos()).unwrap_or(u8::MAX);
        base.saturating_add(intervals)
            .min((WorkClass::COUNT - 1) as u8)
    }

    fn take(&mut self, class: WorkClass, position: usize) -> Option<T> {
        let entry = self.classes[class.index()].remove(position)?;
        let counters = self.metrics.for_class_mut(class);
        counters.dequeued = counters.dequeued.saturating_add(1);
        counters.latency.record(entry.enqueued.elapsed());
        Some(entry.item)
    }

    /// Most urgent entry, aging applied; FIFO within equal urgency.
    pub fn pop(&mut self) -> Option<T> {
        let mut best: Option<(u8, u64, WorkClass)> = None;
        for class in WorkClass::ALL {
            let Some(front) = self.classes[class.index()].front() else {
                continue;
            };
            let rank = self.effective_rank(front);
            let candidate = (rank, u64::MAX - front.sequence, class);
            if best.is_none_or(|current| candidate > current) {
                best = Some(candidate);
            }
        }
        let (_, _, class) = best?;
        self.take(class, 0)
    }

    /// Most urgent entry accepted by `include`; entries not accepted stay
    /// queued. Class-filtered draining skips aging so a filtered consumer
    /// cannot be starved by an ineligible older entry of another class.
    pub fn pop_where(&mut self, mut include: impl FnMut(&T) -> bool) -> Option<T> {
        for class in WorkClass::ALL {
            let position = self.classes[class.index()]
                .iter()
                .position(|entry| include(&entry.item));
            if let Some(position) = position {
                return self.take(class, position);
            }
        }
        None
    }

    /// Removes obsolete queued work, returning how many entries were
    /// cancelled. Cancelled entries are counted per class.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) -> usize {
        let mut removed = 0_usize;
        for class in WorkClass::ALL {
            let backlog = &mut self.classes[class.index()];
            let before = backlog.len();
            backlog.retain(|entry| keep(&entry.item));
            let cancelled = before - backlog.len();
            let counters = self.metrics.for_class_mut(class);
            counters.cancelled = counters.cancelled.saturating_add(cancelled as u64);
            removed += cancelled;
        }
        removed
    }

    /// Marks one already-dequeued entry of `class` as obsolete at selection
    /// time (for example a filesystem signal whose epoch a newer reconcile
    /// already covered).
    pub fn note_superseded(&mut self, class: WorkClass) {
        let counters = self.metrics.for_class_mut(class);
        counters.superseded = counters.superseded.saturating_add(1);
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        self.classes.iter().map(VecDeque::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(capacity: usize, aging_after: Duration) -> PriorityWorkQueue<&'static str> {
        PriorityWorkQueue::new(capacity, aging_after)
    }

    #[test]
    fn freshness_edit_overtakes_older_background_work() -> Result<(), QueueFull> {
        let mut queue = queue(4, Duration::from_secs(60));
        let enqueued = Instant::now();
        queue.push(WorkClass::Maintenance, "maintenance", enqueued)?;
        queue.push(WorkClass::CacheWarmup, "warmup", enqueued)?;
        queue.push(WorkClass::Reconciliation, "reconcile", enqueued)?;
        queue.push(WorkClass::ProviderSync, "sync", enqueued)?;
        queue.push(WorkClass::FreshnessEdit, "edit", enqueued)?;

        assert_eq!(queue.pop(), Some("edit"));
        assert_eq!(queue.pop(), Some("sync"));
        assert_eq!(queue.pop(), Some("reconcile"));
        assert_eq!(queue.pop(), Some("warmup"));
        assert_eq!(queue.pop(), Some("maintenance"));
        assert!(queue.pop().is_none());

        let metrics = queue.metrics();
        for class in WorkClass::ALL {
            assert_eq!(metrics.for_class(class).enqueued, 1);
            assert_eq!(metrics.for_class(class).dequeued, 1);
            assert_eq!(metrics.for_class(class).latency.samples, 1);
        }
        Ok(())
    }

    #[test]
    fn equal_class_entries_stay_fifo() -> Result<(), QueueFull> {
        let mut queue = queue(4, Duration::from_secs(60));
        let enqueued = Instant::now();
        queue.push(WorkClass::FreshnessEdit, "first", enqueued)?;
        queue.push(WorkClass::FreshnessEdit, "second", enqueued)?;
        queue.push(WorkClass::Maintenance, "bg", enqueued)?;

        assert_eq!(queue.pop(), Some("first"));
        assert_eq!(queue.pop(), Some("second"));
        assert_eq!(queue.pop(), Some("bg"));
        Ok(())
    }

    #[test]
    fn full_class_rejects_with_a_typed_reason_without_dropping_others() -> Result<(), QueueFull> {
        let mut queue = queue(1, Duration::from_secs(60));
        let enqueued = Instant::now();
        queue.push(WorkClass::Reconciliation, "a", enqueued)?;

        let rejection = queue.push(WorkClass::Reconciliation, "b", enqueued);
        assert_eq!(
            rejection,
            Err(QueueFull {
                class: WorkClass::Reconciliation
            })
        );
        queue.push(WorkClass::FreshnessEdit, "edit", enqueued)?;

        assert_eq!(queue.pop(), Some("edit"));
        assert_eq!(queue.pop(), Some("a"));
        let metrics = queue.metrics();
        assert_eq!(metrics.for_class(WorkClass::Reconciliation).rejected, 1);
        assert_eq!(metrics.for_class(WorkClass::FreshnessEdit).rejected, 0);
        Ok(())
    }

    #[test]
    fn aging_promotes_long_waiting_background_work() -> Result<(), QueueFull> {
        let aging_after = Duration::from_millis(10);
        let mut queue = queue(4, aging_after);
        // Deterministic aging: the background entry is already old when it is
        // enqueued, so it has aged past `FreshnessEdit` urgency.
        let old = Instant::now() - 4 * aging_after;
        queue.push(WorkClass::Maintenance, "aged", old)?;
        queue.push(WorkClass::FreshnessEdit, "fresh", Instant::now())?;

        assert_eq!(queue.pop(), Some("aged"));
        assert_eq!(queue.pop(), Some("fresh"));
        Ok(())
    }

    #[test]
    fn retain_cancels_obsolete_work_and_counts_it_per_class() -> Result<(), QueueFull> {
        let mut queue = queue(4, Duration::from_secs(60));
        let enqueued = Instant::now();
        queue.push(WorkClass::Reconciliation, "checkpoint-1", enqueued)?;
        queue.push(WorkClass::Reconciliation, "checkpoint-2", enqueued)?;
        queue.push(WorkClass::FreshnessEdit, "edit", enqueued)?;

        let removed = queue.retain(|item| !item.starts_with("checkpoint"));
        assert_eq!(removed, 2);
        assert_eq!(queue.len(), 1);
        let metrics = queue.metrics();
        assert_eq!(metrics.for_class(WorkClass::Reconciliation).cancelled, 2);
        assert_eq!(metrics.for_class(WorkClass::FreshnessEdit).cancelled, 0);
        assert_eq!(queue.pop(), Some("edit"));
        Ok(())
    }

    #[test]
    fn pop_where_skips_ineligible_classes_without_consuming_them() -> Result<(), QueueFull> {
        let mut queue = queue(4, Duration::from_secs(60));
        let enqueued = Instant::now();
        queue.push(WorkClass::Reconciliation, "checkpoint", enqueued)?;
        queue.push(WorkClass::FreshnessEdit, "edit", enqueued)?;

        assert_eq!(queue.pop_where(|item| *item == "edit"), Some("edit"));
        assert_eq!(queue.pop_where(|item| *item == "edit"), None);
        assert_eq!(queue.pop(), Some("checkpoint"));
        Ok(())
    }
}
