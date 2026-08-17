//! Git-aware PHP syntax indexing adapter.
//!
//! The adapter extracts syntax-tier PHP facts through the official
//! Tree-sitter PHP grammar. It has no dependency on an LSP implementation
//! and publishes only language-neutral Chakra graph types.

mod indexer;
mod parser;

pub use indexer::{
    IndexMetrics, IndexReport, LanguageBuildMetrics, PhpIndexError, PhpSyntaxIndex,
    ReconcileMetrics, ReconcileReport, SyntaxFactCounts, index_repository, scan_repository_sources,
};
