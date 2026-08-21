//! Git-aware Python syntax indexing adapter.
//!
//! The adapter extracts syntax-tier Python facts through the official
//! Tree-sitter Python grammar (ADR-0027) for `.py`/`.pyi` sources. It has
//! no dependency on an LSP implementation and publishes only
//! language-neutral Chakra graph types.

mod discovery;
mod indexer;
mod parser;

pub use discovery::{
    DiscoveryError, discover_python_files, discover_python_sources, resolve_repository_root,
};
pub use indexer::{
    IndexMetrics, IndexReport, LanguageBuildMetrics, PythonIndexError, PythonSources,
    PythonSyntaxIndex, ReconcileMetrics, ReconcileReport, SyntaxFactCounts, index_repository,
    scan_repository_sources,
};
