//! Git-aware PHP syntax indexing adapter.
//!
//! The adapter extracts syntax-tier PHP facts through the official
//! Tree-sitter PHP grammar. It has no dependency on an LSP implementation
//! and publishes only language-neutral Chakra graph types.

mod indexer;
mod laravel;
mod parser;

pub use indexer::{
    IndexMetrics, IndexReport, LanguageBuildMetrics, PhpIndexError, PhpSources, PhpSyntaxIndex,
    ReconcileMetrics, ReconcileReport, SyntaxFactCounts, detect_laravel, index_repository,
    scan_repository_sources,
};
pub use parser::{
    CallDraft, NamedRelationDraft, ParsedFile, SymbolDraft, TypeRelationDraft, TypeRelationKind,
};
