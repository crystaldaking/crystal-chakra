//! Git-aware JavaScript/JSX syntax indexing adapter.
//!
//! The adapter extracts syntax-tier JavaScript facts through the official
//! Tree-sitter JavaScript grammar (ADR-0027): one grammar covers
//! `.js`/`.mjs`/`.cjs` sources and parses JSX natively for `.jsx` sources.
//! CommonJS `require()`/`module.exports` facts are extracted alongside ES
//! module imports/exports (ADR-0034). The adapter has no dependency on an
//! LSP implementation and publishes only language-neutral Chakra graph types.

mod discovery;
mod indexer;
mod parser;

pub use discovery::{
    DiscoveryError, discover_javascript_files, discover_javascript_sources, resolve_repository_root,
};
pub use indexer::{
    IndexMetrics, IndexReport, JavaScriptIndexError, JavaScriptSources, JavaScriptSyntaxIndex,
    LanguageBuildMetrics, ReconcileMetrics, ReconcileReport, SyntaxFactCounts, index_repository,
    scan_repository_sources,
};
