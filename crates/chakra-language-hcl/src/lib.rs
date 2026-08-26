//! Git-aware HCL syntax indexing adapter.
//!
//! The adapter extracts syntax-tier generic HCL and Terraform/OpenTofu facts
//! through the maintained `tree-sitter-hcl` grammar selected by ADR-0027.
//! It covers modules, variables, outputs, resources, data sources, providers,
//! imports, test runs, byte-accurate ranges, diagnostics, and bounded static
//! reference candidates. The adapter has no dependency on terraform-ls and
//! publishes only language-neutral Chakra graph types.

mod discovery;
mod indexer;
mod json;
mod parser;

pub use discovery::{
    DiscoveryError, discover_hcl_files, discover_hcl_sources, resolve_repository_root,
};
pub use indexer::{
    HclIndexError, HclSources, HclSyntaxIndex, IndexMetrics, IndexReport, LanguageBuildMetrics,
    ReconcileMetrics, ReconcileReport, SyntaxFactCounts, index_repository, scan_repository_sources,
};
