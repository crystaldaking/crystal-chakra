//! Shared indexing metrics and report types.

use std::path::PathBuf;
use std::time::Duration;

use chakra_domain::indexing::IndexPhaseMeasurement;
use chakra_engine::{GraphBuildReport, SymbolGraph};

use crate::driver::LanguageSyntaxIndex;
use crate::hooks::LanguageHooks;

#[derive(Debug, Clone, Copy, Default)]
pub struct IndexMetrics {
    pub discovered_files: u64,
    pub parsed_files: u64,
    pub syntax_error_files: u64,
    pub truncated_call_sites: u64,
    pub symbols: u64,
    pub edges: u64,
    pub call_sites: u64,
    pub ambiguous_call_sites: u64,
    pub unresolved_call_sites: u64,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct ReconcileMetrics {
    pub scanned_files: u64,
    pub unchanged_files: u64,
    pub modified_files: u64,
    pub created_files: u64,
    pub deleted_files: u64,
    pub reparsed_files: u64,
    pub relationship_files_recomputed: u64,
    /// Retained files whose manifest-derived metadata record was replaced
    /// without a source reparse (issue #40).
    pub metadata_files_recomputed: u64,
    pub syntax_error_files: u64,
    pub truncated_call_sites: u64,
    pub publication: chakra_domain::indexing::IndexPublicationMetrics,
}

#[derive(Debug)]
pub struct ReconcileReport<H: LanguageHooks> {
    pub graph: Option<SymbolGraph>,
    pub metrics: ReconcileMetrics,
    pub next_index: Option<LanguageSyntaxIndex<H>>,
    pub build_metrics: Option<LanguageBuildMetrics>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyntaxFactCounts {
    pub files: u64,
    pub source_bytes: u64,
    pub syntax_error_files: u64,
    pub symbols: u64,
    pub relationship_edges: u64,
    pub omitted_relationship_edges: u64,
    pub call_sites: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageBuildMetrics {
    pub facts: SyntaxFactCounts,
    pub graph: GraphBuildReport,
    pub phases: Vec<IndexPhaseMeasurement>,
}

#[derive(Debug)]
pub struct IndexReport<H: LanguageHooks> {
    pub repository_root: PathBuf,
    pub graph: SymbolGraph,
    pub metrics: IndexMetrics,
    pub syntax_index: LanguageSyntaxIndex<H>,
}

/// Observed parser-worker scheduling for one cold build.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseSchedule {
    pub effective_workers: u64,
    pub peak_active_workers: u64,
    pub peak_queue_depth: u64,
}
