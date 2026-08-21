//! Git-aware Go syntax indexing adapter.
//!
//! The adapter extracts syntax-tier Go facts through the official
//! Tree-sitter Go grammar (ADR-0027): one grammar covers `.go` sources.
//! Extraction covers packages, imports, build constraints, structs,
//! interfaces, aliases, fields, functions, methods, generics, Go test entry
//! points, byte-accurate ranges, diagnostics, and bounded lazy call
//! candidates. The adapter has no dependency on an LSP implementation and
//! publishes only language-neutral Chakra graph types.

mod discovery;
mod indexer;
mod parser;

pub use discovery::{
    DiscoveryError, discover_go_files, discover_go_sources, resolve_repository_root,
};
pub use indexer::{
    GoIndexError, GoSources, GoSyntaxIndex, IndexMetrics, IndexReport, LanguageBuildMetrics,
    ReconcileMetrics, ReconcileReport, SyntaxFactCounts, index_repository, scan_repository_sources,
};
