//! Git-aware Cpp syntax indexing adapter.
//!
//! The adapter extracts syntax-tier Cpp facts through the official
//! Tree-sitter Cpp grammar (ADR-0027): one grammar covers `.cpp` sources.
//! Extraction covers classes, interfaces, enums, records, and annotation
//! types with their methods, fields, and constructors; package and
//! nested-class containers; `import`/`import static`/wildcard import facts;
//! JUnit 4/5 `@Test` hints; byte-accurate ranges; diagnostics; and bounded
//! lazy call candidates. The adapter has no dependency on an LSP
//! implementation and publishes only language-neutral Chakra graph types.

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
