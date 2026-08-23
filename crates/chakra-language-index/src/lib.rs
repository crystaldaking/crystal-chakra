//! Shared language-neutral syntax indexing scaffolding (issue #94).
//!
//! One driver owns cold-build and reconcile scheduling, bounded parser
//! workers, metrics, limits, relationship materialization, and graph
//! publication for every language whose per-file facts have the common
//! shape. Language adapters provide typed hooks: a Tree-sitter parser,
//! Git-aware discovery, worker naming, and an optional post-parse evidence
//! pass.
//!
//! Languages with genuinely language-specific index semantics keep their own
//! indexers rather than erasing those semantics behind the shared driver:
//! Rust (impl-block drafts), PHP (receiver-aware call resolution), and C#
//! (extension-method delta machinery).

pub mod driver;
pub mod error;
pub mod facts;
pub mod hooks;
pub mod metrics;

pub use driver::{
    LanguageSources, LanguageSyntaxIndex, MAX_REPOSITORY_SOURCE_BYTES, MAX_SOURCE_FILE_BYTES,
    index_repository, parse_sources_scheduled_observed, scan_repository_sources,
};
pub use error::LanguageIndexError;
pub use facts::{CallDraft, NamedRelationDraft, ParsedFile, SymbolDraft};
pub use hooks::{LanguageHooks, LanguageParser};
pub use metrics::{
    IndexMetrics, IndexReport, LanguageBuildMetrics, ReconcileMetrics, ReconcileReport,
    SyntaxFactCounts,
};
