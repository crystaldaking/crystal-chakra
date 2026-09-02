//! Object-safe contract between the workspace owner and one syntax language.
//!
//! The workspace owner composes an ordered registry of adapters instead of
//! hardcoding Rust and PHP (ADR-0031). Each adapter owns one language's
//! bounded cold build, incremental reconcile, graph access, and fact/status
//! contribution; the owner routes scanned sources by [`Language`] and merges
//! the per-language graphs in registry order.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io::{self, Cursor, Read, Write};
use std::sync::Arc;

use chakra_domain::indexing::{IndexCancellation, IndexPhaseMeasurement, IndexPublicationMetrics};
use chakra_domain::location::RepoRelativePath;
use chakra_domain::source::SourceMetadata;
use chakra_domain::symbol::Language;
use chakra_engine::{GraphBuildLimits, GraphBuildReport, SymbolGraph};
use serde::Serialize;
use serde::de::DeserializeOwned;

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

impl From<LanguageSources> for chakra_language_index::LanguageSources {
    fn from(sources: LanguageSources) -> Self {
        Self {
            files: sources.files,
            metadata: sources.metadata,
        }
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

impl From<LanguageSources> for chakra_language_csharp::CSharpSources {
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

impl From<chakra_language_index::SyntaxFactCounts> for AdapterFactCounts {
    fn from(facts: chakra_language_index::SyntaxFactCounts) -> Self {
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

/// Framework-enrichment contribution of one adapter build. Zero for adapters
/// without framework enrichment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Retained files whose manifest-derived metadata record was replaced
    /// without a source reparse (issue #40).
    pub metadata_files_recomputed: u64,
    pub framework_files_reparsed: u64,
    pub framework_relationship_files_recomputed: u64,
    pub framework_truncated_files: u64,
    /// Framework-enrichment configuration toggles applied (issue #40).
    pub framework_config_changes: u64,
    pub syntax_error_files: u64,
    pub truncated_call_sites: u64,
    pub publication: IndexPublicationMetrics,
}

/// Typed external-input evidence the workspace owner derived from the scan's
/// manifest/config diff (issue #40). `None` fields mean "no decisive
/// evidence; keep the adapter's current derived state".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DependencyEvidence {
    /// Framework-enrichment opt-in evidenced by the typed project model.
    pub framework_detected: Option<bool>,
}

impl From<chakra_language_index::ReconcileMetrics> for AdapterReconcileMetrics {
    fn from(metrics: chakra_language_index::ReconcileMetrics) -> Self {
        Self {
            scanned_files: metrics.scanned_files,
            unchanged_files: metrics.unchanged_files,
            reparsed_files: metrics.reparsed_files,
            created_files: metrics.created_files,
            modified_files: metrics.modified_files,
            deleted_files: metrics.deleted_files,
            relationship_files_recomputed: metrics.relationship_files_recomputed,
            metadata_files_recomputed: metrics.metadata_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            framework_config_changes: 0,
            syntax_error_files: metrics.syntax_error_files,
            truncated_call_sites: metrics.truncated_call_sites,
            publication: metrics.publication,
        }
    }
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
            metadata_files_recomputed: metrics.metadata_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            framework_config_changes: 0,
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
            metadata_files_recomputed: metrics.metadata_files_recomputed,
            framework_files_reparsed: metrics.framework_files_reparsed,
            framework_relationship_files_recomputed: metrics
                .framework_relationship_files_recomputed,
            framework_truncated_files: metrics.framework_truncated_files,
            framework_config_changes: metrics.framework_config_changes,
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
            metadata_files_recomputed: metrics.metadata_files_recomputed,
            framework_files_reparsed: 0,
            framework_relationship_files_recomputed: 0,
            framework_truncated_files: 0,
            framework_config_changes: 0,
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
    /// Dependency evidence is derived from the same captured source layer;
    /// adapters never read a mutable worktree behind the owner's back.
    fn cold_build(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        dependencies: DependencyEvidence,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError>;

    /// Incremental reconcile of classified sources against the current index.
    /// `dependencies` carries typed manifest/config evidence derived by the
    /// workspace owner (issue #40).
    fn reconcile(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        dependencies: DependencyEvidence,
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

    /// Versioned identity of this adapter's complete persisted state. Bump
    /// this value whenever parsing, relationship resolution, graph layout,
    /// or the encoded adapter type changes incompatibly.
    fn snapshot_version(&self) -> &'static str;

    /// Encodes the complete materialization-independent adapter state,
    /// including its already materialized graph and incremental facts.
    fn encode_snapshot(
        &self,
        cancellation: &IndexCancellation,
    ) -> Result<Vec<u8>, WorkspaceIndexError>;

    /// Restores one complete adapter state written by the exact compatible
    /// [`Self::snapshot_version`]. Callers still validate the resulting
    /// graph and the enclosing compatibility fingerprint.
    fn decode_snapshot(
        &self,
        payload: &[u8],
        cancellation: &IndexCancellation,
    ) -> Result<Box<dyn SyntaxLanguageAdapter>, WorkspaceIndexError>;
}

impl Clone for Box<dyn SyntaxLanguageAdapter> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

const MAX_SNAPSHOT_BYTES: usize = 512 * 1024 * 1024;

struct CancellationWriter<'a> {
    bytes: Vec<u8>,
    cancellation: &'a IndexCancellation,
}

impl Write for CancellationWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if self.bytes.len().saturating_add(buf.len()) > MAX_SNAPSHOT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "snapshot byte bound exceeded",
            ));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct CancellationReader<'a> {
    cursor: Cursor<&'a [u8]>,
    cancellation: &'a IndexCancellation,
}

impl Read for CancellationReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        self.cursor.read(buf)
    }
}

pub(crate) fn encode_snapshot_value<T: Serialize>(
    value: &T,
    cancellation: &IndexCancellation,
) -> Result<Vec<u8>, WorkspaceIndexError> {
    let mut writer = CancellationWriter {
        bytes: Vec::new(),
        cancellation,
    };
    let result = value.serialize(&mut rmp_serde::Serializer::new(&mut writer).with_struct_map());
    if cancellation.is_cancelled() {
        return Err(WorkspaceIndexError::Cancelled);
    }
    result.map_err(|error| WorkspaceIndexError::Snapshot(error.to_string()))?;
    Ok(writer.bytes)
}

pub(crate) fn decode_snapshot_value<T: DeserializeOwned>(
    payload: &[u8],
    cancellation: &IndexCancellation,
) -> Result<T, WorkspaceIndexError> {
    if payload.len() > MAX_SNAPSHOT_BYTES {
        return Err(WorkspaceIndexError::Snapshot(
            "snapshot byte bound exceeded".to_owned(),
        ));
    }
    let reader = CancellationReader {
        cursor: Cursor::new(payload),
        cancellation,
    };
    let mut deserializer = rmp_serde::Deserializer::new(reader);
    let value = T::deserialize(&mut deserializer);
    if cancellation.is_cancelled() {
        return Err(WorkspaceIndexError::Cancelled);
    }
    let reader = deserializer.into_inner();
    let consumed = reader.cursor.position();
    let value = value.map_err(|error| WorkspaceIndexError::Snapshot(error.to_string()))?;
    if consumed != payload.len() as u64 {
        return Err(WorkspaceIndexError::Snapshot(
            "snapshot payload contains trailing bytes".to_owned(),
        ));
    }
    Ok(value)
}

fn decode_adapter_snapshot<T>(
    payload: &[u8],
    cancellation: &IndexCancellation,
) -> Result<Box<dyn SyntaxLanguageAdapter>, WorkspaceIndexError>
where
    T: DeserializeOwned + SyntaxLanguageAdapter + 'static,
{
    let adapter: T = decode_snapshot_value(payload, cancellation)?;
    adapter
        .graph()
        .audit_consistency()
        .map_err(|error| WorkspaceIndexError::Snapshot(error.to_string()))?;
    Ok(Box::new(adapter))
}

macro_rules! snapshot_codec_methods {
    ($adapter:ty, $version:literal) => {
        fn snapshot_version(&self) -> &'static str {
            $version
        }

        fn encode_snapshot(
            &self,
            cancellation: &IndexCancellation,
        ) -> Result<Vec<u8>, WorkspaceIndexError> {
            encode_snapshot_value(self, cancellation)
        }

        fn decode_snapshot(
            &self,
            payload: &[u8],
            cancellation: &IndexCancellation,
        ) -> Result<Box<dyn SyntaxLanguageAdapter>, WorkspaceIndexError> {
            decode_adapter_snapshot::<$adapter>(payload, cancellation)
        }
    };
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
        _dependencies: DependencyEvidence,
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
        _dependencies: DependencyEvidence,
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

    snapshot_codec_methods!(chakra_language_rust::RustSyntaxIndex, "rust:s1");
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
        dependencies: DependencyEvidence,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterColdBuild, WorkspaceIndexError> {
        let laravel_detected = dependencies.framework_detected.unwrap_or(false);
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
        dependencies: DependencyEvidence,
        cancellation: &IndexCancellation,
    ) -> Result<AdapterReconcile, WorkspaceIndexError> {
        let report = self.reconcile_classified_sources_with_evidence(
            sources.into(),
            graph_limits,
            dependencies.framework_detected,
            cancellation,
        )?;
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

    snapshot_codec_methods!(chakra_language_php::PhpSyntaxIndex, "php:s1");
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
        _dependencies: DependencyEvidence,
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
        _dependencies: DependencyEvidence,
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

    snapshot_codec_methods!(
        chakra_language_typescript::TypeScriptSyntaxIndex,
        "typescript:s1"
    );
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
        _dependencies: DependencyEvidence,
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
        _dependencies: DependencyEvidence,
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

    snapshot_codec_methods!(chakra_language_python::PythonSyntaxIndex, "python:s1");
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
        _dependencies: DependencyEvidence,
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
        _dependencies: DependencyEvidence,
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

    snapshot_codec_methods!(
        chakra_language_javascript::JavaScriptSyntaxIndex,
        "javascript:s1"
    );
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
        _dependencies: DependencyEvidence,
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
        _dependencies: DependencyEvidence,
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

    snapshot_codec_methods!(chakra_language_java::JavaSyntaxIndex, "java:s1");
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
        _dependencies: DependencyEvidence,
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
        _dependencies: DependencyEvidence,
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

    snapshot_codec_methods!(chakra_language_csharp::CSharpSyntaxIndex, "csharp:s1");
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
        _dependencies: DependencyEvidence,
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
        _dependencies: DependencyEvidence,
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

    snapshot_codec_methods!(chakra_language_shell::ShellSyntaxIndex, "shell:s1");
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
        _dependencies: DependencyEvidence,
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
        _dependencies: DependencyEvidence,
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

    snapshot_codec_methods!(chakra_language_cpp::CppSyntaxIndex, "cpp:s1");
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
        _dependencies: DependencyEvidence,
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
        _dependencies: DependencyEvidence,
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

    snapshot_codec_methods!(chakra_language_hcl::HclSyntaxIndex, "hcl:s1");
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
        _dependencies: DependencyEvidence,
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
        _dependencies: DependencyEvidence,
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

    snapshot_codec_methods!(chakra_language_go::GoSyntaxIndex, "go:s1");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_registry_matches_the_domain_language_catalog() {
        assert_eq!(registered_languages(), Language::ALL);
    }
}
