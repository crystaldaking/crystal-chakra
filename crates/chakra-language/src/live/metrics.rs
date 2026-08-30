use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use chakra_domain::indexing::{
    FileInvalidation, FullReconciliationReason, FullReconciliationReasonCounts,
    MAX_FILE_INVALIDATION_RECORDS, ProjectInvalidationDiagnostics, ReconciliationKind,
};
use chakra_domain::project::ProjectModelImpact;
use chakra_domain::scheduling::WorkQueueMetrics;

use crate::indexer::{DependencyImpactMetrics, ReconcileMetrics};

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
    /// Retained files whose manifest-derived metadata record was replaced
    /// without a source reparse (issue #40).
    pub metadata_files_recomputed: u64,
    /// Framework-enrichment configuration toggles applied (issue #40).
    pub framework_config_changes: u64,
    /// Accumulated typed external-input invalidation picture (issue #40).
    pub dependency_impact: DependencyImpactMetrics,
    pub watch_set_recomputations: u64,
    pub last_reconciliation_kind: ReconciliationKind,
    /// Per-class staging queue instrumentation: queue latency and typed
    /// drop/cancellation counters for scheduled work (issue #44).
    pub queue: WorkQueueMetrics,
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
    pub(super) metadata_files_recomputed: AtomicU64,
    pub(super) framework_config_changes: AtomicU64,
    pub(super) dependency_impacted_units: AtomicU64,
    pub(super) dependency_impacted_dependents: AtomicU64,
    pub(super) dependency_manifest_issue_changes: AtomicU64,
    pub(super) dependency_units_added: AtomicU64,
    pub(super) dependency_units_removed: AtomicU64,
    pub(super) dependency_units_definition_changed: AtomicU64,
    pub(super) dependency_units_source_roots_changed: AtomicU64,
    pub(super) dependency_units_dependencies_changed: AtomicU64,
    pub(super) dependency_units_membership_changed: AtomicU64,
    pub(super) one_file_edits: AtomicU64,
    pub(super) full_reason_cold_start: AtomicU64,
    pub(super) full_reason_watcher_error: AtomicU64,
    pub(super) full_reason_watcher_event_missed: AtomicU64,
    pub(super) full_reason_uncertain_event_hints: AtomicU64,
    pub(super) full_reason_periodic_checkpoint: AtomicU64,
    pub(super) full_reason_scan_instability: AtomicU64,
    /// Bitmask of the `FullReconciliationReason` values that forced the most
    /// recent full reconciliation; zero when none has run.
    pub(super) last_full_reason_bits: AtomicU64,
    pub(super) file_invalidation_records: AtomicU64,
    /// Bounded newest-last per-file invalidation window.
    pub(super) invalidations: Mutex<VecDeque<FileInvalidation>>,
    /// Latest non-empty bounded project-model impact.
    pub(super) last_project_impact: Mutex<Option<ProjectModelImpact>>,
    pub(super) watch_set_recomputations: AtomicU64,
    pub(super) last_reconciliation_kind: AtomicU64,
    pub(super) event_epoch: AtomicU64,
    pub(super) queue: Mutex<WorkQueueMetrics>,
}

#[derive(Debug, Default)]
pub(super) struct FileInvalidationBatch {
    pub(super) records: u64,
    pub(super) recent: VecDeque<FileInvalidation>,
}

impl FileInvalidationBatch {
    pub(super) fn push(&mut self, invalidation: FileInvalidation) {
        self.records = self.records.saturating_add(1);
        if self.recent.len() >= MAX_FILE_INVALIDATION_RECORDS {
            self.recent.pop_front();
        }
        self.recent.push_back(invalidation);
    }
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
            metadata_files_recomputed: load(&self.metadata_files_recomputed),
            framework_config_changes: load(&self.framework_config_changes),
            dependency_impact: DependencyImpactMetrics {
                impacted_units: load(&self.dependency_impacted_units),
                impacted_dependents: load(&self.dependency_impacted_dependents),
                manifest_issue_changes: load(&self.dependency_manifest_issue_changes),
                unit_changes: chakra_domain::project::ProjectUnitChangeCounts {
                    added: load(&self.dependency_units_added),
                    removed: load(&self.dependency_units_removed),
                    definition_changed: load(&self.dependency_units_definition_changed),
                    source_roots_changed: load(&self.dependency_units_source_roots_changed),
                    dependencies_changed: load(&self.dependency_units_dependencies_changed),
                    membership_changed: load(&self.dependency_units_membership_changed),
                },
            },
            watch_set_recomputations: load(&self.watch_set_recomputations),
            last_reconciliation_kind: match load(&self.last_reconciliation_kind) {
                1 => ReconciliationKind::Noop,
                2 => ReconciliationKind::Targeted,
                3 => ReconciliationKind::Full,
                _ => ReconciliationKind::None,
            },
            queue: self
                .queue
                .lock()
                .map(|queue| *queue)
                .unwrap_or_else(|poisoned| *poisoned.into_inner()),
        }
    }

    /// Publishes the worker-owned staging queue instrumentation. The queue
    /// itself lives on the worker thread; readers see the last published
    /// snapshot.
    pub(super) fn publish_queue_metrics(&self, queue: WorkQueueMetrics) {
        let mut slot = match self.queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = queue;
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

    pub(super) fn record_full_reconciliation_reasons(&self, reasons: &[FullReconciliationReason]) {
        let mut bits = 0_u64;
        for reason in reasons {
            let (counter, bit) = match reason {
                FullReconciliationReason::ColdStart => (&self.full_reason_cold_start, 1),
                FullReconciliationReason::WatcherError => (&self.full_reason_watcher_error, 2),
                FullReconciliationReason::WatcherEventMissed => {
                    (&self.full_reason_watcher_event_missed, 4)
                }
                FullReconciliationReason::UncertainEventHints => {
                    (&self.full_reason_uncertain_event_hints, 8)
                }
                FullReconciliationReason::PeriodicCheckpoint => {
                    (&self.full_reason_periodic_checkpoint, 16)
                }
                FullReconciliationReason::ScanInstability => {
                    (&self.full_reason_scan_instability, 32)
                }
            };
            counter.fetch_add(1, Ordering::Relaxed);
            bits |= bit;
        }
        self.last_full_reason_bits.store(bits, Ordering::Relaxed);
    }

    pub(super) fn full_reconciliation_reason_counts(&self) -> FullReconciliationReasonCounts {
        let load = |value: &AtomicU64| value.load(Ordering::Relaxed);
        FullReconciliationReasonCounts {
            cold_start: load(&self.full_reason_cold_start),
            watcher_error: load(&self.full_reason_watcher_error),
            watcher_event_missed: load(&self.full_reason_watcher_event_missed),
            uncertain_event_hints: load(&self.full_reason_uncertain_event_hints),
            periodic_checkpoint: load(&self.full_reason_periodic_checkpoint),
            scan_instability: load(&self.full_reason_scan_instability),
        }
    }

    pub(super) fn project_invalidation_diagnostics(&self) -> ProjectInvalidationDiagnostics {
        let last_impact = match self.last_project_impact.lock() {
            Ok(impact) => impact.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        ProjectInvalidationDiagnostics {
            impacted_units: self.dependency_impacted_units.load(Ordering::Relaxed),
            impacted_dependents: self.dependency_impacted_dependents.load(Ordering::Relaxed),
            manifest_issue_changes: self
                .dependency_manifest_issue_changes
                .load(Ordering::Relaxed),
            unit_changes: chakra_domain::project::ProjectUnitChangeCounts {
                added: self.dependency_units_added.load(Ordering::Relaxed),
                removed: self.dependency_units_removed.load(Ordering::Relaxed),
                definition_changed: self
                    .dependency_units_definition_changed
                    .load(Ordering::Relaxed),
                source_roots_changed: self
                    .dependency_units_source_roots_changed
                    .load(Ordering::Relaxed),
                dependencies_changed: self
                    .dependency_units_dependencies_changed
                    .load(Ordering::Relaxed),
                membership_changed: self
                    .dependency_units_membership_changed
                    .load(Ordering::Relaxed),
            },
            last_impact,
        }
    }

    pub(super) fn record_project_impact(&self, impact: Option<&ProjectModelImpact>) {
        let Some(impact) = impact else {
            return;
        };
        let mut last = match self.last_project_impact.lock() {
            Ok(last) => last,
            Err(poisoned) => poisoned.into_inner(),
        };
        *last = Some(impact.clone());
    }

    pub(super) fn last_full_reconciliation_reasons(&self) -> Vec<FullReconciliationReason> {
        let bits = self.last_full_reason_bits.load(Ordering::Relaxed);
        let mut reasons = Vec::new();
        for (bit, reason) in [
            (1, FullReconciliationReason::ColdStart),
            (2, FullReconciliationReason::WatcherError),
            (4, FullReconciliationReason::WatcherEventMissed),
            (8, FullReconciliationReason::UncertainEventHints),
            (16, FullReconciliationReason::PeriodicCheckpoint),
            (32, FullReconciliationReason::ScanInstability),
        ] {
            if bits & bit != 0 {
                reasons.push(reason);
            }
        }
        reasons
    }

    pub(super) fn record_file_invalidations(&self, invalidations: FileInvalidationBatch) {
        if invalidations.records == 0 {
            return;
        }
        self.file_invalidation_records
            .fetch_add(invalidations.records, Ordering::Relaxed);
        let mut retained = match self.invalidations.lock() {
            Ok(retained) => retained,
            Err(poisoned) => poisoned.into_inner(),
        };
        for invalidation in invalidations.recent {
            if retained.len() >= MAX_FILE_INVALIDATION_RECORDS {
                retained.pop_front();
            }
            retained.push_back(invalidation);
        }
    }

    pub(super) fn recent_file_invalidations(&self) -> Vec<FileInvalidation> {
        let retained = match self.invalidations.lock() {
            Ok(retained) => retained,
            Err(poisoned) => poisoned.into_inner(),
        };
        retained.iter().cloned().collect()
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
        if metrics.modified_files == 1 && metrics.created_files == 0 && metrics.deleted_files == 0 {
            self.one_file_edits.fetch_add(1, Ordering::Relaxed);
        }
        self.files_scanned
            .fetch_add(metrics.scanned_files, Ordering::Relaxed);
        self.files_reparsed
            .fetch_add(metrics.reparsed_files, Ordering::Relaxed);
        self.relationship_files_recomputed
            .fetch_add(metrics.relationship_files_recomputed, Ordering::Relaxed);
        self.metadata_files_recomputed
            .fetch_add(metrics.metadata_files_recomputed, Ordering::Relaxed);
        self.framework_config_changes
            .fetch_add(metrics.framework_config_changes, Ordering::Relaxed);
        let impact = &metrics.dependency_impact;
        self.dependency_impacted_units
            .fetch_add(impact.impacted_units, Ordering::Relaxed);
        self.dependency_impacted_dependents
            .fetch_add(impact.impacted_dependents, Ordering::Relaxed);
        self.dependency_manifest_issue_changes
            .fetch_add(impact.manifest_issue_changes, Ordering::Relaxed);
        self.dependency_units_added
            .fetch_add(impact.unit_changes.added, Ordering::Relaxed);
        self.dependency_units_removed
            .fetch_add(impact.unit_changes.removed, Ordering::Relaxed);
        self.dependency_units_definition_changed
            .fetch_add(impact.unit_changes.definition_changed, Ordering::Relaxed);
        self.dependency_units_source_roots_changed
            .fetch_add(impact.unit_changes.source_roots_changed, Ordering::Relaxed);
        self.dependency_units_dependencies_changed
            .fetch_add(impact.unit_changes.dependencies_changed, Ordering::Relaxed);
        self.dependency_units_membership_changed
            .fetch_add(impact.unit_changes.membership_changed, Ordering::Relaxed);
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
