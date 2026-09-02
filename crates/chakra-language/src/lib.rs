//! Multi-language syntax index composition and live workspace ownership.
//!
//! Rust and PHP adapters build independent syntax graphs. This crate combines
//! them into one atomically published workspace revision and owns the single
//! watcher/freshness barrier for that revision.

mod adapter;
mod indexer;
mod live;
mod scheduler;

pub use adapter::{
    AdapterBuildMetrics, AdapterColdBuild, AdapterFactCounts, AdapterFrameworkMetrics,
    AdapterReconcile, AdapterReconcileMetrics, DependencyEvidence, LanguageSources,
    SyntaxLanguageAdapter, default_adapters, registered_languages,
};
pub use chakra_domain::indexing::ReconciliationKind;
pub use indexer::{
    COMMIT_SNAPSHOT_FORMAT_VERSION, COMMIT_SNAPSHOT_GRAPH_MODEL_VERSION, CommitIndexProvider,
    CommitIndexReport, CommitSnapshotCompatibility, CommitSnapshotPayloadError,
    DependencyImpactMetrics, IndexMetrics, IndexOptions, IndexReport, LayeredIndexReport,
    ReconcileMetrics, ReconcileReport, WorkspaceIndexError, WorkspaceLanguageSources,
    WorkspaceSourceScan, WorkspaceSources, WorkspaceSyntaxIndex, index_commit_with_options,
    index_head_commit_with_options, index_repository, index_repository_with_options,
    scan_repository_sources, scan_repository_sources_with_options,
};
pub use live::{
    LiveIndex, LiveIndexError, LiveIndexMetrics, LiveIndexOptions, start_live_index,
    start_live_index_with_options, start_live_index_with_options_and_commit_provider,
};
