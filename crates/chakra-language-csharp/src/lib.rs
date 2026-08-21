//! Git-aware C# syntax indexing adapter.
//!
//! The adapter extracts syntax-tier C# facts through the official
//! Tree-sitter C# grammar (ADR-0027): one grammar covers `.cs` sources.
//! Extraction covers namespaces; classes, structs, interfaces, enums,
//! records, and delegates; members and constructors; `using` facts; common
//! xUnit/NUnit/MSTest attributes; byte-accurate ranges; diagnostics; heritage;
//! and bounded lazy call candidates. The adapter has no dependency on an LSP
//! implementation and publishes only language-neutral Chakra graph types.

mod discovery;
mod indexer;
mod parser;

pub use discovery::{
    DiscoveryError, discover_csharp_files, discover_csharp_sources, resolve_repository_root,
};
pub use indexer::{
    CSharpIndexError, CSharpSources, CSharpSyntaxIndex, IndexMetrics, IndexReport,
    LanguageBuildMetrics, ReconcileMetrics, ReconcileReport, SyntaxFactCounts, index_repository,
    scan_repository_sources,
};
