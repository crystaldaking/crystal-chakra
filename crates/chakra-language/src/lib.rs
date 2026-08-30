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
    AdapterReconcile, AdapterReconcileMetrics, LanguageSources, SyntaxLanguageAdapter,
    default_adapters, registered_languages,
};
pub use indexer::{
    IndexMetrics, IndexOptions, IndexReport, ReconcileMetrics, ReconcileReport,
    WorkspaceIndexError, WorkspaceLanguageSources, WorkspaceSourceScan, WorkspaceSources,
    WorkspaceSyntaxIndex, index_repository, index_repository_with_options, scan_repository_sources,
    scan_repository_sources_with_options,
};
pub use live::{
    LiveIndex, LiveIndexError, LiveIndexMetrics, LiveIndexOptions, ReconciliationKind,
    start_live_index, start_live_index_with_options,
};
