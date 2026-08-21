//! Git-aware Shell syntax indexing adapter.
//!
//! The adapter extracts syntax-tier Shell facts through the official
//! Tree-sitter Bash grammar (ADR-0027) for `.sh`, `.bash`, `.zsh`, and `.ksh`
//! sources. Extraction covers script modules, function and test-function
//! declarations, `source`/`.` and alias facts, byte-accurate ranges, bounded
//! diagnostics, and conservative static function-call candidates. The
//! adapter has no dependency on an LSP implementation and publishes only
//! language-neutral Chakra graph types.

mod discovery;
mod indexer;
mod parser;

pub use discovery::{
    DiscoveryError, discover_shell_files, discover_shell_sources, resolve_repository_root,
};
pub use indexer::{
    IndexMetrics, IndexReport, LanguageBuildMetrics, ReconcileMetrics, ReconcileReport,
    ShellIndexError, ShellSources, ShellSyntaxIndex, SyntaxFactCounts, index_repository,
    scan_repository_sources,
};
