//! Multi-language syntax index composition and live workspace ownership.
//!
//! Rust and PHP adapters build independent syntax graphs. This crate combines
//! them into one atomically published workspace revision and owns the single
//! watcher/freshness barrier for that revision.

mod indexer;
mod live;

pub use indexer::{
    IndexMetrics, IndexReport, ReconcileMetrics, ReconcileReport, WorkspaceIndexError,
    WorkspaceSources, WorkspaceSyntaxIndex, index_repository, scan_repository_sources,
};
pub use live::{LiveIndex, LiveIndexError, LiveIndexMetrics, start_live_index};
