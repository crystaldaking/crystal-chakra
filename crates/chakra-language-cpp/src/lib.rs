//! Git-aware Cpp syntax indexing adapter.
//!
//! The adapter extracts syntax-tier C/C++ facts through the official
//! Tree-sitter C++ grammar (ADR-0027) across translation units and headers.
//! Extraction covers namespaces, classes, structs, unions, enums, templates,
//! aliases, functions, methods, fields, constructors, includes, common C/C++
//! test macros, byte-accurate ranges, diagnostics, and bounded lazy call
//! candidates. The adapter has no dependency on an LSP implementation and
//! publishes only language-neutral Chakra graph types.

mod discovery;
mod indexer;
mod parser;

pub use discovery::{
    DiscoveryError, discover_cpp_files, discover_cpp_sources, resolve_repository_root,
};
pub use indexer::{
    CppIndexError, CppSources, CppSyntaxIndex, IndexMetrics, IndexReport, LanguageBuildMetrics,
    ReconcileMetrics, ReconcileReport, SyntaxFactCounts, index_repository, scan_repository_sources,
};
