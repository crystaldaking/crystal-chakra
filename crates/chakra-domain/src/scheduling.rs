//! Typed priority classes and queue-latency instrumentation for scheduled
//! indexing work (issue #44).
//!
//! The classes below are the shared vocabulary for every place indexing or
//! provider work waits in a queue. They map to real work in the current
//! architecture:
//!
//! - [`WorkClass::FreshnessEdit`]: live watcher events and freshness-barrier
//!   demand in the live syntax worker (`chakra-language` `live`).
//! - [`WorkClass::ProviderSync`]: provider requests admitted by the provider
//!   pool (`chakra-provider-pool`), which synchronize provider documents
//!   before answering. The pool keeps its own three-level admission priority
//!   within this class.
//! - [`WorkClass::Reconciliation`]: deferred full content rereads (the
//!   periodic reconciliation checkpoint) in the live syntax worker.
//! - [`WorkClass::CacheWarmup`]: the initial cold index build before serving.
//!   It runs once at startup and is not queued today; the class exists so
//!   future warmup producers enter any scheduler below freshness work.
//! - [`WorkClass::Maintenance`]: provider-pool reaper evictions and watch-set
//!   maintenance. These run on their own owner threads and are not queued
//!   today; the class reserves the lowest band for them.

use std::time::Duration;

/// Bounded priority class of one unit of scheduled work. Ordering is
/// significance: [`WorkClass::FreshnessEdit`] is the greatest (most urgent)
/// and [`WorkClass::Maintenance`] the least.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkClass {
    Maintenance,
    CacheWarmup,
    Reconciliation,
    ProviderSync,
    FreshnessEdit,
}

impl WorkClass {
    /// Every class, ordered from most to least urgent.
    pub const ALL: [Self; Self::COUNT] = [
        Self::FreshnessEdit,
        Self::ProviderSync,
        Self::Reconciliation,
        Self::CacheWarmup,
        Self::Maintenance,
    ];

    pub const COUNT: usize = 5;

    /// Metrics-array index. Classes are stored most urgent first, so
    /// `WorkClass::ALL[class.index()] == class`.
    pub fn index(self) -> usize {
        match self {
            Self::FreshnessEdit => 0,
            Self::ProviderSync => 1,
            Self::Reconciliation => 2,
            Self::CacheWarmup => 3,
            Self::Maintenance => 4,
        }
    }
}

/// Histogram-lite queue-wait instrumentation: sample count plus total and
/// maximum observed wait.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub struct QueueLatencyStats {
    pub samples: u64,
    pub total_micros: u64,
    pub max_micros: u64,
}

impl QueueLatencyStats {
    pub fn record(&mut self, wait: Duration) {
        let micros = u64::try_from(wait.as_micros()).unwrap_or(u64::MAX);
        self.samples = self.samples.saturating_add(1);
        self.total_micros = self.total_micros.saturating_add(micros);
        self.max_micros = self.max_micros.max(micros);
    }
}

/// Typed per-class queue counters. `rejected` counts backpressure refusals of
/// a full class; `cancelled` counts obsolete work removed while queued;
/// `superseded` counts dequeued work that was already obsolete at selection.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub struct WorkClassQueueMetrics {
    pub enqueued: u64,
    pub dequeued: u64,
    pub cancelled: u64,
    pub superseded: u64,
    pub rejected: u64,
    pub latency: QueueLatencyStats,
}

/// Queue instrumentation keyed by [`WorkClass`], most urgent class first.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub struct WorkQueueMetrics {
    pub classes: [WorkClassQueueMetrics; WorkClass::COUNT],
}

impl WorkQueueMetrics {
    pub fn for_class(&self, class: WorkClass) -> WorkClassQueueMetrics {
        self.classes[class.index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urgency_ordering_and_metrics_indexing_are_consistent() {
        assert!(WorkClass::FreshnessEdit > WorkClass::ProviderSync);
        assert!(WorkClass::ProviderSync > WorkClass::Reconciliation);
        assert!(WorkClass::Reconciliation > WorkClass::CacheWarmup);
        assert!(WorkClass::CacheWarmup > WorkClass::Maintenance);
        for class in WorkClass::ALL {
            assert_eq!(WorkClass::ALL[class.index()], class);
        }
    }

    #[test]
    fn latency_stats_track_count_total_and_max() {
        let mut stats = QueueLatencyStats::default();
        stats.record(Duration::from_micros(5));
        stats.record(Duration::from_micros(9));
        assert_eq!(stats.samples, 2);
        assert_eq!(stats.total_micros, 14);
        assert_eq!(stats.max_micros, 9);
    }
}
