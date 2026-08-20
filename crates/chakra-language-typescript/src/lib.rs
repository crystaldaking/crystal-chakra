//! Git-aware TypeScript/TSX syntax indexing adapter.
//!
//! The adapter extracts syntax-tier TypeScript facts through the official
//! Tree-sitter TypeScript grammar (ADR-0027): the TypeScript grammar for
//! `.ts`/`.mts`/`.cts` sources and the TSX grammar for `.tsx` sources. It has
//! no dependency on an LSP implementation and publishes only language-neutral
//! Chakra graph types.

mod discovery;
mod indexer;
mod parser;

pub use discovery::{
    DiscoveryError, discover_typescript_files, discover_typescript_sources, resolve_repository_root,
};
pub use indexer::{
    IndexMetrics, IndexReport, LanguageBuildMetrics, ReconcileMetrics, ReconcileReport,
    SyntaxFactCounts, TypeScriptIndexError, TypeScriptSources, TypeScriptSyntaxIndex,
    index_repository, scan_repository_sources,
};
