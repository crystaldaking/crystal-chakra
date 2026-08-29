use std::sync::atomic::{AtomicU64, Ordering};

use super::ReconciliationKind;
use crate::indexer::ReconcileMetrics;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveIndexMetrics {
    pub barrier_requests: u64,
    pub barrier_generations_completed: u64,
    pub barrier_waiters_coalesced: u64,
    pub reconciliations: u64,
    pub reconciliation_failures: u64,
    pub published_revisions: u64,
    pub files_scanned: u64,
    pub files_reparsed: u64,
    pub relationship_files_recomputed: u64,
    pub framework_files_reparsed: u64,
    pub framework_relationship_files_recomputed: u64,
    pub framework_truncated_files: u64,
    pub unchanged_files: u64,
    pub created_files: u64,
    pub modified_files: u64,
    pub deleted_files: u64,
    pub syntax_error_files: u64,
    pub graph_files_reused: u64,
    pub graph_files_rebuilt: u64,
    pub graph_source_bytes_reused: u64,
    pub graph_source_bytes_rebuilt: u64,
    pub graph_source_bytes_copied: u64,
    pub graph_symbols_reused: u64,
    pub graph_symbols_rebuilt: u64,
    pub graph_symbols_copied: u64,
    pub graph_edges_reused: u64,
    pub graph_edges_rebuilt: u64,
    pub graph_edges_copied: u64,
    pub graph_call_sites_reused: u64,
    pub graph_call_sites_rebuilt: u64,
    pub graph_call_sites_copied: u64,
    pub watcher_events: u64,
    pub dropped_watcher_events: u64,
    pub watcher_errors: u64,
    pub watched_directories: u64,
    pub watcher_hint_paths: u64,
    pub git_subprocesses: u64,
    pub files_inspected: u64,
    pub source_bytes_inspected: u64,
    pub metadata_files_inspected: u64,
    pub metadata_bytes_inspected: u64,
    pub files_read: u64,
    pub source_bytes_read: u64,
    pub no_op_reconciliations: u64,
    pub targeted_reconciliations: u64,
    pub full_reconciliations: u64,
    pub watch_set_recomputations: u64,
    pub last_reconciliation_kind: ReconciliationKind,
}

#[derive(Debug, Default)]
pub(super) struct MetricsState {
    pub(super) barrier_requests: AtomicU64,
    pub(super) barrier_generations_completed: AtomicU64,
    pub(super) barrier_waiters_coalesced: AtomicU64,
    pub(super) reconciliations: AtomicU64,
    pub(super) reconciliation_failures: AtomicU64,
    pub(super) published_revisions: AtomicU64,
    pub(super) files_scanned: AtomicU64,
    pub(super) files_reparsed: AtomicU64,
    pub(super) relationship_files_recomputed: AtomicU64,
    pub(super) framework_files_reparsed: AtomicU64,
    pub(super) framework_relationship_files_recomputed: AtomicU64,
    pub(super) framework_truncated_files: AtomicU64,
    pub(super) unchanged_files: AtomicU64,
    pub(super) created_files: AtomicU64,
    pub(super) modified_files: AtomicU64,
    pub(super) deleted_files: AtomicU64,
    pub(super) syntax_error_files: AtomicU64,
    pub(super) graph_files_reused: AtomicU64,
    pub(super) graph_files_rebuilt: AtomicU64,
    pub(super) graph_source_bytes_reused: AtomicU64,
    pub(super) graph_source_bytes_rebuilt: AtomicU64,
    pub(super) graph_source_bytes_copied: AtomicU64,
    pub(super) graph_symbols_reused: AtomicU64,
    pub(super) graph_symbols_rebuilt: AtomicU64,
    pub(super) graph_symbols_copied: AtomicU64,
    pub(super) graph_edges_reused: AtomicU64,
    pub(super) graph_edges_rebuilt: AtomicU64,
    pub(super) graph_edges_copied: AtomicU64,
    pub(super) graph_call_sites_reused: AtomicU64,
    pub(super) graph_call_sites_rebuilt: AtomicU64,
    pub(super) graph_call_sites_copied: AtomicU64,
    pub(super) watcher_events: AtomicU64,
    pub(super) dropped_watcher_events: AtomicU64,
    pub(super) watcher_errors: AtomicU64,
    pub(super) watched_directories: AtomicU64,
    pub(super) watcher_hint_paths: AtomicU64,
    pub(super) git_subprocesses: AtomicU64,
    pub(super) files_inspected: AtomicU64,
    pub(super) source_bytes_inspected: AtomicU64,
    pub(super) metadata_files_inspected: AtomicU64,
    pub(super) metadata_bytes_inspected: AtomicU64,
    pub(super) files_read: AtomicU64,
    pub(super) source_bytes_read: AtomicU64,
    pub(super) no_op_reconciliations: AtomicU64,
    pub(super) targeted_reconciliations: AtomicU64,
    pub(super) full_reconciliations: AtomicU64,
    pub(super) watch_set_recomputations: AtomicU64,
    pub(super) last_reconciliation_kind: AtomicU64,
    pub(super) event_epoch: AtomicU64,
}

impl MetricsState {
    pub(super) fn snapshot(&self) -> LiveIndexMetrics {
        let load = |value: &AtomicU64| value.load(Ordering::Relaxed);
        LiveIndexMetrics {
            barrier_requests: load(&self.barrier_requests),
            barrier_generations_completed: load(&self.barrier_generations_completed),
            barrier_waiters_coalesced: load(&self.barrier_waiters_coalesced),
            reconciliations: load(&self.reconciliations),
            reconciliation_failures: load(&self.reconciliation_failures),
            published_revisions: load(&self.published_revisions),
            files_scanned: load(&self.files_scanned),
            files_reparsed: load(&self.files_reparsed),
            relationship_files_recomputed: load(&self.relationship_files_recomputed),
            framework_files_reparsed: load(&self.framework_files_reparsed),
            framework_relationship_files_recomputed: load(
                &self.framework_relationship_files_recomputed,
            ),
            framework_truncated_files: load(&self.framework_truncated_files),
            unchanged_files: load(&self.unchanged_files),
            created_files: load(&self.created_files),
            modified_files: load(&self.modified_files),
            deleted_files: load(&self.deleted_files),
            syntax_error_files: load(&self.syntax_error_files),
            graph_files_reused: load(&self.graph_files_reused),
            graph_files_rebuilt: load(&self.graph_files_rebuilt),
            graph_source_bytes_reused: load(&self.graph_source_bytes_reused),
            graph_source_bytes_rebuilt: load(&self.graph_source_bytes_rebuilt),
            graph_source_bytes_copied: load(&self.graph_source_bytes_copied),
            graph_symbols_reused: load(&self.graph_symbols_reused),
            graph_symbols_rebuilt: load(&self.graph_symbols_rebuilt),
            graph_symbols_copied: load(&self.graph_symbols_copied),
            graph_edges_reused: load(&self.graph_edges_reused),
            graph_edges_rebuilt: load(&self.graph_edges_rebuilt),
            graph_edges_copied: load(&self.graph_edges_copied),
            graph_call_sites_reused: load(&self.graph_call_sites_reused),
            graph_call_sites_rebuilt: load(&self.graph_call_sites_rebuilt),
            graph_call_sites_copied: load(&self.graph_call_sites_copied),
            watcher_events: load(&self.watcher_events),
            dropped_watcher_events: load(&self.dropped_watcher_events),
            watcher_errors: load(&self.watcher_errors),
            watched_directories: load(&self.watched_directories),
            watcher_hint_paths: load(&self.watcher_hint_paths),
            git_subprocesses: load(&self.git_subprocesses),
            files_inspected: load(&self.files_inspected),
            source_bytes_inspected: load(&self.source_bytes_inspected),
            metadata_files_inspected: load(&self.metadata_files_inspected),
            metadata_bytes_inspected: load(&self.metadata_bytes_inspected),
            files_read: load(&self.files_read),
            source_bytes_read: load(&self.source_bytes_read),
            no_op_reconciliations: load(&self.no_op_reconciliations),
            targeted_reconciliations: load(&self.targeted_reconciliations),
            full_reconciliations: load(&self.full_reconciliations),
            watch_set_recomputations: load(&self.watch_set_recomputations),
            last_reconciliation_kind: match load(&self.last_reconciliation_kind) {
                1 => ReconciliationKind::Noop,
                2 => ReconciliationKind::Targeted,
                3 => ReconciliationKind::Full,
                _ => ReconciliationKind::None,
            },
        }
    }

    pub(super) fn record_reconciliation_kind(&self, kind: ReconciliationKind) {
        let (counter, raw) = match kind {
            ReconciliationKind::None => return,
            ReconciliationKind::Noop => (&self.no_op_reconciliations, 1),
            ReconciliationKind::Targeted => (&self.targeted_reconciliations, 2),
            ReconciliationKind::Full => (&self.full_reconciliations, 3),
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.last_reconciliation_kind.store(raw, Ordering::Relaxed);
    }

    pub(super) fn record_barrier_completion(&self, covered: u64, completed_before: u64) {
        let completed = covered.saturating_sub(completed_before);
        self.barrier_generations_completed
            .fetch_add(completed, Ordering::Relaxed);
        self.barrier_waiters_coalesced
            .fetch_add(completed.saturating_sub(1), Ordering::Relaxed);
    }

    pub(super) fn record_reconcile(&self, metrics: ReconcileMetrics) {
        self.reconciliations.fetch_add(1, Ordering::Relaxed);
        self.files_scanned
            .fetch_add(metrics.scanned_files, Ordering::Relaxed);
        self.files_reparsed
            .fetch_add(metrics.reparsed_files, Ordering::Relaxed);
        self.relationship_files_recomputed
            .fetch_add(metrics.relationship_files_recomputed, Ordering::Relaxed);
        self.framework_files_reparsed
            .fetch_add(metrics.framework_files_reparsed, Ordering::Relaxed);
        self.framework_relationship_files_recomputed.fetch_add(
            metrics.framework_relationship_files_recomputed,
            Ordering::Relaxed,
        );
        self.framework_truncated_files
            .store(metrics.framework_truncated_files, Ordering::Relaxed);
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
        let publication = metrics.publication;
        self.graph_files_reused
            .fetch_add(publication.reused_files, Ordering::Relaxed);
        self.graph_files_rebuilt
            .fetch_add(publication.rebuilt_files, Ordering::Relaxed);
        self.graph_source_bytes_reused
            .fetch_add(publication.reused_source_bytes, Ordering::Relaxed);
        self.graph_source_bytes_rebuilt
            .fetch_add(publication.rebuilt_source_bytes, Ordering::Relaxed);
        self.graph_source_bytes_copied
            .fetch_add(publication.copied_source_bytes, Ordering::Relaxed);
        self.graph_symbols_reused
            .fetch_add(publication.reused_symbols, Ordering::Relaxed);
        self.graph_symbols_rebuilt
            .fetch_add(publication.rebuilt_symbols, Ordering::Relaxed);
        self.graph_symbols_copied
            .fetch_add(publication.copied_symbols, Ordering::Relaxed);
        self.graph_edges_reused
            .fetch_add(publication.reused_edges, Ordering::Relaxed);
        self.graph_edges_rebuilt
            .fetch_add(publication.rebuilt_edges, Ordering::Relaxed);
        self.graph_edges_copied
            .fetch_add(publication.copied_edges, Ordering::Relaxed);
        self.graph_call_sites_reused
            .fetch_add(publication.reused_call_sites, Ordering::Relaxed);
        self.graph_call_sites_rebuilt
            .fetch_add(publication.rebuilt_call_sites, Ordering::Relaxed);
        self.graph_call_sites_copied
            .fetch_add(publication.copied_call_sites, Ordering::Relaxed);
    }
}
