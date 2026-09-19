//! Git-aware Kotlin syntax indexing adapter.
//!
//! The adapter extracts syntax-tier Kotlin facts through the
//! tree-sitter-grammars Kotlin grammar (ADR-0056): one grammar covers `.kt`
//! and `.kts` sources. Extraction covers packages, imports and aliases,
//! classes, interfaces, objects, companion objects, enum entries,
//! properties and accessors, constructors, type aliases, extension-function
//! receivers, annotations, inheritance delegation, Kotlin/JUnit test hints,
//! byte-accurate ranges, diagnostics, and bounded lazy call candidates.
//! Type-directed resolution remains the precise provider's responsibility.
//! The adapter has no dependency on an LSP implementation and publishes
//! only language-neutral Chakra graph types.

pub mod calls;
mod discovery;
mod indexer;
mod parser;

pub use discovery::{
    DiscoveryError, discover_kotlin_files, discover_kotlin_sources, resolve_repository_root,
};
pub use indexer::{
    IndexMetrics, IndexReport, KotlinIndexError, KotlinSources, KotlinSyntaxIndex,
    LanguageBuildMetrics, ReconcileMetrics, ReconcileReport, SyntaxFactCounts, index_repository,
    scan_repository_sources,
};
