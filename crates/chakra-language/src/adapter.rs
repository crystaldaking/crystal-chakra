//! Object-safe contract between the workspace owner and one syntax language.
//!
//! The workspace owner composes an ordered registry of adapters instead of
//! hardcoding Rust and PHP (ADR-0031). Each adapter owns one language's
//! bounded cold build, incremental reconcile, graph access, and fact/status
//! contribution; the owner routes scanned sources by [`Language`] and merges
//! the per-language graphs in registry order.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::Path;
use std::sync::Arc;

use chakra_domain::indexing::{IndexCancellation, IndexPhaseMeasurement, IndexPublicationMetrics};
use chakra_domain::location::RepoRelativePath;
use chakra_domain::source::SourceMetadata;
use chakra_domain::symbol::Language;
use chakra_engine::{GraphBuildLimits, GraphBuildReport, SymbolGraph};
use tracing::warn;

use crate::indexer::WorkspaceIndexError;

/// Latest source text plus role/package metadata for one language, exactly as
/// classified by the shared workspace scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LanguageSources {
    pub files: BTreeMap<RepoRelativePath, Arc<str>>,
    pub metadata: BTreeMap<RepoRelativePath, SourceMetadata>,
}

impl LanguageSources {
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

impl From<LanguageSources> for chakra_language_rust::RustSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
    }
}

impl From<LanguageSources> for chakra_language_php::PhpSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
    }
}

impl From<LanguageSources> for chakra_language_typescript::TypeScriptSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
    }
}

impl From<LanguageSources> for chakra_language_python::PythonSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
    }
}

impl From<LanguageSources> for chakra_language_javascript::JavaScriptSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
    }
}

impl From<LanguageSources> for chakra_language_java::JavaSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
    }
}

impl From<LanguageSources> for chakra_language_csharp::CSharpSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
    }
}

impl From<LanguageSources> for chakra_language_shell::ShellSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
    }
}

impl From<LanguageSources> for chakra_language_cpp::CppSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
    }
}

impl From<LanguageSources> for chakra_language_hcl::HclSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
    }
}

impl From<LanguageSources> for chakra_language_go::GoSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
    }
}

/// Per-language syntax fact counts, identical in shape across adapters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdapterFactCounts {
    pub files: u64,
    pub source_bytes: u64,
    pub syntax_error_files: u64,
    pub symbols: u64,
    pub relationship_edges: u64,
    pub omitted_relationship_edges: u64,
    pub call_sites: u64,
}

impl From<chakra_language_rust::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_rust::SyntaxFactCounts) -> Self {
        Self {
            files: facts.files,
            source_bytes: facts.source_bytes,
            syntax_error_files: facts.syntax_error_files,
            symbols: facts.symbols,
            relationship_edges: facts.relationship_edges,
            omitted_relationship_edges: facts.omitted_relationship_edges,
            call_sites: facts.call_sites,
        }
    }
}

impl From<chakra_language_php::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_php::SyntaxFactCounts) -> Self {
        Self {
            files: facts.files,
            source_bytes: facts.source_bytes,
            syntax_error_files: facts.syntax_error_files,
            symbols: facts.symbols,
            relationship_edges: facts.relationship_edges,
            omitted_relationship_edges: facts.omitted_relationship_edges,
            call_sites: facts.call_sites,
        }
    }
}

impl From<chakra_language_typescript::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_typescript::SyntaxFactCounts) -> Self {
        Self {
            files: facts.files,
            source_bytes: facts.source_bytes,
            syntax_error_files: facts.syntax_error_files,
            symbols: facts.symbols,
            relationship_edges: facts.relationship_edges,
            omitted_relationship_edges: facts.omitted_relationship_edges,
            call_sites: facts.call_sites,
        }
    }
}

impl From<chakra_language_python::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_python::SyntaxFactCounts) -> Self {
        Self {
            files: facts.files,
            source_bytes: facts.source_bytes,
            syntax_error_files: facts.syntax_error_files,
            symbols: facts.symbols,
            relationship_edges: facts.relationship_edges,
            omitted_relationship_edges: facts.omitted_relationship_edges,
            call_sites: facts.call_sites,
        }
    }
}

impl From<chakra_language_javascript::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_javascript::SyntaxFactCounts) -> Self {
        Self {
            files: facts.files,
            source_bytes: facts.source_bytes,
            syntax_error_files: facts.syntax_error_files,
            symbols: facts.symbols,
            relationship_edges: facts.relationship_edges,
            omitted_relationship_edges: facts.omitted_relationship_edges,
            call_sites: facts.call_sites,
        }
    }
}

impl From<chakra_language_java::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_java::SyntaxFactCounts) -> Self {
        Self {
            files: facts.files,
            source_bytes: facts.source_bytes,
            syntax_error_files: facts.syntax_error_files,
            symbols: facts.symbols,
            relationship_edges: facts.relationship_edges,
            omitted_relationship_edges: facts.omitted_relationship_edges,
            call_sites: facts.call_sites,
        }
    }
}

impl From<chakra_language_csharp::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_csharp::SyntaxFactCounts) -> Self {
        Self {
            files: facts.files,
            source_bytes: facts.source_bytes,
            syntax_error_files: facts.syntax_error_files,
            symbols: facts.symbols,
            relationship_edges: facts.relationship_edges,
            omitted_relationship_edges: facts.omitted_relationship_edges,
            call_sites: facts.call_sites,
        }
    }
}

impl From<chakra_language_shell::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_shell::SyntaxFactCounts) -> Self {
        Self {
            files: facts.files,
            source_bytes: facts.source_bytes,
            syntax_error_files: facts.syntax_error_files,
            symbols: facts.symbols,
            relationship_edges: facts.relationship_edges,
            omitted_relationship_edges: facts.omitted_relationship_edges,
            call_sites: facts.call_sites,
        }
    }
}

impl From<chakra_language_cpp::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_cpp::SyntaxFactCounts) -> Self {
        Self {
            files: facts.files,
            source_bytes: facts.source_bytes,
            syntax_error_files: facts.syntax_error_files,
            symbols: facts.symbols,
            relationship_edges: facts.relationship_edges,
            omitted_relationship_edges: facts.omitted_relationship_edges,
            call_sites: facts.call_sites,
        }
    }
}

impl From<chakra_language_hcl::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_hcl::SyntaxFactCounts) -> Self {
        Self {
            files: facts.files,
            source_bytes: facts.source_bytes,
            syntax_error_files: facts.syntax_error_files,
            symbols: facts.symbols,
            relationship_edges: facts.relationship_edges,
            omitted_relationship_edges: facts.omitted_relationship_edges,
            call_sites: facts.call_sites,
        }
    }
}

impl From<chakra_language_go::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_go::SyntaxFactCounts) -> Self {
        Self {
            files: facts.files,
            source_bytes: facts.source_bytes,
            syntax_error_files: facts.syntax_error_files,
            symbols: facts.symbols,
            relationship_edges: facts.relationship_edges,
            omitted_relationship_edges: facts.omitted_relationship_edges,
            call_sites: facts.call_sites,
        }
    }
}

/// Framework-enrichment contribution of one adapter build. Zero for adapters
/// without framework enrichment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdapterFrameworkMetrics {
    pub detected: bool,
    pub symbols: u64,
    pub edges: u64,
    pub truncated_files: u64,
}

/// Build metrics one adapter contributes to the workspace indexing status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterBuildMetrics {
    pub facts: AdapterFactCounts,
    pub graph: GraphBuildReport,
    pub framework: AdapterFrameworkMetrics,
    pub phases: Vec<IndexPhaseMeasurement>,
}

/// Reconcile metrics one adapter contributes. Framework fields are zero for
/// adapters without framework enrichment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdapterReconcileMetrics {
    pub scanned_files: u64,
    pub unchanged_files: u64,
    pub reparsed_files: u64,
    pub created_files: u64,
    pub modified_files: u64,
    pub deleted_files: u64,
    pub relationship_files_recomputed: u64,
    pub framework_files_reparsed: u64,
    pub framework_relationship_files_recomputed: u64,
    pub framework_truncated_files: u64,
    pub syntax_error_files: u64,
    pub truncated_call_sites: u64,
    pub publication: IndexPublicationMetrics,
}

impl From<chakra_language_rust::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_rust::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
}

impl From<chakra_language_php::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_php::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            framework_files_reparsed: metrics.framework_files_reparsed,
            framework_relationship_files_recomputed: metrics
                .framework_relationship_files_recomputed,
            framework_truncated_files: metrics.framework_truncated_files,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
}

impl From<chakra_language_typescript::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_typescript::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
}

impl From<chakra_language_python::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_python::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
}

impl From<chakra_language_javascript::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_javascript::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
}

impl From<chakra_language_java::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_java::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
}

impl From<chakra_language_csharp::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_csharp::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
}

impl From<chakra_language_shell::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_shell::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
}

impl From<chakra_language_cpp::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_cpp::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
}

impl From<chakra_language_hcl::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_hcl::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
}

impl From<chakra_language_go::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_go::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
}

/// Result of one adapter's bounded cold build.
#[derive(Debug)]
pub struct AdapterColdBuild {
    pub index: Box<dyn SyntaxLanguageAdapter>,
    pub graph: SymbolGraph,
    pub metrics: AdapterBuildMetrics,
}

/// Result of one adapter's incremental reconcile.
#[derive(Debug)]
pub struct AdapterReconcile {
    pub graph: Option<SymbolGraph>,
    pub metrics: AdapterReconcileMetrics,
    pub next_index: Option<Box<dyn SyntaxLanguageAdapter>>,
    pub build_metrics: Option<AdapterBuildMetrics>,
}

/// One syntax language's share of the workspace index. Implementations live
/// in the `chakra-language-*` adapter crates; the trait object covers exactly
/// what the workspace owner calls.
pub trait SyntaxLanguageAdapter: Debug + Send + Sync {
    /// Language this adapter indexes.
    fn language(&self) -> Language;

    /// Boxed clone so the owner can keep registry snapshots.
    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter>;

    /// Bounded, worker-scheduled cold build from classified sources.
    /// `repository_root` lets adapters read optional ecosystem metadata
    /// (Composer/Laravel detection) during the build.
    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError>;

    /// Incremental reconcile of classified sources against the current index.
    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError>;

    /// Repository-relative paths currently indexed by this adapter.
    fn paths(&self) -> Vec<RepoRelativePath>;

    /// This adapter's materialized per-language graph.
    fn graph(&self) -> &SymbolGraph;

    /// Build report of the currently materialized graph.
    fn graph_report(&self) -> GraphBuildReport;

    /// Fact counts backing the workspace indexing status.
    fn fact_counts(&self) -> AdapterFactCounts;
}

impl Clone for Box<dyn SyntaxLanguageAdapter> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Fresh registry of every syntax language adapter, in composition order.
/// Graph merge, degradation records, and budget splitting all follow this
/// order, so a new language crate appends its adapter here. TypeScript comes
/// after PHP, Python after TypeScript, JavaScript after Python, and Java
/// after JavaScript, and C# after Java: appending keeps the existing
/// Rust/PHP/TypeScript/Python/JavaScript/Java budget shares, merge order, and
/// entity-id ranges untouched in repositories without the appended
/// languages' sources (ADR-0031).
pub fn default_adapters() -> Vec<Box<dyn SyntaxLanguageAdapter>> {
    vec![
        Box::new(chakra_language_rust::RustSyntaxIndex::default()),
        Box::new(chakra_language_php::PhpSyntaxIndex::default()),
        Box::new(chakra_language_typescript::TypeScriptSyntaxIndex::default()),
        Box::new(chakra_language_python::PythonSyntaxIndex::default()),
        Box::new(chakra_language_javascript::JavaScriptSyntaxIndex::default()),
        Box::new(chakra_language_java::JavaSyntaxIndex::default()),
        Box::new(chakra_language_csharp::CSharpSyntaxIndex::default()),
        Box::new(chakra_language_shell::ShellSyntaxIndex::default()),
        Box::new(chakra_language_cpp::CppSyntaxIndex::default()),
        Box::new(chakra_language_hcl::HclSyntaxIndex::default()),
        Box::new(chakra_language_go::GoSyntaxIndex::default()),
    ]
}

/// Languages covered by the registered adapters, in composition order.
pub fn registered_languages() -> Vec<Language> {
    default_adapters()
        .iter()
        .map(|adapter| adapter.language())
        .collect()
}

impl SyntaxLanguageAdapter for chakra_language_rust::RustSyntaxIndex {
    fn language(&self) -> Language {
        Language::Rust
    }

    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter> {
        Box::new(self.clone())
    }

    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        _repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let (index, graph, metrics) = Self::from_classified_sources_scheduled(
            sources.into(),
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            cancellation,
        )?;
        Ok(AdapterColdBuild {
            index: Box::new(index),
            graph,
            metrics: AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            },
        })
    }

    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report =
            self.reconcile_classified_sources_bounded(sources.into(), graph_limits, cancellation)?;
        Ok(AdapterReconcile {
            graph: report.graph,
            metrics: report.metrics.into(),
            next_index: report
                .next_index
                .map(|index| Box::new(index) as Box<dyn SyntaxLanguageAdapter>),
            build_metrics: report.build_metrics.map(|metrics| AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            }),
        })
    }

    fn paths(&self) -> Vec<RepoRelativePath> {
        self.paths()
    }

    fn graph(&self) -> &SymbolGraph {
        self.graph()
    }

    fn graph_report(&self) -> GraphBuildReport {
        self.graph_report()
    }

    fn fact_counts(&self) -> AdapterFactCounts {
        self.fact_counts().into()
    }
}

impl SyntaxLanguageAdapter for chakra_language_php::PhpSyntaxIndex {
    fn language(&self) -> Language {
        Language::Php
    }

    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter> {
        Box::new(self.clone())
    }

    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let laravel_detected = match chakra_language_php::detect_laravel(repository_root) {
            Ok(detected) => detected,
            Err(error) => {
                warn!(
                    error = %error,
                    "Laravel enrichment disabled because Composer metadata is unavailable or invalid"
                );
                false
            }
        };
        let (index, graph, metrics) = Self::from_classified_sources_scheduled(
            sources.into(),
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            laravel_detected,
            cancellation,
        )?;
        Ok(AdapterColdBuild {
            index: Box::new(index),
            graph,
            metrics: AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics {
                    detected: metrics.laravel_detected,
                    symbols: metrics.framework_symbols,
                    edges: metrics.framework_edges,
                    truncated_files: metrics.framework_truncated_files,
                },
                phases: metrics.phases,
            },
        })
    }

    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report =
            self.reconcile_classified_sources_bounded(sources.into(), graph_limits, cancellation)?;
        Ok(AdapterReconcile {
            graph: report.graph,
            metrics: report.metrics.into(),
            next_index: report
                .next_index
                .map(|index| Box::new(index) as Box<dyn SyntaxLanguageAdapter>),
            build_metrics: report.build_metrics.map(|metrics| AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics {
                    detected: metrics.laravel_detected,
                    symbols: metrics.framework_symbols,
                    edges: metrics.framework_edges,
                    truncated_files: metrics.framework_truncated_files,
                },
                phases: metrics.phases,
            }),
        })
    }

    fn paths(&self) -> Vec<RepoRelativePath> {
        self.paths()
    }

    fn graph(&self) -> &SymbolGraph {
        self.graph()
    }

    fn graph_report(&self) -> GraphBuildReport {
        self.graph_report()
    }

    fn fact_counts(&self) -> AdapterFactCounts {
        self.fact_counts().into()
    }
}

impl SyntaxLanguageAdapter for chakra_language_typescript::TypeScriptSyntaxIndex {
    fn language(&self) -> Language {
        Language::TypeScript
    }

    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter> {
        Box::new(self.clone())
    }

    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        _repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let (index, graph, metrics) = Self::from_classified_sources_scheduled(
            sources.into(),
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            cancellation,
        )?;
        Ok(AdapterColdBuild {
            index: Box::new(index),
            graph,
            metrics: AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            },
        })
    }

    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report =
            self.reconcile_classified_sources_bounded(sources.into(), graph_limits, cancellation)?;
        Ok(AdapterReconcile {
            graph: report.graph,
            metrics: report.metrics.into(),
            next_index: report
                .next_index
                .map(|index| Box::new(index) as Box<dyn SyntaxLanguageAdapter>),
            build_metrics: report.build_metrics.map(|metrics| AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            }),
        })
    }

    fn paths(&self) -> Vec<RepoRelativePath> {
        self.paths()
    }

    fn graph(&self) -> &SymbolGraph {
        self.graph()
    }

    fn graph_report(&self) -> GraphBuildReport {
        self.graph_report()
    }

    fn fact_counts(&self) -> AdapterFactCounts {
        self.fact_counts().into()
    }
}

impl SyntaxLanguageAdapter for chakra_language_python::PythonSyntaxIndex {
    fn language(&self) -> Language {
        Language::Python
    }

    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter> {
        Box::new(self.clone())
    }

    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        _repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let (index, graph, metrics) = Self::from_classified_sources_scheduled(
            sources.into(),
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            cancellation,
        )?;
        Ok(AdapterColdBuild {
            index: Box::new(index),
            graph,
            metrics: AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            },
        })
    }

    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report =
            self.reconcile_classified_sources_bounded(sources.into(), graph_limits, cancellation)?;
        Ok(AdapterReconcile {
            graph: report.graph,
            metrics: report.metrics.into(),
            next_index: report
                .next_index
                .map(|index| Box::new(index) as Box<dyn SyntaxLanguageAdapter>),
            build_metrics: report.build_metrics.map(|metrics| AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            }),
        })
    }

    fn paths(&self) -> Vec<RepoRelativePath> {
        self.paths()
    }

    fn graph(&self) -> &SymbolGraph {
        self.graph()
    }

    fn graph_report(&self) -> GraphBuildReport {
        self.graph_report()
    }

    fn fact_counts(&self) -> AdapterFactCounts {
        self.fact_counts().into()
    }
}

impl SyntaxLanguageAdapter for chakra_language_javascript::JavaScriptSyntaxIndex {
    fn language(&self) -> Language {
        Language::JavaScript
    }

    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter> {
        Box::new(self.clone())
    }

    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        _repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let (index, graph, metrics) = Self::from_classified_sources_scheduled(
            sources.into(),
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            cancellation,
        )?;
        Ok(AdapterColdBuild {
            index: Box::new(index),
            graph,
            metrics: AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            },
        })
    }

    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report =
            self.reconcile_classified_sources_bounded(sources.into(), graph_limits, cancellation)?;
        Ok(AdapterReconcile {
            graph: report.graph,
            metrics: report.metrics.into(),
            next_index: report
                .next_index
                .map(|index| Box::new(index) as Box<dyn SyntaxLanguageAdapter>),
            build_metrics: report.build_metrics.map(|metrics| AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            }),
        })
    }

    fn paths(&self) -> Vec<RepoRelativePath> {
        self.paths()
    }

    fn graph(&self) -> &SymbolGraph {
        self.graph()
    }

    fn graph_report(&self) -> GraphBuildReport {
        self.graph_report()
    }

    fn fact_counts(&self) -> AdapterFactCounts {
        self.fact_counts().into()
    }
}

impl SyntaxLanguageAdapter for chakra_language_java::JavaSyntaxIndex {
    fn language(&self) -> Language {
        Language::Java
    }

    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter> {
        Box::new(self.clone())
    }

    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        _repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let (index, graph, metrics) = Self::from_classified_sources_scheduled(
            sources.into(),
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            cancellation,
        )?;
        Ok(AdapterColdBuild {
            index: Box::new(index),
            graph,
            metrics: AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            },
        })
    }

    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report =
            self.reconcile_classified_sources_bounded(sources.into(), graph_limits, cancellation)?;
        Ok(AdapterReconcile {
            graph: report.graph,
            metrics: report.metrics.into(),
            next_index: report
                .next_index
                .map(|index| Box::new(index) as Box<dyn SyntaxLanguageAdapter>),
            build_metrics: report.build_metrics.map(|metrics| AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            }),
        })
    }

    fn paths(&self) -> Vec<RepoRelativePath> {
        self.paths()
    }

    fn graph(&self) -> &SymbolGraph {
        self.graph()
    }

    fn graph_report(&self) -> GraphBuildReport {
        self.graph_report()
    }

    fn fact_counts(&self) -> AdapterFactCounts {
        self.fact_counts().into()
    }
}

impl SyntaxLanguageAdapter for chakra_language_csharp::CSharpSyntaxIndex {
    fn language(&self) -> Language {
        Language::CSharp
    }

    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter> {
        Box::new(self.clone())
    }

    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        _repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let (index, graph, metrics) = Self::from_classified_sources_scheduled(
            sources.into(),
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            cancellation,
        )?;
        Ok(AdapterColdBuild {
            index: Box::new(index),
            graph,
            metrics: AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            },
        })
    }

    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report =
            self.reconcile_classified_sources_bounded(sources.into(), graph_limits, cancellation)?;
        Ok(AdapterReconcile {
            graph: report.graph,
            metrics: report.metrics.into(),
            next_index: report
                .next_index
                .map(|index| Box::new(index) as Box<dyn SyntaxLanguageAdapter>),
            build_metrics: report.build_metrics.map(|metrics| AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            }),
        })
    }

    fn paths(&self) -> Vec<RepoRelativePath> {
        self.paths()
    }

    fn graph(&self) -> &SymbolGraph {
        self.graph()
    }

    fn graph_report(&self) -> GraphBuildReport {
        self.graph_report()
    }

    fn fact_counts(&self) -> AdapterFactCounts {
        self.fact_counts().into()
    }
}

impl SyntaxLanguageAdapter for chakra_language_shell::ShellSyntaxIndex {
    fn language(&self) -> Language {
        Language::Shell
    }

    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter> {
        Box::new(self.clone())
    }

    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        _repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let (index, graph, metrics) = Self::from_classified_sources_scheduled(
            sources.into(),
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            cancellation,
        )?;
        Ok(AdapterColdBuild {
            index: Box::new(index),
            graph,
            metrics: AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            },
        })
    }

    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report =
            self.reconcile_classified_sources_bounded(sources.into(), graph_limits, cancellation)?;
        Ok(AdapterReconcile {
            graph: report.graph,
            metrics: report.metrics.into(),
            next_index: report
                .next_index
                .map(|index| Box::new(index) as Box<dyn SyntaxLanguageAdapter>),
            build_metrics: report.build_metrics.map(|metrics| AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            }),
        })
    }

    fn paths(&self) -> Vec<RepoRelativePath> {
        self.paths()
    }

    fn graph(&self) -> &SymbolGraph {
        self.graph()
    }

    fn graph_report(&self) -> GraphBuildReport {
        self.graph_report()
    }

    fn fact_counts(&self) -> AdapterFactCounts {
        self.fact_counts().into()
    }
}

impl SyntaxLanguageAdapter for chakra_language_cpp::CppSyntaxIndex {
    fn language(&self) -> Language {
        Language::Cpp
    }

    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter> {
        Box::new(self.clone())
    }

    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        _repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let (index, graph, metrics) = Self::from_classified_sources_scheduled(
            sources.into(),
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            cancellation,
        )?;
        Ok(AdapterColdBuild {
            index: Box::new(index),
            graph,
            metrics: AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            },
        })
    }

    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report =
            self.reconcile_classified_sources_bounded(sources.into(), graph_limits, cancellation)?;
        Ok(AdapterReconcile {
            graph: report.graph,
            metrics: report.metrics.into(),
            next_index: report
                .next_index
                .map(|index| Box::new(index) as Box<dyn SyntaxLanguageAdapter>),
            build_metrics: report.build_metrics.map(|metrics| AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            }),
        })
    }

    fn paths(&self) -> Vec<RepoRelativePath> {
        self.paths()
    }

    fn graph(&self) -> &SymbolGraph {
        self.graph()
    }

    fn graph_report(&self) -> GraphBuildReport {
        self.graph_report()
    }

    fn fact_counts(&self) -> AdapterFactCounts {
        self.fact_counts().into()
    }
}

impl SyntaxLanguageAdapter for chakra_language_hcl::HclSyntaxIndex {
    fn language(&self) -> Language {
        Language::Hcl
    }

    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter> {
        Box::new(self.clone())
    }

    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        _repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let (index, graph, metrics) = Self::from_classified_sources_scheduled(
            sources.into(),
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            cancellation,
        )?;
        Ok(AdapterColdBuild {
            index: Box::new(index),
            graph,
            metrics: AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            },
        })
    }

    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report =
            self.reconcile_classified_sources_bounded(sources.into(), graph_limits, cancellation)?;
        Ok(AdapterReconcile {
            graph: report.graph,
            metrics: report.metrics.into(),
            next_index: report
                .next_index
                .map(|index| Box::new(index) as Box<dyn SyntaxLanguageAdapter>),
            build_metrics: report.build_metrics.map(|metrics| AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            }),
        })
    }

    fn paths(&self) -> Vec<RepoRelativePath> {
        self.paths()
    }

    fn graph(&self) -> &SymbolGraph {
        self.graph()
    }

    fn graph_report(&self) -> GraphBuildReport {
        self.graph_report()
    }

    fn fact_counts(&self) -> AdapterFactCounts {
        self.fact_counts().into()
    }
}

impl SyntaxLanguageAdapter for chakra_language_go::GoSyntaxIndex {
    fn language(&self) -> Language {
        Language::Go
    }

    fn clone_box(&self) -> Box<dyn SyntaxLanguageAdapter> {
        Box::new(self.clone())
    }

    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        _repository_root: &Path,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let (index, graph, metrics) = Self::from_classified_sources_scheduled(
            sources.into(),
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            cancellation,
        )?;
        Ok(AdapterColdBuild {
            index: Box::new(index),
            graph,
            metrics: AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            },
        })
    }

    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report =
            self.reconcile_classified_sources_bounded(sources.into(), graph_limits, cancellation)?;
        Ok(AdapterReconcile {
            graph: report.graph,
            metrics: report.metrics.into(),
            next_index: report
                .next_index
                .map(|index| Box::new(index) as Box<dyn SyntaxLanguageAdapter>),
            build_metrics: report.build_metrics.map(|metrics| AdapterBuildMetrics {
                facts: metrics.facts.into(),
                graph: metrics.graph,
                framework: AdapterFrameworkMetrics::default(),
                phases: metrics.phases,
            }),
        })
    }

    fn paths(&self) -> Vec<RepoRelativePath> {
        self.paths()
    }

    fn graph(&self) -> &SymbolGraph {
        self.graph()
    }

    fn graph_report(&self) -> GraphBuildReport {
        self.graph_report()
    }

    fn fact_counts(&self) -> AdapterFactCounts {
        self.fact_counts().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_registry_matches_the_domain_language_catalog() {
        assert_eq!(registered_languages(), Language::ALL);
    }
}
