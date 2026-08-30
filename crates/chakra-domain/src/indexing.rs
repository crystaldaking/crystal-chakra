//! Language-neutral indexing budgets, coverage, and degradation metadata.
//!
//! These values describe the exact syntax revision carried by a query
//! envelope. They intentionally contain no Tree-sitter, Git, MCP, or LSP
//! types, so adapters can report bounded work without becoming part of the
//! domain model.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crate::operation::CancellationToken as IndexCancellation;
use crate::symbol::Language;

pub const DEFAULT_MAX_INDEX_FILES: u64 = 100_000;
pub const DEFAULT_MAX_SOURCE_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const DEFAULT_MAX_WORKSPACE_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
pub const DEFAULT_MAX_INDEX_SYMBOLS: u64 = 500_000;
pub const DEFAULT_MAX_INDEX_EDGES: u64 = 1_000_000;
pub const DEFAULT_MAX_INDEX_CALL_SITES: u64 = 1_000_000;
pub const DEFAULT_STARTUP_TARGET_MILLIS: u64 = 120_000;
pub const DEFAULT_MEMORY_TARGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_INDEX_WORKERS: u64 = 8;

pub const HARD_MAX_INDEX_FILES: u64 = 1_000_000;
pub const HARD_MAX_SOURCE_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const HARD_MAX_WORKSPACE_SOURCE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const HARD_MAX_INDEX_SYMBOLS: u64 = 2_000_000;
pub const HARD_MAX_INDEX_EDGES: u64 = 5_000_000;
pub const HARD_MAX_INDEX_CALL_SITES: u64 = 5_000_000;
pub const HARD_MAX_STARTUP_TARGET_MILLIS: u64 = 600_000;
pub const HARD_MAX_MEMORY_TARGET_BYTES: u64 = 128 * 1024 * 1024 * 1024;
pub const HARD_MAX_INDEX_WORKERS: u64 = 64;

/// Deterministic work/resource limits for one syntax workspace revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexBudgets {
    pub max_files: u64,
    pub max_source_file_bytes: u64,
    pub max_workspace_source_bytes: u64,
    pub max_symbols: u64,
    pub max_edges: u64,
    pub max_call_sites: u64,
    /// Observable startup target. Crossing it is reported, but never changes
    /// graph contents; deterministic work limits above control degradation.
    pub startup_target_millis: u64,
    /// Observable current/phase-sampled RSS target. It is not used to mutate
    /// graph contents because allocator/OS observations are nondeterministic.
    pub memory_target_bytes: u64,
    /// Upper bound for CPU workers. The effective value is additionally capped
    /// by available parallelism, the memory policy, and phase thresholds.
    #[serde(default = "default_max_index_workers")]
    pub max_workers: u64,
}

impl Default for IndexBudgets {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_INDEX_FILES,
            max_source_file_bytes: DEFAULT_MAX_SOURCE_FILE_BYTES,
            max_workspace_source_bytes: DEFAULT_MAX_WORKSPACE_SOURCE_BYTES,
            max_symbols: DEFAULT_MAX_INDEX_SYMBOLS,
            max_edges: DEFAULT_MAX_INDEX_EDGES,
            max_call_sites: DEFAULT_MAX_INDEX_CALL_SITES,
            startup_target_millis: DEFAULT_STARTUP_TARGET_MILLIS,
            memory_target_bytes: DEFAULT_MEMORY_TARGET_BYTES,
            max_workers: DEFAULT_MAX_INDEX_WORKERS,
        }
    }
}

impl IndexBudgets {
    pub fn validate(self) -> Result<Self, IndexBudgetError> {
        validate_limit("max_files", self.max_files, HARD_MAX_INDEX_FILES)?;
        validate_limit(
            "max_source_file_bytes",
            self.max_source_file_bytes,
            HARD_MAX_SOURCE_FILE_BYTES,
        )?;
        validate_limit(
            "max_workspace_source_bytes",
            self.max_workspace_source_bytes,
            HARD_MAX_WORKSPACE_SOURCE_BYTES,
        )?;
        validate_limit("max_symbols", self.max_symbols, HARD_MAX_INDEX_SYMBOLS)?;
        validate_limit("max_edges", self.max_edges, HARD_MAX_INDEX_EDGES)?;
        validate_limit(
            "max_call_sites",
            self.max_call_sites,
            HARD_MAX_INDEX_CALL_SITES,
        )?;
        validate_limit(
            "startup_target_millis",
            self.startup_target_millis,
            HARD_MAX_STARTUP_TARGET_MILLIS,
        )?;
        validate_limit(
            "memory_target_bytes",
            self.memory_target_bytes,
            HARD_MAX_MEMORY_TARGET_BYTES,
        )?;
        validate_limit("max_workers", self.max_workers, HARD_MAX_INDEX_WORKERS)?;
        if self.max_source_file_bytes > self.max_workspace_source_bytes {
            return Err(IndexBudgetError::FileExceedsWorkspace);
        }
        Ok(self)
    }
}

const fn default_max_index_workers() -> u64 {
    DEFAULT_MAX_INDEX_WORKERS
}

fn validate_limit(name: &'static str, value: u64, hard_max: u64) -> Result<(), IndexBudgetError> {
    if value == 0 || value > hard_max {
        return Err(IndexBudgetError::OutOfRange {
            name,
            value,
            hard_max,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IndexBudgetError {
    #[error("index budget `{name}` must be between 1 and {hard_max}, got {value}")]
    OutOfRange {
        name: &'static str,
        value: u64,
        hard_max: u64,
    },
    #[error("max_source_file_bytes cannot exceed max_workspace_source_bytes")]
    FileExceedsWorkspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IndexPhase {
    GitInventory,
    SourceRead,
    ParseExtraction,
    SymbolCatalog,
    Relationships,
    GraphMaterialization,
    GraphValidation,
    LanguageComposition,
    RevisionPublication,
    LiveReconciliation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IndexBudgetKind {
    Files,
    SourceFileBytes,
    WorkspaceSourceBytes,
    Symbols,
    Edges,
    CallSites,
    StartupWallTime,
    Memory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IndexCapability {
    FileInventory,
    TextSearch,
    Declarations,
    Relationships,
    CallSites,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexPhaseMeasurement {
    pub phase: IndexPhase,
    pub language: Option<Language>,
    pub elapsed_micros: u64,
    /// Process CPU consumed during the phase when the platform exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_micros: Option<u64>,
    /// CPU time divided by wall time, where 1,000 means one fully utilized
    /// logical CPU and 2,000 means two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_utilization_per_mille: Option<u64>,
    pub work_items: u64,
    pub bytes: u64,
    /// Effective workers selected for this phase after all caps/thresholds.
    #[serde(default)]
    pub effective_workers: u64,
    /// Observed high-water count of workers executing this phase at once.
    #[serde(default)]
    pub peak_active_workers: u64,
    /// Observed scheduler queue depth. Zero is truthful for cursor-based
    /// scheduling that has no retained task queue.
    #[serde(default)]
    pub peak_queue_depth: u64,
    /// Best-effort resident memory sample at the end of the phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
    /// Best-effort process high-water RSS observed by the end of the phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_bytes: Option<u64>,
}

/// Resource-aware scheduling facts for one completed syntax revision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexSchedulingMetrics {
    pub configured_max_workers: u64,
    pub available_parallelism: u64,
    /// Source bytes reserved before assigning memory to parser workers.
    pub source_memory_reserve_bytes: u64,
    /// Conservative private-memory allowance assigned to each parser worker.
    pub worker_memory_reserve_bytes: u64,
    pub memory_limited_workers: u64,
    pub effective_worker_limit: u64,
    pub peak_active_workers: u64,
    pub peak_queue_depth: u64,
    pub parallel_parse_files: u64,
    pub sequential_parse_files: u64,
    pub parallel_parse_file_threshold: u64,
    pub low_resource_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexDegradation {
    pub phase: IndexPhase,
    pub language: Option<Language>,
    pub cause: IndexBudgetKind,
    pub affected_capabilities: Vec<IndexCapability>,
    pub limit: u64,
    pub observed: u64,
    pub omitted: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexCapabilityCoverage {
    pub capability: IndexCapability,
    pub retained: u64,
    pub omitted: u64,
    pub complete: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexCoverage {
    pub discovered_files: u64,
    pub indexed_files: u64,
    pub skipped_files: u64,
    /// Source or metadata files skipped because they could not be read or
    /// decoded (for example non-UTF-8 contents or a file that vanished
    /// between inventory and read). Skipping never aborts the index.
    #[serde(default)]
    pub unreadable_files: u64,
    pub source_bytes: u64,
    pub parsed_files: u64,
    pub syntax_error_files: u64,
    pub extracted_symbols: u64,
    pub retained_symbols: u64,
    pub retained_edges: u64,
    pub omitted_edges: u64,
    pub extracted_call_sites: u64,
    pub retained_call_sites: u64,
    pub omitted_call_sites: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexMemoryMetrics {
    pub retained_source_bytes: u64,
    pub retained_parsed_symbols: u64,
    pub retained_parsed_relationship_edges: u64,
    pub retained_parsed_call_sites: u64,
    pub retained_graph_symbols: u64,
    pub retained_graph_edges: u64,
    pub retained_graph_call_sites: u64,
    /// Best-effort platform sample at the end of the operation.
    pub current_rss_bytes: Option<u64>,
    /// Maximum of best-effort phase-boundary samples, not an OS peak claim.
    pub observed_phase_peak_rss_bytes: Option<u64>,
    /// Optional precise-provider/cache accounting. `None` means the provider
    /// adapter cannot currently supply byte-exact retained size.
    pub provider_cache_bytes: Option<u64>,
}

/// Exact graph-assembly work for the revision carrying this status.
///
/// `reused_*` payloads remain physically shared with the previous immutable
/// revision. `copied_edges` counts retained adjacency entries copied while a
/// touched per-entity vector is replaced; other `copied_*` fields count their
/// corresponding payloads. Persistent-map path nodes and scalar index keys are
/// intentionally not reported as domain facts. Initial/full builds report
/// everything as rebuilt.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct IndexPublicationMetrics {
    pub structurally_incremental: bool,
    pub reused_files: u64,
    pub rebuilt_files: u64,
    pub reused_source_bytes: u64,
    pub rebuilt_source_bytes: u64,
    pub copied_source_bytes: u64,
    pub reused_symbols: u64,
    pub rebuilt_symbols: u64,
    pub copied_symbols: u64,
    pub reused_edges: u64,
    pub rebuilt_edges: u64,
    pub copied_edges: u64,
    pub reused_call_sites: u64,
    pub rebuilt_call_sites: u64,
    pub copied_call_sites: u64,
}

/// Metadata atomically attached to one published syntax revision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexingStatus {
    pub budgets: IndexBudgets,
    pub coverage: IndexCoverage,
    pub capabilities: Vec<IndexCapabilityCoverage>,
    pub degradations: Vec<IndexDegradation>,
    pub phases: Vec<IndexPhaseMeasurement>,
    #[serde(default)]
    pub scheduling: IndexSchedulingMetrics,
    pub memory: IndexMemoryMetrics,
    pub publication: IndexPublicationMetrics,
}

impl IndexingStatus {
    pub fn is_degraded(&self) -> bool {
        !self.degradations.is_empty()
    }

    /// Revision metadata suitable for non-status query envelopes.
    ///
    /// Coverage, capability, degradation, memory, scheduling, and publication
    /// facts still explain the exact observed revision. The per-phase cold
    /// index history is operator telemetry and remains available from
    /// `status`; repeating it in every small query wastes response bytes.
    pub fn query_summary(&self) -> Self {
        Self {
            budgets: self.budgets,
            coverage: self.coverage.clone(),
            capabilities: self.capabilities.clone(),
            degradations: self.degradations.clone(),
            phases: Vec::new(),
            scheduling: self.scheduling.clone(),
            memory: self.memory.clone(),
            publication: self.publication,
        }
    }
}

// --- Machine-readable live indexing diagnostics (issue #43) ---

/// Maximum number of retained per-file invalidation records. Older records
/// are dropped; `IndexingDiagnostics::file_invalidation_records` keeps the
/// cumulative total so the bound is observable.
pub const MAX_FILE_INVALIDATION_RECORDS: usize = 32;

/// Why the syntax fact cache reports no activity.
///
/// The per-file syntax fact cache built for issue #39 did not meet its
/// persistence acceptance benchmarks and was deliberately not shipped, so
/// there are no hit/miss/rebuild or corruption-fallback counters to report.
pub const SYNTAX_FACT_CACHE_DISABLED_REASON: &str =
    "rejected by persistence acceptance benchmarks (docs/evaluation/v0.2.0-syntax-fact-cache.md)";

/// How one reconciliation pass classified its work.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationKind {
    /// No reconciliation has completed yet.
    #[default]
    None,
    /// Freshness was re-proven without rereading any source body.
    Noop,
    /// Only changed/added/removed files were reprocessed.
    Targeted,
    /// Every retained source body was reread.
    Full,
}

/// Why one file was reindexed, as far as the reconciliation paths can
/// actually distinguish. Content bytes never appear in diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileInvalidationReason {
    /// The file entered the Git-discovered source/metadata inventory.
    Added,
    /// The file identity changed and its source bytes differ from the
    /// retained snapshot.
    ContentChanged,
    /// The file identity changed without a content change (for example a
    /// timestamp-only rewrite), or a manifest/metadata input such as
    /// `Cargo.toml` or `composer.json` changed.
    MetadataChanged,
    /// The file left the Git-discovered source/metadata inventory.
    Removed,
}

/// One bounded per-file invalidation record: path and reason only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileInvalidation {
    pub path: crate::location::RepoRelativePath,
    pub reason: FileInvalidationReason,
}

/// Why a workspace reconciliation was forced to reread every retained
/// source body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FullReconciliationReason {
    /// First reconciliation after startup: no retained source snapshot.
    ColdStart,
    /// The filesystem watcher reported a new error.
    WatcherError,
    /// Watcher events were dropped from the bounded event queue.
    WatcherEventMissed,
    /// Event hints were missing, overflowed, or arrived with an epoch gap.
    UncertainEventHints,
    /// The configured periodic full-reread checkpoint was reached.
    PeriodicCheckpoint,
    /// A stable-scan attempt observed the inventory or file identities
    /// changing underneath it and escalated its retry to a full reread.
    ScanInstability,
}

/// Cumulative full-reconciliation causes observed by the live owner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FullReconciliationReasonCounts {
    pub cold_start: u64,
    pub watcher_error: u64,
    pub watcher_event_missed: u64,
    pub uncertain_event_hints: u64,
    pub periodic_checkpoint: u64,
    pub scan_instability: u64,
}

/// Health of the persistent syntax fact cache.
///
/// The cache is currently disabled (see
/// [`SYNTAX_FACT_CACHE_DISABLED_REASON`]), so no hit/miss/rebuild or
/// corruption-fallback counters exist; reporting fabricated zeros would be
/// dishonest. Cold restore, warm hit, metadata invalidation, and corruption
/// fallback counters are deferred together with the cache itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum CacheHealth {
    Disabled { reason: String },
}

/// Cumulative rebuild/reconcile counters owned by the indexing pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReconciliationCounters {
    /// Published revisions whose graph was fully rebuilt instead of
    /// structurally reused (initial cold builds), counted by the engine at
    /// publication time.
    pub cold_builds: u64,
    pub no_op_reconciliations: u64,
    pub targeted_reconciliations: u64,
    pub full_reconciliations: u64,
    /// Reconciliations caused by exactly one modified file and no
    /// additions/removals.
    pub one_file_edits: u64,
    pub reconciliation_failures: u64,
    pub published_revisions: u64,
}

/// Live freshness-owner queue state. All values are counters or bounded
/// capacities; the std sync-channel depth is not observable, so the durable
/// barrier generations are reported instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LiveQueueState {
    /// Highest freshness-barrier generation waiters have requested.
    pub requested_barrier_generation: u64,
    /// Highest freshness-barrier generation completed by the worker.
    pub completed_barrier_generation: u64,
    pub barrier_requests: u64,
    pub barrier_waiters_coalesced: u64,
    pub watcher_events: u64,
    pub dropped_watcher_events: u64,
    pub watcher_errors: u64,
    pub watched_directories: u64,
    pub watcher_event_queue_capacity: u64,
}

/// Cumulative and latest typed project-scope invalidation evidence.
///
/// Aggregate counters describe all successful live reconciliations. The
/// detailed impact is already bounded by the project model contract and
/// identifies the latest changed units, their typed reasons, affected
/// dependents, and manifest-issue transitions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectInvalidationDiagnostics {
    pub impacted_units: u64,
    pub impacted_dependents: u64,
    pub manifest_issue_changes: u64,
    pub unit_changes: crate::project::ProjectUnitChangeCounts,
    pub last_impact: Option<crate::project::ProjectModelImpact>,
}

/// Bounded, typed, source-content-free live indexing diagnostics (issue #43).
///
/// Phase timings, coverage, resource samples, degraded capabilities, and
/// graph publication facts are revision-scoped and already travel in the
/// query envelope's [`IndexingStatus`]; this struct carries the cross-revision
/// operational picture owned by the live indexing pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexingDiagnostics {
    pub cache: CacheHealth,
    pub counters: ReconciliationCounters,
    pub queue: LiveQueueState,
    /// Why manifest/config inputs most recently invalidated project scopes,
    /// plus cumulative impact counters.
    #[serde(default)]
    pub project_invalidations: ProjectInvalidationDiagnostics,
    pub last_reconciliation_kind: ReconciliationKind,
    /// Cumulative causes of full (project-scope) reconciliations.
    pub full_reconciliation_reasons: FullReconciliationReasonCounts,
    /// Causes that forced the most recent full reconciliation. Empty when no
    /// full reconciliation has run; stable-scan retries report
    /// `scan_instability` explicitly.
    pub last_full_reconciliation_reasons: Vec<FullReconciliationReason>,
    /// Newest-last per-file invalidation records, capped at
    /// [`MAX_FILE_INVALIDATION_RECORDS`].
    pub recent_file_invalidations: Vec<FileInvalidation>,
    /// Cumulative per-file invalidation records produced, including those
    /// dropped from the bounded `recent_file_invalidations` window.
    pub file_invalidation_records: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_bounded() {
        assert_eq!(
            IndexBudgets::default().validate(),
            Ok(IndexBudgets::default())
        );
    }

    #[test]
    fn invalid_budgets_are_rejected() {
        let zero = IndexBudgets {
            max_files: 0,
            ..IndexBudgets::default()
        };
        assert!(matches!(
            zero.validate(),
            Err(IndexBudgetError::OutOfRange {
                name: "max_files",
                ..
            })
        ));

        let inverted = IndexBudgets {
            max_source_file_bytes: 2,
            max_workspace_source_bytes: 1,
            ..IndexBudgets::default()
        };
        assert_eq!(
            inverted.validate(),
            Err(IndexBudgetError::FileExceedsWorkspace)
        );

        let too_many_workers = IndexBudgets {
            max_workers: HARD_MAX_INDEX_WORKERS + 1,
            ..IndexBudgets::default()
        };
        assert!(matches!(
            too_many_workers.validate(),
            Err(IndexBudgetError::OutOfRange {
                name: "max_workers",
                ..
            })
        ));
    }

    #[test]
    fn cancellation_is_shared_between_owners() {
        let cancellation = IndexCancellation::default();
        let observer = cancellation.clone();
        assert!(!observer.is_cancelled());
        cancellation.cancel();
        assert!(observer.is_cancelled());
    }

    #[test]
    fn query_summary_keeps_revision_evidence_without_phase_history() {
        let status = IndexingStatus {
            phases: vec![IndexPhaseMeasurement {
                phase: IndexPhase::GitInventory,
                language: None,
                elapsed_micros: 7,
                cpu_micros: Some(3),
                cpu_utilization_per_mille: Some(428),
                work_items: 2,
                bytes: 0,
                effective_workers: 1,
                peak_active_workers: 1,
                peak_queue_depth: 0,
                rss_bytes: None,
                peak_rss_bytes: None,
            }],
            degradations: vec![IndexDegradation {
                phase: IndexPhase::ParseExtraction,
                language: Some(Language::Rust),
                cause: IndexBudgetKind::Files,
                affected_capabilities: vec![IndexCapability::Declarations],
                limit: 1,
                observed: 2,
                omitted: 1,
            }],
            ..IndexingStatus::default()
        };

        let summary = status.query_summary();
        assert!(summary.phases.is_empty());
        assert_eq!(summary.degradations, status.degradations);
        assert_eq!(summary.coverage, status.coverage);
        assert_eq!(summary.publication, status.publication);
    }

    #[test]
    fn diagnostics_serialize_typed_bounded_and_source_free()
    -> Result<(), Box<dyn std::error::Error>> {
        const SOURCE_MARKER: &str = "fn secret_internal_payload() { leak() }";
        let diagnostics = IndexingDiagnostics {
            cache: CacheHealth::Disabled {
                reason: SYNTAX_FACT_CACHE_DISABLED_REASON.to_owned(),
            },
            counters: ReconciliationCounters {
                cold_builds: 1,
                no_op_reconciliations: 2,
                targeted_reconciliations: 3,
                full_reconciliations: 1,
                one_file_edits: 3,
                reconciliation_failures: 0,
                published_revisions: 4,
            },
            queue: LiveQueueState {
                requested_barrier_generation: 6,
                completed_barrier_generation: 6,
                watcher_event_queue_capacity: 256,
                ..LiveQueueState::default()
            },
            project_invalidations: ProjectInvalidationDiagnostics::default(),
            last_reconciliation_kind: ReconciliationKind::Targeted,
            full_reconciliation_reasons: FullReconciliationReasonCounts {
                cold_start: 1,
                ..FullReconciliationReasonCounts::default()
            },
            last_full_reconciliation_reasons: vec![FullReconciliationReason::ColdStart],
            recent_file_invalidations: vec![FileInvalidation {
                path: crate::location::RepoRelativePath::new("src/secret.rs")?,
                reason: FileInvalidationReason::ContentChanged,
            }],
            file_invalidation_records: 1,
        };

        let json = serde_json::to_string(&diagnostics)?;
        assert!(json.contains("\"cache\":{\"state\":\"disabled\""));
        assert!(json.contains("\"reason\":\"content_changed\""));
        assert!(json.contains("\"last_reconciliation_kind\":\"targeted\""));
        assert!(json.contains("\"cold_start\""));
        // Diagnostics carry paths and counters only, never source contents.
        assert!(!json.contains(SOURCE_MARKER));
        assert!(json.contains("src/secret.rs"));

        let roundtrip: IndexingDiagnostics = serde_json::from_str(&json)?;
        assert_eq!(roundtrip, diagnostics);
        Ok(())
    }

    #[test]
    fn diagnostics_window_bound_is_explicit() {
        // The retention bound is a published constant so consumers can rely
        // on it; the cumulative counter makes dropped records observable.
        assert_eq!(MAX_FILE_INVALIDATION_RECORDS, 32);
    }
}
