//! Git-aware Rust syntax indexing adapter.
//!
//! File discovery delegates repository semantics to Git, while syntax facts
//! come from the official Tree-sitter Rust grammar. The crate points inward:
//! it builds Chakra domain/engine types, and neither core crate depends on a
//! language-specific parser.

mod discovery;
mod indexer;
mod parser;

pub use discovery::{
    DiscoveryError, discover_rust_files, discover_rust_sources, resolve_repository_root,
};
pub use indexer::{
    IndexMetrics, IndexReport, LanguageBuildMetrics, ReconcileMetrics, ReconcileReport,
    RustIndexError, RustSources, RustSyntaxIndex, SyntaxFactCounts, index_repository,
    scan_repository_sources,
};
