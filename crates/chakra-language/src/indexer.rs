//! Bounded composition of independently parsed language indexes.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::indexing::{
    IndexBudgetError, IndexBudgetKind, IndexBudgets, IndexCancellation, IndexCapability,
    IndexCapabilityCoverage, IndexCoverage, IndexDegradation, IndexMemoryMetrics, IndexPhase,
    IndexPhaseMeasurement, IndexPublicationMetrics, IndexSchedulingMetrics, IndexingStatus,
};
use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;
use chakra_domain::symbol::Language;
use chakra_engine::{
    ConsistencyError, GraphBuildLimits, GraphBuildReport, GraphError, ProviderInput, SymbolGraph,
};
use thiserror::Error;
use tracing::{info, warn};

#[cfg(unix)]
use nix::sys::resource::{UsageWho, getrusage};
#[cfg(unix)]
use nix::sys::time::TimeValLike;

use crate::adapter::{
    AdapterColdBuild, AdapterFactCounts, AdapterFrameworkMetrics, AdapterReconcileMetrics,
    LanguageSources, SyntaxLanguageAdapter, default_adapters, registered_languages,
};
use crate::cache::{
    CacheRestoreOutcome, CacheStore, CompatibilityKey, FactsToStore, ManifestEntry,
    SyntaxCacheMode, SyntaxCacheReport, content_hash,
};

const PARALLEL_PARSE_FILE_THRESHOLD: u64 = 32;
const INDEX_WORKER_MEMORY_RESERVE_BYTES: u64 = 64 * 1024 * 1024;

/// Classified sources of every registered adapter language, in composition
/// (registry) order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSources {
    languages: Vec<WorkspaceLanguageSources>,
}

/// Classified sources of one adapter language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLanguageSources {
    pub language: Language,
    pub sources: LanguageSources,
}

impl WorkspaceSources {
    pub fn languages(&self) -> &[WorkspaceLanguageSources] {
        &self.languages
    }

    pub fn get(&self, language: Language) -> Option<&LanguageSources> {
        self.languages
            .iter()
            .find(|entry| entry.language == language)
            .map(|entry| &entry.sources)
    }

    pub fn file_count(&self, language: Language) -> usize {
        self.get(language).map_or(0, LanguageSources::len)
    }

    fn take(&mut self, language: Language) -> LanguageSources {
        self.languages
            .iter_mut()
            .find(|entry| entry.language == language)
            .map(|entry| std::mem::take(&mut entry.sources))
            .unwrap_or_default()
    }

    fn counts(&self) -> Vec<u64> {
        self.languages
            .iter()
            .map(|entry| entry.sources.len() as u64)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSourceScan {
    pub sources: WorkspaceSources,
    /// Non-source manifests/configuration captured with this exact scan for
    /// revision-scoped provider watched-file synchronization.
    pub provider_inputs: Vec<ProviderInput>,
    pub discovered_files: u64,
    pub indexed_files: u64,
    pub source_bytes: u64,
    /// Files skipped because they could not be read or decoded as UTF-8
    /// (including files that vanished between inventory and read).
    pub unreadable_files: u64,
    /// Repository-relative paths of the skipped unreadable files.
    pub unreadable_paths: Vec<RepoRelativePath>,
    pub degradations: Vec<IndexDegradation>,
    pub phases: Vec<IndexPhaseMeasurement>,
}

#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    pub budgets: IndexBudgets,
    pub cancellation: IndexCancellation,
    /// Opt-in per-file syntax fact cache (issue #39). Disabled by default;
    /// below the configured size gate the cache is neither read nor written.
    pub cache: SyntaxCacheMode,
}

#[derive(Debug, Clone, Copy)]
struct WorkerPolicy {
    configured_max_workers: u64,
    available_parallelism: u64,
    source_memory_reserve_bytes: u64,
    worker_memory_reserve_bytes: u64,
    memory_limited_workers: u64,
    effective_worker_limit: u64,
}

impl WorkerPolicy {
    fn from_budgets(budgets: IndexBudgets) -> Self {
        let available_parallelism = std::thread::available_parallelism()
            .map(|value| value.get() as u64)
            .unwrap_or(1);
        let source_memory_reserve_bytes = budgets.max_workspace_source_bytes.min(
            budgets
                .memory_target_bytes
                .saturating_sub(INDEX_WORKER_MEMORY_RESERVE_BYTES),
        );
        let schedulable_memory = budgets
            .memory_target_bytes
            .saturating_sub(source_memory_reserve_bytes);
        let memory_limited_workers =
            (schedulable_memory / INDEX_WORKER_MEMORY_RESERVE_BYTES).max(1);
        let effective_worker_limit = budgets
            .max_workers
            .min(available_parallelism)
            .min(memory_limited_workers)
            .max(1);
        Self {
            configured_max_workers: budgets.max_workers,
            available_parallelism,
            source_memory_reserve_bytes,
            worker_memory_reserve_bytes: INDEX_WORKER_MEMORY_RESERVE_BYTES,
            memory_limited_workers,
            effective_worker_limit,
        }
    }

    fn scheduling(self, phases: &[IndexPhaseMeasurement]) -> IndexSchedulingMetrics {
        let peak_active_workers = phases
            .iter()
            .map(|phase| phase.peak_active_workers)
            .max()
            .unwrap_or(0);
        let peak_queue_depth = phases
            .iter()
            .map(|phase| phase.peak_queue_depth)
            .max()
            .unwrap_or(0);
        let mut parallel_parse_files = 0_u64;
        let mut sequential_parse_files = 0_u64;
        for phase in phases
            .iter()
            .filter(|phase| phase.phase == IndexPhase::ParseExtraction)
        {
            if phase.effective_workers > 1 {
                parallel_parse_files = parallel_parse_files.saturating_add(phase.work_items);
            } else {
                sequential_parse_files = sequential_parse_files.saturating_add(phase.work_items);
            }
        }
        IndexSchedulingMetrics {
            configured_max_workers: self.configured_max_workers,
            available_parallelism: self.available_parallelism,
            source_memory_reserve_bytes: self.source_memory_reserve_bytes,
            worker_memory_reserve_bytes: self.worker_memory_reserve_bytes,
            memory_limited_workers: self.memory_limited_workers,
            effective_worker_limit: self.effective_worker_limit,
            peak_active_workers,
            peak_queue_depth,
            parallel_parse_files,
            sequential_parse_files,
            parallel_parse_file_threshold: PARALLEL_PARSE_FILE_THRESHOLD,
            low_resource_mode: self.effective_worker_limit == 1,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PhaseTimer {
    wall: Instant,
    cpu_micros: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct PhaseConcurrency {
    effective_workers: u64,
    peak_active_workers: u64,
    peak_queue_depth: u64,
}

impl PhaseConcurrency {
    const SERIAL: Self = Self {
        effective_workers: 1,
        peak_active_workers: 1,
        peak_queue_depth: 0,
    };
}

impl PhaseTimer {
    fn start() -> Self {
        Self {
            wall: Instant::now(),
            cpu_micros: process_cpu_micros(),
        }
    }
}

impl IndexOptions {
    pub fn new(
        budgets: IndexBudgets,
        cancellation: IndexCancellation,
    ) -> Result<Self, IndexBudgetError> {
        Ok(Self {
            budgets: budgets.validate()?,
            cancellation,
            cache: SyntaxCacheMode::default(),
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileMetrics {
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

#[derive(Debug)]
pub struct ReconcileReport {
    pub graph: Option<SymbolGraph>,
    pub metrics: ReconcileMetrics,
    pub next_index: Option<WorkspaceSyntaxIndex>,
    pub indexing: IndexingStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub rust_files: u64,
    pub php_files: u64,
    pub typescript_files: u64,
    pub python_files: u64,
    pub javascript_files: u64,
    pub java_files: u64,
    pub csharp_files: u64,
    pub shell_files: u64,
    pub cpp_files: u64,
    pub hcl_files: u64,
    pub go_files: u64,
    pub laravel_detected: bool,
    pub framework_symbols: u64,
    pub framework_edges: u64,
    pub framework_truncated_files: u64,
    pub elapsed: Duration,
    pub indexing: IndexingStatus,
}

#[derive(Debug)]
pub struct IndexReport {
    pub repository_root: PathBuf,
    pub graph: SymbolGraph,
    pub metrics: IndexMetrics,
    pub syntax_index: WorkspaceSyntaxIndex,
    pub provider_inputs: Vec<ProviderInput>,
    /// Syntax fact cache participation of this run (issue #39).
    pub cache: SyntaxCacheReport,
}

#[derive(Debug, Error)]
pub enum WorkspaceIndexError {
    #[error(transparent)]
    Discovery(#[from] chakra_git::DiscoveryError),
    #[error("failed to read source {path}: {source}")]
    Read {
        path: RepoRelativePath,
        #[source]
        source: io::Error,
    },
    #[error("{language:?} syntax adapter failed: {source}")]
    Adapter {
        language: Language,
        #[source]
        source: Box<WorkspaceIndexError>,
    },
    #[error(transparent)]
    Rust(#[from] chakra_language_rust::RustIndexError),
    #[error(transparent)]
    Php(#[from] chakra_language_php::PhpIndexError),
    #[error(transparent)]
    CSharp(#[from] chakra_language_csharp::CSharpIndexError),
    /// TypeScript, Python, JavaScript, Java, Shell, C++, HCL, and Go adapters
    /// share one indexing driver and one error type (issue #94).
    #[error(transparent)]
    Shared(#[from] chakra_language_index::LanguageIndexError),
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error("constructed workspace syntax graph is inconsistent: {0}")]
    Consistency(#[from] ConsistencyError),
    #[error(transparent)]
    InvalidBudget(#[from] IndexBudgetError),
    #[error("workspace syntax indexing was cancelled")]
    Cancelled,
    #[error("workspace syntax index update failed: {0}")]
    Update(String),
}

#[derive(Debug, Clone)]
struct WorkspaceAdapterState {
    adapter: Box<dyn SyntaxLanguageAdapter>,
    limits: GraphBuildLimits,
}

#[derive(Debug, Clone)]
pub struct WorkspaceSyntaxIndex {
    adapters: Vec<WorkspaceAdapterState>,
    budgets: IndexBudgets,
    indexing: IndexingStatus,
    provider_inputs: Vec<ProviderInput>,
}

impl WorkspaceSyntaxIndex {
    pub fn paths(&self) -> Vec<RepoRelativePath> {
        let mut paths = Vec::new();
        for state in &self.adapters {
            paths.extend(state.adapter.paths());
        }
        paths.sort();
        paths.dedup();
        paths
    }

    pub fn budgets(&self) -> IndexBudgets {
        self.budgets
    }

    pub fn indexing(&self) -> &IndexingStatus {
        &self.indexing
    }

    pub fn provider_inputs(&self) -> &[ProviderInput] {
        &self.provider_inputs
    }

    pub fn scan_repository(
        &self,
        repository_root: &Path,
    ) -> Result<WorkspaceSourceScan, WorkspaceIndexError> {
        scan_repository_sources_with_options(
            repository_root,
            &IndexOptions::new(self.budgets, IndexCancellation::default())?,
        )
    }

    pub fn reconcile_sources(
        &self,
        scan: WorkspaceSourceScan,
    ) -> Result<ReconcileReport, WorkspaceIndexError> {
        self.reconcile_sources_with_cancellation(scan, &IndexCancellation::default())
    }

    pub fn reconcile_sources_with_cancellation(
        &self,
        mut scan: WorkspaceSourceScan,
        cancellation: &IndexCancellation,
    ) -> Result<ReconcileReport, WorkspaceIndexError> {
        let started = PhaseTimer::start();
        check_cancelled(cancellation)?;
        let limits = self.live_graph_limits(&scan.sources);
        let mut reconciled = Vec::with_capacity(self.adapters.len());
        for (state, limits) in self.adapters.iter().zip(limits.iter().copied()) {
            let language = state.adapter.language();
            let sources = scan.sources.take(language);
            reconciled.push(
                state
                    .adapter
                    .reconcile(sources, limits, cancellation)
                    .map_err(|source| WorkspaceIndexError::Adapter {
                        language,
                        source: Box::new(source),
                    })?,
            );
            check_cancelled(cancellation)?;
        }
        let mut metrics = ReconcileMetrics::default();
        metrics.publication.structurally_incremental = true;
        for report in &reconciled {
            accumulate_reconcile_metrics(&mut metrics, report.metrics);
        }

        let graph_builds: Vec<GraphBuildReport> = self
            .adapters
            .iter()
            .zip(reconciled.iter())
            .map(|(state, report)| {
                report
                    .build_metrics
                    .as_ref()
                    .map_or_else(|| state.adapter.graph_report(), |metrics| metrics.graph)
            })
            .collect();
        metrics.truncated_call_sites = graph_builds.iter().fold(0_u64, |total, build| {
            total.saturating_add(build.omitted_call_sites)
        });
        let graph_changed = reconciled.iter().any(|report| report.next_index.is_some());
        let next_adapters: Vec<Box<dyn SyntaxLanguageAdapter>> = self
            .adapters
            .iter()
            .zip(reconciled.iter_mut())
            .map(|(state, report)| {
                report
                    .next_index
                    .take()
                    .unwrap_or_else(|| state.adapter.clone_box())
            })
            .collect();

        let (graph, composition_phase) = if graph_changed {
            check_cancelled(cancellation)?;
            let composition_started = PhaseTimer::start();
            let graph =
                SymbolGraph::merge(next_adapters.iter().map(|adapter| adapter.graph().clone()))?;
            let composition_phase = measured_phase(
                IndexPhase::LanguageComposition,
                None,
                composition_started,
                next_adapters.len() as u64,
                0,
                PhaseConcurrency::SERIAL,
            );
            (Some(graph), Some(composition_phase))
        } else {
            (None, None)
        };

        let mut phases = if graph_changed {
            scan.phases.clone()
        } else {
            self.indexing.phases.clone()
        };
        for report in &reconciled {
            if let Some(build) = report.build_metrics.as_ref() {
                phases.extend(build.phases.clone());
            }
        }
        let mut language_facts = Vec::with_capacity(self.adapters.len());
        for ((adapter, limits), build) in next_adapters
            .iter()
            .zip(limits.iter().copied())
            .zip(graph_builds.iter().copied())
        {
            language_facts.push(LanguageIndexingFacts {
                language: adapter.language(),
                facts: adapter.fact_counts(),
                graph: build,
                limits,
            });
        }
        phases.extend(composition_phase);
        if graph_changed {
            phases.push(measured_phase(
                IndexPhase::LiveReconciliation,
                None,
                started,
                metrics
                    .scanned_files
                    .saturating_add(metrics.reparsed_files)
                    .saturating_add(metrics.relationship_files_recomputed),
                scan.source_bytes,
                PhaseConcurrency::SERIAL,
            ));
        }

        let current_rss = process_rss_bytes();
        let indexing = build_indexing_status(
            self.budgets,
            &scan,
            IndexingParts {
                languages: language_facts,
                phases,
                memory: IndexMemoryMetrics {
                    current_rss_bytes: current_rss,
                    observed_phase_peak_rss_bytes: max_option(
                        self.indexing.memory.observed_phase_peak_rss_bytes,
                        current_rss,
                    ),
                    ..IndexMemoryMetrics::default()
                },
                publication: metrics.publication,
            },
        );

        let indexing_changed = !indexing_semantically_equal(&self.indexing, &indexing);
        let provider_inputs_changed = self.provider_inputs != scan.provider_inputs;
        if !graph_changed && !indexing_changed && !provider_inputs_changed {
            return Ok(ReconcileReport {
                graph: None,
                metrics,
                next_index: None,
                indexing: self.indexing.clone(),
            });
        }

        let next = Self {
            adapters: next_adapters
                .into_iter()
                .zip(limits.iter().copied())
                .map(|(adapter, limits)| WorkspaceAdapterState { adapter, limits })
                .collect(),
            budgets: self.budgets,
            indexing: indexing.clone(),
            provider_inputs: scan.provider_inputs,
        };
        Ok(ReconcileReport {
            graph,
            metrics,
            next_index: Some(next),
            indexing,
        })
    }

    fn live_graph_limits(&self, sources: &WorkspaceSources) -> Vec<GraphBuildLimits> {
        let any_activated = self.adapters.iter().any(|state| {
            state.adapter.fact_counts().files == 0
                && sources
                    .get(state.adapter.language())
                    .is_some_and(|sources| !sources.is_empty())
        });
        if any_activated {
            split_graph_limits(self.budgets, &sources.counts())
        } else {
            self.adapters.iter().map(|state| state.limits).collect()
        }
    }
}

fn accumulate_reconcile_metrics(combined: &mut ReconcileMetrics, metrics: AdapterReconcileMetrics) {
    combined.scanned_files += metrics.scanned_files;
    combined.unchanged_files += metrics.unchanged_files;
    combined.reparsed_files += metrics.reparsed_files;
    combined.created_files += metrics.created_files;
    combined.modified_files += metrics.modified_files;
    combined.deleted_files += metrics.deleted_files;
    combined.relationship_files_recomputed += metrics.relationship_files_recomputed;
    combined.framework_files_reparsed += metrics.framework_files_reparsed;
    combined.framework_relationship_files_recomputed +=
        metrics.framework_relationship_files_recomputed;
    combined.framework_truncated_files += metrics.framework_truncated_files;
    combined.syntax_error_files += metrics.syntax_error_files;
    combined.truncated_call_sites += metrics.truncated_call_sites;
    accumulate_publication(&mut combined.publication, metrics.publication);
}

fn accumulate_publication(
    combined: &mut IndexPublicationMetrics,
    publication: IndexPublicationMetrics,
) {
    combined.structurally_incremental =
        combined.structurally_incremental && publication.structurally_incremental;
    combined.reused_files = combined
        .reused_files
        .saturating_add(publication.reused_files);
    combined.rebuilt_files = combined
        .rebuilt_files
        .saturating_add(publication.rebuilt_files);
    combined.reused_source_bytes = combined
        .reused_source_bytes
        .saturating_add(publication.reused_source_bytes);
    combined.rebuilt_source_bytes = combined
        .rebuilt_source_bytes
        .saturating_add(publication.rebuilt_source_bytes);
    combined.copied_source_bytes = combined
        .copied_source_bytes
        .saturating_add(publication.copied_source_bytes);
    combined.reused_symbols = combined
        .reused_symbols
        .saturating_add(publication.reused_symbols);
    combined.rebuilt_symbols = combined
        .rebuilt_symbols
        .saturating_add(publication.rebuilt_symbols);
    combined.copied_symbols = combined
        .copied_symbols
        .saturating_add(publication.copied_symbols);
    combined.reused_edges = combined
        .reused_edges
        .saturating_add(publication.reused_edges);
    combined.rebuilt_edges = combined
        .rebuilt_edges
        .saturating_add(publication.rebuilt_edges);
    combined.copied_edges = combined
        .copied_edges
        .saturating_add(publication.copied_edges);
    combined.reused_call_sites = combined
        .reused_call_sites
        .saturating_add(publication.reused_call_sites);
    combined.rebuilt_call_sites = combined
        .rebuilt_call_sites
        .saturating_add(publication.rebuilt_call_sites);
    combined.copied_call_sites = combined
        .copied_call_sites
        .saturating_add(publication.copied_call_sites);
}

pub fn index_repository(root: &Path) -> Result<IndexReport, WorkspaceIndexError> {
    index_repository_with_options(root, IndexOptions::default())
}

pub fn index_repository_with_options(
    root: &Path,
    options: IndexOptions,
) -> Result<IndexReport, WorkspaceIndexError> {
    let started = Instant::now();
    let budgets = options.budgets.validate()?;
    let worker_policy = WorkerPolicy::from_budgets(budgets);
    check_cancelled(&options.cancellation)?;
    let operation = OperationContext::from_cancellation(options.cancellation.clone());
    let repository_root = chakra_git::resolve_repository_root_with_context(root, &operation)?;
    let mut rss_peak = process_rss_bytes();
    let mut scan = scan_repository_sources_with_options(&repository_root, &options)?;
    rss_peak = max_option(rss_peak, process_rss_bytes());

    let prototypes = default_adapters();
    let limits = split_graph_limits(budgets, &scan.sources.counts());
    let (builds, mut cache_report, cache_write_plan) = build_adapters(
        &repository_root,
        &options,
        &mut scan,
        &prototypes,
        &limits,
        &worker_policy,
        &mut rss_peak,
    )?;
    check_cancelled(&options.cancellation)?;
    let adapter_count = builds.len() as u64;
    let mut built_adapters = Vec::with_capacity(builds.len());
    let mut built_graphs = Vec::with_capacity(builds.len());
    let mut built_metrics = Vec::with_capacity(builds.len());
    for build in builds {
        built_adapters.push(build.index);
        built_graphs.push(build.graph);
        built_metrics.push(build.metrics);
    }

    let composition_started = PhaseTimer::start();
    let graph = SymbolGraph::merge(built_graphs)?;
    let composition_phase = measured_phase(
        IndexPhase::LanguageComposition,
        None,
        composition_started,
        adapter_count,
        0,
        PhaseConcurrency::SERIAL,
    );
    let validation_started = PhaseTimer::start();
    let audit = graph.audit_consistency()?;
    let validation_phase = measured_phase(
        IndexPhase::GraphValidation,
        None,
        validation_started,
        audit
            .symbols_audited
            .saturating_add(audit.adjacency_entries_examined),
        0,
        PhaseConcurrency::SERIAL,
    );
    rss_peak = max_option(rss_peak, process_rss_bytes());

    let mut phases = scan.phases.clone();
    for metrics in &built_metrics {
        phases.extend(metrics.phases.clone());
    }
    phases.push(composition_phase);
    phases.push(validation_phase);
    let current_rss = process_rss_bytes();
    rss_peak = max_option(rss_peak, current_rss);
    let mut publication = IndexPublicationMetrics {
        rebuilt_source_bytes: scan.source_bytes,
        ..IndexPublicationMetrics::default()
    };
    let mut language_facts = Vec::with_capacity(built_metrics.len());
    for ((adapter, metrics), limits) in built_adapters
        .iter()
        .zip(built_metrics.iter())
        .zip(limits.iter().copied())
    {
        publication.rebuilt_files = publication
            .rebuilt_files
            .saturating_add(metrics.facts.files);
        publication.rebuilt_symbols = publication
            .rebuilt_symbols
            .saturating_add(metrics.graph.retained_symbols);
        publication.rebuilt_edges = publication
            .rebuilt_edges
            .saturating_add(metrics.graph.retained_edges);
        publication.rebuilt_call_sites = publication
            .rebuilt_call_sites
            .saturating_add(metrics.graph.retained_call_sites);
        language_facts.push(LanguageIndexingFacts {
            language: adapter.language(),
            facts: metrics.facts,
            graph: metrics.graph,
            limits,
        });
    }
    let indexing = build_indexing_status(
        budgets,
        &scan,
        IndexingParts {
            languages: language_facts,
            phases,
            memory: IndexMemoryMetrics {
                current_rss_bytes: current_rss,
                observed_phase_peak_rss_bytes: rss_peak,
                ..IndexMemoryMetrics::default()
            },
            publication,
        },
    );
    let elapsed = started.elapsed();
    if elapsed.as_millis() > u128::from(budgets.startup_target_millis) {
        warn!(
            elapsed_millis = elapsed.as_millis(),
            target_millis = budgets.startup_target_millis,
            "syntax startup exceeded its observable wall-time target"
        );
    }
    if rss_peak.is_some_and(|rss| rss > budgets.memory_target_bytes) {
        warn!(
            observed_phase_peak_rss_bytes = ?rss_peak,
            target_bytes = budgets.memory_target_bytes,
            "syntax startup exceeded its observable memory target"
        );
    }
    let language_files = |language: Language| {
        built_adapters
            .iter()
            .find(|adapter| adapter.language() == language)
            .map_or(0, |adapter| adapter.fact_counts().files)
    };
    let rust_files = language_files(Language::Rust);
    let php_files = language_files(Language::Php);
    let typescript_files = language_files(Language::TypeScript);
    let python_files = language_files(Language::Python);
    let javascript_files = language_files(Language::JavaScript);
    let java_files = language_files(Language::Java);
    let csharp_files = language_files(Language::CSharp);
    let shell_files = language_files(Language::Shell);
    let cpp_files = language_files(Language::Cpp);
    let hcl_files = language_files(Language::Hcl);
    let go_files = language_files(Language::Go);
    let mut framework = AdapterFrameworkMetrics::default();
    for metrics in &built_metrics {
        framework.detected |= metrics.framework.detected;
        framework.symbols = framework.symbols.saturating_add(metrics.framework.symbols);
        framework.edges = framework.edges.saturating_add(metrics.framework.edges);
        framework.truncated_files = framework
            .truncated_files
            .saturating_add(metrics.framework.truncated_files);
    }
    if let Some((key, store, source_hashes)) = cache_write_plan.as_ref() {
        let needs_write = !matches!(
            cache_report.restore,
            CacheRestoreOutcome::Restored { misses: 0, .. }
        );
        if needs_write {
            let mut files = Vec::new();
            for adapter in &built_adapters {
                let language = adapter.language();
                for facts in adapter.export_file_facts() {
                    let Some(hash) = source_hashes.get(&facts.path).copied() else {
                        continue;
                    };
                    files.push(FactsToStore {
                        language,
                        facts,
                        content_hash: hash,
                    });
                }
            }
            match store.write(key, &files) {
                Ok(outcome) => cache_report.write = Some(outcome),
                Err(error) => {
                    warn!(
                        error = %error,
                        "syntax fact cache publication failed; continuing without a cache"
                    );
                }
            }
        }
    }
    let syntax_index = WorkspaceSyntaxIndex {
        adapters: built_adapters
            .into_iter()
            .zip(limits.iter().copied())
            .map(|(adapter, limits)| WorkspaceAdapterState { adapter, limits })
            .collect(),
        budgets,
        indexing: indexing.clone(),
        provider_inputs: scan.provider_inputs.clone(),
    };
    let metrics = IndexMetrics {
        discovered_files: scan.discovered_files,
        parsed_files: scan.indexed_files,
        syntax_error_files: indexing.coverage.syntax_error_files,
        truncated_call_sites: indexing.coverage.omitted_call_sites,
        symbols: graph.symbol_count(),
        edges: graph.edge_count(),
        call_sites: graph.call_site_count(),
        ambiguous_call_sites: graph.ambiguous_call_site_count(),
        unresolved_call_sites: graph.unresolved_call_site_count(),
        rust_files,
        php_files,
        typescript_files,
        python_files,
        javascript_files,
        java_files,
        csharp_files,
        shell_files,
        cpp_files,
        hcl_files,
        go_files,
        laravel_detected: framework.detected,
        framework_symbols: framework.symbols,
        framework_edges: framework.edges,
        framework_truncated_files: framework.truncated_files,
        elapsed,
        indexing,
    };
    info!(
        discovered_files = metrics.discovered_files,
        parsed_files = metrics.parsed_files,
        symbols = metrics.symbols,
        edges = metrics.edges,
        call_sites = metrics.call_sites,
        degraded = metrics.indexing.is_degraded(),
        configured_workers = metrics.indexing.scheduling.configured_max_workers,
        effective_worker_limit = metrics.indexing.scheduling.effective_worker_limit,
        peak_active_workers = metrics.indexing.scheduling.peak_active_workers,
        elapsed_micros = elapsed.as_micros(),
        "bounded multi-language syntax index completed"
    );
    Ok(IndexReport {
        repository_root,
        graph,
        metrics,
        provider_inputs: syntax_index.provider_inputs.clone(),
        syntax_index,
        cache: cache_report,
    })
}

/// Cache write inputs threaded from the build attempt to the post-build
/// publication: the resolved key, the store, and the content hashes of every
/// scanned source.
type CacheWritePlan = (
    CompatibilityKey,
    CacheStore,
    BTreeMap<RepoRelativePath, [u8; 16]>,
);

/// Builds every language partition, restoring from the syntax fact cache
/// when it is enabled, above the B1 size gate, and compatible. Every cache
/// failure mode degrades to the deterministic cold build — per file
/// (reparse), per partition (adapter cold build), or for the whole
/// workspace (full rebuild). The returned report records exactly which path
/// produced the revision.
#[allow(clippy::too_many_arguments)]
fn build_adapters(
    repository_root: &Path,
    options: &IndexOptions,
    scan: &mut WorkspaceSourceScan,
    prototypes: &[Box<dyn SyntaxLanguageAdapter>],
    limits: &[GraphBuildLimits],
    worker_policy: &WorkerPolicy,
    rss_peak: &mut Option<u64>,
) -> Result<
    (
        Vec<AdapterColdBuild>,
        SyntaxCacheReport,
        Option<CacheWritePlan>,
    ),
    WorkspaceIndexError,
> {
    let Some(config) = options.cache.config() else {
        let builds = cold_builds(
            scan,
            prototypes,
            limits,
            worker_policy,
            repository_root,
            &options.cancellation,
            rss_peak,
        )?;
        return Ok((builds, SyntaxCacheReport::default(), None));
    };
    if scan.indexed_files <= config.min_indexed_files {
        let report = SyntaxCacheReport {
            restore: CacheRestoreOutcome::BelowGate {
                indexed_files: scan.indexed_files,
                gate: config.min_indexed_files,
            },
            ..SyntaxCacheReport::default()
        };
        let builds = cold_builds(
            scan,
            prototypes,
            limits,
            worker_policy,
            repository_root,
            &options.cancellation,
            rss_peak,
        )?;
        return Ok((builds, report, None));
    }

    let cold_fallback = |scan: &mut WorkspaceSourceScan,
                         rss_peak: &mut Option<u64>|
     -> Result<Vec<AdapterColdBuild>, WorkspaceIndexError> {
        cold_builds(
            scan,
            prototypes,
            limits,
            worker_policy,
            repository_root,
            &options.cancellation,
            rss_peak,
        )
    };
    let extractors = prototypes
        .iter()
        .map(|adapter| (adapter.language(), adapter.extractor_version()))
        .collect();
    let key = match CompatibilityKey::resolve(repository_root, &options.budgets, extractors) {
        Ok(key) => key,
        Err(error) => {
            let report = SyntaxCacheReport {
                restore: CacheRestoreOutcome::Fallback {
                    reason: format!("cache identity resolution failed: {error}"),
                },
                ..SyntaxCacheReport::default()
            };
            return Ok((cold_fallback(scan, rss_peak)?, report, None));
        }
    };
    let store = CacheStore::new(config.clone());
    let source_hashes = hash_scan_sources(scan);
    let (manifest, rejection) = store.read_compatible_manifest(&key);
    let Some((entries, manifest_bytes)) = manifest else {
        let report = SyntaxCacheReport {
            restore: CacheRestoreOutcome::Fallback {
                reason: rejection.unwrap_or_else(|| "cache unavailable".to_owned()),
            },
            ..SyntaxCacheReport::default()
        };
        let builds = cold_fallback(scan, rss_peak)?;
        return Ok((builds, report, Some((key, store, source_hashes))));
    };

    let entry_map: std::collections::HashMap<&RepoRelativePath, &ManifestEntry> =
        entries.iter().map(|entry| (&entry.path, entry)).collect();
    let mut hits = 0_u64;
    let mut misses = 0_u64;
    let mut bytes_read = manifest_bytes;
    let mut builds = Vec::with_capacity(prototypes.len());
    for (adapter, limits) in prototypes.iter().zip(limits.iter().copied()) {
        let language = adapter.language();
        let sources = scan.sources.take(language);
        let mut facts = Vec::new();
        for path in sources.files.keys() {
            let hit = entry_map
                .get(path)
                .filter(|entry| source_hashes.get(path) == Some(&entry.content_hash))
                .and_then(|entry| store.read_facts(entry, language).ok());
            match hit {
                Some((fact, bytes)) => {
                    bytes_read = bytes_read.saturating_add(bytes);
                    facts.push(fact);
                    hits = hits.saturating_add(1);
                }
                None => misses = misses.saturating_add(1),
            }
        }
        let fallback_sources = sources.clone();
        let build = match adapter.cached_build(
            sources,
            facts,
            limits,
            worker_policy.effective_worker_limit as usize,
            PARALLEL_PARSE_FILE_THRESHOLD as usize,
            repository_root,
            &options.cancellation,
        ) {
            Ok(build) => build,
            Err(error) => {
                warn!(
                    error = %error,
                    ?language,
                    "cached build failed; falling back to a deterministic build for this partition"
                );
                adapter
                    .cold_build(
                        fallback_sources,
                        limits,
                        worker_policy.effective_worker_limit as usize,
                        PARALLEL_PARSE_FILE_THRESHOLD as usize,
                        repository_root,
                        &options.cancellation,
                    )
                    .map_err(|source| WorkspaceIndexError::Adapter {
                        language,
                        source: Box::new(source),
                    })?
            }
        };
        builds.push(build);
        *rss_peak = max_option(*rss_peak, process_rss_bytes());
    }
    let report = SyntaxCacheReport {
        restore: CacheRestoreOutcome::Restored { hits, misses },
        write: None,
        bytes_read,
    };
    Ok((builds, report, Some((key, store, source_hashes))))
}

/// The deterministic build path: every partition parses its classified
/// sources through the bounded cold build.
fn cold_builds(
    scan: &mut WorkspaceSourceScan,
    prototypes: &[Box<dyn SyntaxLanguageAdapter>],
    limits: &[GraphBuildLimits],
    worker_policy: &WorkerPolicy,
    repository_root: &Path,
    cancellation: &IndexCancellation,
    rss_peak: &mut Option<u64>,
) -> Result<Vec<AdapterColdBuild>, WorkspaceIndexError> {
    let mut builds = Vec::with_capacity(prototypes.len());
    for (adapter, limits) in prototypes.iter().zip(limits.iter().copied()) {
        let language = adapter.language();
        let sources = scan.sources.take(language);
        builds.push(
            adapter
                .cold_build(
                    sources,
                    limits,
                    worker_policy.effective_worker_limit as usize,
                    PARALLEL_PARSE_FILE_THRESHOLD as usize,
                    repository_root,
                    cancellation,
                )
                .map_err(|source| WorkspaceIndexError::Adapter {
                    language,
                    source: Box::new(source),
                })?,
        );
        *rss_peak = max_option(*rss_peak, process_rss_bytes());
    }
    Ok(builds)
}

/// Content hashes of every scanned source, computed once and shared by
/// cache validation (restore) and cache publication (write).
fn hash_scan_sources(scan: &WorkspaceSourceScan) -> BTreeMap<RepoRelativePath, [u8; 16]> {
    let mut hashes = BTreeMap::new();
    for language in scan.sources.languages() {
        for (path, source) in &language.sources.files {
            hashes.insert(path.clone(), content_hash(source));
        }
    }
    hashes
}

/// Compatibility helper using safe defaults.
pub fn scan_repository_sources(
    repository_root: &Path,
) -> Result<WorkspaceSources, WorkspaceIndexError> {
    Ok(scan_repository_sources_with_options(repository_root, &IndexOptions::default())?.sources)
}

pub fn scan_repository_sources_with_options(
    repository_root: &Path,
    options: &IndexOptions,
) -> Result<WorkspaceSourceScan, WorkspaceIndexError> {
    check_cancelled(&options.cancellation)?;
    let operation = OperationContext::from_cancellation(options.cancellation.clone());
    let inventory_started = PhaseTimer::start();
    let inventory = chakra_git::discover_workspace_inventory_in_worktree_with_context(
        repository_root,
        &operation,
    )?;
    let inventory_phase = measured_phase(
        IndexPhase::GitInventory,
        None,
        inventory_started,
        inventory.sources.len() as u64,
        0,
        PhaseConcurrency::SERIAL,
    );
    scan_discovered_sources_with_inventory_phase(
        repository_root,
        options,
        &inventory,
        inventory_phase,
        &mut FilesystemSourceLoader,
        &operation,
    )
}

pub(crate) trait WorkspaceSourceLoader {
    fn observe(&mut self, path: &RepoRelativePath, metadata: &fs::Metadata);

    fn observe_metadata(&mut self, path: &RepoRelativePath, metadata: &fs::Metadata);

    fn load(
        &mut self,
        absolute: &Path,
        path: &RepoRelativePath,
        metadata: &fs::Metadata,
        max_bytes: u64,
    ) -> Result<Arc<str>, WorkspaceIndexError>;
}

struct FilesystemSourceLoader;

impl WorkspaceSourceLoader for FilesystemSourceLoader {
    fn observe(&mut self, _path: &RepoRelativePath, _metadata: &fs::Metadata) {}

    fn observe_metadata(&mut self, _path: &RepoRelativePath, _metadata: &fs::Metadata) {}

    fn load(
        &mut self,
        absolute: &Path,
        path: &RepoRelativePath,
        _metadata: &fs::Metadata,
        max_bytes: u64,
    ) -> Result<Arc<str>, WorkspaceIndexError> {
        let file = fs::File::open(absolute).map_err(|source| WorkspaceIndexError::Read {
            path: path.clone(),
            source,
        })?;
        let mut source = String::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_string(&mut source)
            .map_err(|source| WorkspaceIndexError::Read {
                path: path.clone(),
                source,
            })?;
        Ok(Arc::<str>::from(source))
    }
}

pub(crate) fn scan_discovered_sources_with_options(
    repository_root: &Path,
    options: &IndexOptions,
    inventory: &chakra_git::WorkspaceInventory,
    inventory_elapsed: Duration,
    loader: &mut impl WorkspaceSourceLoader,
    operation: &OperationContext,
) -> Result<WorkspaceSourceScan, WorkspaceIndexError> {
    scan_discovered_sources_with_inventory_phase(
        repository_root,
        options,
        inventory,
        phase(
            IndexPhase::GitInventory,
            None,
            inventory_elapsed,
            inventory.sources.len() as u64,
            0,
        ),
        loader,
        operation,
    )
}

fn scan_discovered_sources_with_inventory_phase(
    repository_root: &Path,
    options: &IndexOptions,
    inventory: &chakra_git::WorkspaceInventory,
    inventory_phase: IndexPhaseMeasurement,
    loader: &mut impl WorkspaceSourceLoader,
    operation: &OperationContext,
) -> Result<WorkspaceSourceScan, WorkspaceIndexError> {
    let budgets = options.budgets.validate()?;
    check_cancelled(&options.cancellation)?;
    operation
        .check()
        .map_err(|_| WorkspaceIndexError::Cancelled)?;
    let discovered_files = inventory.sources.len() as u64;
    let read_started = PhaseTimer::start();
    let mut files_by_language: BTreeMap<Language, BTreeMap<RepoRelativePath, Arc<str>>> =
        BTreeMap::new();
    let mut source_bytes = 0_u64;
    let mut oversized_files = 0_u64;
    let mut largest_file = 0_u64;
    let mut workspace_omitted = 0_u64;
    let mut workspace_observed = 0_u64;
    let mut unreadable_files = 0_u64;
    let mut unreadable_paths = Vec::new();

    for (index, path) in inventory.sources.iter().enumerate() {
        check_cancelled(&options.cancellation)?;
        if index as u64 >= budgets.max_files {
            continue;
        }
        let absolute = repository_root.join(path.as_str());
        // A file may vanish or become unreadable between inventory and read;
        // skip it instead of aborting the whole scan.
        let metadata = match fs::metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(source) => {
                let error = WorkspaceIndexError::Read {
                    path: path.clone(),
                    source,
                };
                warn!(error = %error, "skipping source file that cannot be inspected");
                unreadable_files = unreadable_files.saturating_add(1);
                unreadable_paths.push(path.clone());
                continue;
            }
        };
        loader.observe(path, &metadata);
        let measured_len = metadata.len();
        if measured_len > budgets.max_source_file_bytes {
            oversized_files = oversized_files.saturating_add(1);
            largest_file = largest_file.max(measured_len);
            continue;
        }
        if source_bytes.saturating_add(measured_len) > budgets.max_workspace_source_bytes {
            workspace_omitted = workspace_omitted.saturating_add(1);
            workspace_observed = workspace_observed.max(source_bytes.saturating_add(measured_len));
            continue;
        }
        let source = match loader.load(&absolute, path, &metadata, budgets.max_source_file_bytes) {
            Ok(source) => source,
            Err(error @ WorkspaceIndexError::Read { .. }) => {
                warn!(error = %error, "skipping source file that cannot be read");
                unreadable_files = unreadable_files.saturating_add(1);
                unreadable_paths.push(path.clone());
                continue;
            }
            Err(error) => return Err(error),
        };
        let actual_len = source.len() as u64;
        if actual_len > budgets.max_source_file_bytes {
            oversized_files = oversized_files.saturating_add(1);
            largest_file = largest_file.max(actual_len);
            continue;
        }
        if source_bytes.saturating_add(actual_len) > budgets.max_workspace_source_bytes {
            workspace_omitted = workspace_omitted.saturating_add(1);
            workspace_observed = workspace_observed.max(source_bytes.saturating_add(actual_len));
            continue;
        }
        source_bytes = source_bytes.saturating_add(actual_len);
        if let Some(language) = chakra_git::source_language(path.as_str()) {
            files_by_language
                .entry(language)
                .or_default()
                .insert(path.clone(), source);
        }
    }

    let mut provider_inputs = Vec::new();
    for path in &inventory.metadata_inputs {
        operation
            .check()
            .map_err(|_| WorkspaceIndexError::Cancelled)?;
        let absolute = repository_root.join(path.as_str());
        let metadata = match fs::metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(source) => {
                let error = WorkspaceIndexError::Read {
                    path: path.clone(),
                    source,
                };
                warn!(error = %error, "skipping metadata input that cannot be inspected");
                unreadable_files = unreadable_files.saturating_add(1);
                unreadable_paths.push(path.clone());
                continue;
            }
        };
        if let Some(input) = ProviderInput::from_metadata(
            path.clone(),
            chakra_git::metadata_languages(path.as_str())
                .iter()
                .copied(),
            &metadata,
        ) {
            provider_inputs.push(input);
        }
        loader.observe_metadata(path, &metadata);
    }

    let mut languages = Vec::new();
    let mut indexed_files = 0_u64;
    for language in registered_languages() {
        let files = files_by_language.remove(&language).unwrap_or_default();
        let metadata = chakra_git::classify_discovered_sources_with_context(
            repository_root,
            &inventory.sources,
            &inventory.metadata_inputs,
            language,
            operation,
        )?
        .into_iter()
        .filter(|source| files.contains_key(&source.path))
        .map(|source| (source.path, source.metadata))
        .collect();
        indexed_files = indexed_files.saturating_add(files.len() as u64);
        languages.push(WorkspaceLanguageSources {
            language,
            sources: LanguageSources { files, metadata },
        });
    }
    let mut degradations = Vec::new();
    if discovered_files > budgets.max_files {
        degradations.push(IndexDegradation {
            phase: IndexPhase::GitInventory,
            language: None,
            cause: IndexBudgetKind::Files,
            affected_capabilities: all_index_capabilities(),
            limit: budgets.max_files,
            observed: discovered_files,
            omitted: discovered_files.saturating_sub(budgets.max_files),
        });
    }
    if oversized_files > 0 {
        degradations.push(IndexDegradation {
            phase: IndexPhase::SourceRead,
            language: None,
            cause: IndexBudgetKind::SourceFileBytes,
            affected_capabilities: all_index_capabilities(),
            limit: budgets.max_source_file_bytes,
            observed: largest_file,
            omitted: oversized_files,
        });
    }
    if workspace_omitted > 0 {
        degradations.push(IndexDegradation {
            phase: IndexPhase::SourceRead,
            language: None,
            cause: IndexBudgetKind::WorkspaceSourceBytes,
            affected_capabilities: all_index_capabilities(),
            limit: budgets.max_workspace_source_bytes,
            observed: workspace_observed,
            omitted: workspace_omitted,
        });
    }
    let phases = vec![
        inventory_phase,
        measured_phase(
            IndexPhase::SourceRead,
            None,
            read_started,
            indexed_files,
            source_bytes,
            PhaseConcurrency::SERIAL,
        ),
    ];
    Ok(WorkspaceSourceScan {
        sources: WorkspaceSources { languages },
        provider_inputs,
        discovered_files,
        indexed_files,
        source_bytes,
        unreadable_files,
        unreadable_paths,
        degradations,
        phases,
    })
}

/// Splits the graph budgets proportionally to each language's file count, in
/// registry order. Each language receives `remaining * count / remaining
/// total`, so the last nonzero language gets the remainder; for two languages
/// this is exactly the historical Rust/PHP split.
fn split_graph_limits(budgets: IndexBudgets, file_counts: &[u64]) -> Vec<GraphBuildLimits> {
    let split = |limit: u64| {
        let mut remaining_limit = limit;
        let mut remaining_total: u64 = file_counts
            .iter()
            .fold(0_u64, |total, count| total.saturating_add(*count));
        file_counts
            .iter()
            .map(|count| {
                if *count == 0 || remaining_total == 0 {
                    return 0;
                }
                let share = remaining_limit.saturating_mul(*count) / remaining_total;
                remaining_limit = remaining_limit.saturating_sub(share);
                remaining_total = remaining_total.saturating_sub(*count);
                share
            })
            .collect::<Vec<_>>()
    };
    let symbols = split(budgets.max_symbols);
    let edges = split(budgets.max_edges);
    let calls = split(budgets.max_call_sites);
    symbols
        .into_iter()
        .zip(edges)
        .zip(calls)
        .map(
            |((max_symbols, max_edges), max_call_sites)| GraphBuildLimits {
                max_symbols,
                max_edges,
                max_call_sites,
            },
        )
        .collect()
}

/// One language's facts, graph report, and limits for the indexing status, in
/// registry order.
struct LanguageIndexingFacts {
    language: Language,
    facts: AdapterFactCounts,
    graph: GraphBuildReport,
    limits: GraphBuildLimits,
}

struct IndexingParts {
    languages: Vec<LanguageIndexingFacts>,
    phases: Vec<IndexPhaseMeasurement>,
    memory: IndexMemoryMetrics,
    publication: IndexPublicationMetrics,
}

fn build_indexing_status(
    budgets: IndexBudgets,
    scan: &WorkspaceSourceScan,
    parts: IndexingParts,
) -> IndexingStatus {
    let IndexingParts {
        languages,
        phases,
        mut memory,
        publication,
    } = parts;
    let mut extracted_symbols = 0_u64;
    let mut extracted_call_sites = 0_u64;
    let mut extracted_relationship_edges = 0_u64;
    let mut parsed_files = 0_u64;
    let mut syntax_error_files = 0_u64;
    let mut retained_symbols = 0_u64;
    let mut retained_edges = 0_u64;
    let mut omitted_edges = 0_u64;
    let mut retained_call_sites = 0_u64;
    let mut omitted_call_sites = 0_u64;
    let mut unknown_relationship_omissions = 0_u64;
    for language in &languages {
        extracted_symbols = extracted_symbols.saturating_add(language.facts.symbols);
        extracted_call_sites = extracted_call_sites.saturating_add(language.facts.call_sites);
        extracted_relationship_edges =
            extracted_relationship_edges.saturating_add(language.facts.relationship_edges);
        parsed_files = parsed_files.saturating_add(language.facts.files);
        syntax_error_files = syntax_error_files.saturating_add(language.facts.syntax_error_files);
        retained_symbols = retained_symbols.saturating_add(language.graph.retained_symbols);
        retained_edges = retained_edges.saturating_add(language.graph.retained_edges);
        omitted_edges = omitted_edges.saturating_add(language.graph.omitted_edges);
        retained_call_sites =
            retained_call_sites.saturating_add(language.graph.retained_call_sites);
        omitted_call_sites = omitted_call_sites.saturating_add(language.graph.omitted_call_sites);
        unknown_relationship_omissions = unknown_relationship_omissions
            .saturating_add(language.graph.call_sites_omitted_by_symbol_budget);
    }
    let skipped_files = scan.discovered_files.saturating_sub(scan.indexed_files);
    let coverage = IndexCoverage {
        discovered_files: scan.discovered_files,
        indexed_files: scan.indexed_files,
        skipped_files,
        unreadable_files: scan.unreadable_files,
        source_bytes: scan.source_bytes,
        parsed_files,
        syntax_error_files,
        extracted_symbols,
        retained_symbols,
        retained_edges,
        omitted_edges,
        extracted_call_sites,
        retained_call_sites,
        omitted_call_sites,
    };
    let mut degradations = scan.degradations.clone();
    for language in &languages {
        append_graph_degradations(
            &mut degradations,
            language.language,
            language.limits,
            language.graph,
        );
    }
    let capabilities = vec![
        capability(
            IndexCapability::FileInventory,
            scan.indexed_files,
            skipped_files,
            true,
        ),
        capability(
            IndexCapability::TextSearch,
            scan.indexed_files,
            skipped_files,
            true,
        ),
        capability(
            IndexCapability::Declarations,
            retained_symbols,
            extracted_symbols.saturating_sub(retained_symbols),
            skipped_files == 0,
        ),
        capability(
            IndexCapability::Relationships,
            retained_edges,
            omitted_edges,
            skipped_files == 0 && unknown_relationship_omissions == 0,
        ),
        capability(
            IndexCapability::CallSites,
            retained_call_sites,
            omitted_call_sites,
            skipped_files == 0,
        ),
    ];
    memory.retained_source_bytes = scan.source_bytes;
    memory.retained_parsed_symbols = extracted_symbols;
    memory.retained_parsed_relationship_edges = extracted_relationship_edges;
    memory.retained_parsed_call_sites = extracted_call_sites;
    memory.retained_graph_symbols = retained_symbols;
    memory.retained_graph_edges = retained_edges;
    memory.retained_graph_call_sites = retained_call_sites;
    let scheduling = WorkerPolicy::from_budgets(budgets).scheduling(&phases);
    IndexingStatus {
        budgets,
        coverage,
        capabilities,
        degradations,
        phases,
        scheduling,
        memory,
        publication,
    }
}

fn capability(
    capability: IndexCapability,
    retained: u64,
    omitted: u64,
    corpus_complete: bool,
) -> IndexCapabilityCoverage {
    IndexCapabilityCoverage {
        capability,
        retained,
        omitted,
        complete: corpus_complete && omitted == 0,
    }
}

fn append_graph_degradations(
    degradations: &mut Vec<IndexDegradation>,
    language: Language,
    limits: GraphBuildLimits,
    report: GraphBuildReport,
) {
    let mut record = |cause, affected_capabilities, limit, observed, omitted| {
        if omitted != 0 {
            degradations.push(IndexDegradation {
                phase: IndexPhase::GraphMaterialization,
                language: Some(language),
                cause,
                affected_capabilities,
                limit,
                observed,
                omitted,
            });
        }
    };
    let observed_symbols = report
        .retained_symbols
        .saturating_add(report.omitted_symbols);
    let observed_edges = report
        .retained_edges
        .saturating_add(report.edges_omitted_by_edge_budget);
    let observed_call_sites = report
        .retained_call_sites
        .saturating_add(report.call_sites_omitted_by_call_site_budget);
    record(
        IndexBudgetKind::Symbols,
        vec![IndexCapability::Declarations],
        limits.max_symbols,
        observed_symbols,
        report.omitted_symbols,
    );
    record(
        IndexBudgetKind::Symbols,
        vec![IndexCapability::Relationships],
        limits.max_symbols,
        observed_symbols,
        report.edges_omitted_by_symbol_budget,
    );
    record(
        IndexBudgetKind::Symbols,
        vec![IndexCapability::Relationships, IndexCapability::CallSites],
        limits.max_symbols,
        observed_symbols,
        report.call_sites_omitted_by_symbol_budget,
    );
    record(
        IndexBudgetKind::Edges,
        vec![IndexCapability::Relationships],
        limits.max_edges,
        observed_edges,
        report.edges_omitted_by_edge_budget,
    );
    record(
        IndexBudgetKind::Edges,
        vec![IndexCapability::CallSites],
        limits.max_edges,
        observed_edges,
        report.call_sites_omitted_by_edge_budget,
    );
    record(
        IndexBudgetKind::CallSites,
        vec![IndexCapability::Relationships],
        limits.max_call_sites,
        observed_call_sites,
        report.edges_omitted_by_call_site_budget,
    );
    record(
        IndexBudgetKind::CallSites,
        vec![IndexCapability::CallSites],
        limits.max_call_sites,
        observed_call_sites,
        report.call_sites_omitted_by_call_site_budget,
    );
}

fn indexing_semantically_equal(left: &IndexingStatus, right: &IndexingStatus) -> bool {
    left.budgets == right.budgets
        && left.coverage == right.coverage
        && left.capabilities == right.capabilities
        && left.degradations == right.degradations
}

fn all_index_capabilities() -> Vec<IndexCapability> {
    vec![
        IndexCapability::FileInventory,
        IndexCapability::TextSearch,
        IndexCapability::Declarations,
        IndexCapability::Relationships,
        IndexCapability::CallSites,
    ]
}

fn check_cancelled(cancellation: &IndexCancellation) -> Result<(), WorkspaceIndexError> {
    if cancellation.is_cancelled() {
        Err(WorkspaceIndexError::Cancelled)
    } else {
        Ok(())
    }
}

fn phase(
    phase: IndexPhase,
    language: Option<Language>,
    elapsed: Duration,
    work_items: u64,
    bytes: u64,
) -> IndexPhaseMeasurement {
    IndexPhaseMeasurement {
        phase,
        language,
        elapsed_micros: elapsed.as_micros().min(u128::from(u64::MAX)) as u64,
        cpu_micros: None,
        cpu_utilization_per_mille: None,
        work_items,
        bytes,
        effective_workers: u64::from(work_items > 0),
        peak_active_workers: u64::from(work_items > 0),
        peak_queue_depth: 0,
        rss_bytes: None,
        peak_rss_bytes: process_peak_rss_bytes(),
    }
}

fn measured_phase(
    phase: IndexPhase,
    language: Option<Language>,
    started: PhaseTimer,
    work_items: u64,
    bytes: u64,
    concurrency: PhaseConcurrency,
) -> IndexPhaseMeasurement {
    let elapsed = started.wall.elapsed();
    let elapsed_micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
    let cpu_micros = process_cpu_micros()
        .zip(started.cpu_micros)
        .map(|(end, start)| end.saturating_sub(start));
    let cpu_utilization_per_mille = cpu_micros.map(|cpu| {
        cpu.saturating_mul(1_000)
            .checked_div(elapsed_micros.max(1))
            .unwrap_or(0)
    });
    IndexPhaseMeasurement {
        phase,
        language,
        elapsed_micros,
        cpu_micros,
        cpu_utilization_per_mille,
        work_items,
        bytes,
        effective_workers: if work_items == 0 {
            0
        } else {
            concurrency.effective_workers
        },
        peak_active_workers: if work_items == 0 {
            0
        } else {
            concurrency.peak_active_workers
        },
        peak_queue_depth: concurrency.peak_queue_depth,
        rss_bytes: (work_items >= PARALLEL_PARSE_FILE_THRESHOLD)
            .then(process_rss_bytes)
            .flatten(),
        peak_rss_bytes: process_peak_rss_bytes(),
    }
}

#[cfg(unix)]
fn process_cpu_micros() -> Option<u64> {
    let usage = getrusage(UsageWho::RUSAGE_SELF).ok()?;
    let total = usage
        .user_time()
        .num_microseconds()
        .checked_add(usage.system_time().num_microseconds())?;
    u64::try_from(total).ok()
}

#[cfg(not(unix))]
fn process_cpu_micros() -> Option<u64> {
    None
}

#[cfg(unix)]
fn process_peak_rss_bytes() -> Option<u64> {
    let rss = u64::try_from(getrusage(UsageWho::RUSAGE_SELF).ok()?.max_rss()).ok()?;
    #[cfg(target_vendor = "apple")]
    {
        Some(rss)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        rss.checked_mul(1024)
    }
}

#[cfg(not(unix))]
fn process_peak_rss_bytes() -> Option<u64> {
    None
}

fn max_option(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    }
}

#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    line.split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_rss_bytes() -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p"])
        .arg(std::process::id().to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

#[cfg(not(unix))]
fn process_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::process::Command;

    use chakra_domain::query::{QueryService, RepoMapRequest, SearchRequest, SymbolSearchRequest};
    use chakra_domain::state::{Freshness, WorkspaceStatus};
    use tempfile::TempDir;

    use super::*;

    fn repository() -> Result<TempDir, Box<dyn Error>> {
        let repository = TempDir::new()?;
        let status = Command::new("git")
            .current_dir(repository.path())
            .args(["init", "--quiet"])
            .status()?;
        if !status.success() {
            return Err("git init failed".into());
        }
        Ok(repository)
    }

    #[test]
    fn combines_rust_and_php_without_cross_language_call_edges() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(
            repository.path().join("lib.rs"),
            "pub fn rust_caller() { shared(); }\npub fn shared() {}\n",
        )?;
        fs::write(
            repository.path().join("service.php"),
            "<?php function php_caller(): void { shared(); } function shared(): void {}\n",
        )?;

        let report = index_repository(repository.path())?;
        assert_eq!(report.graph.resolve_name("shared").len(), 2);
        for symbol in report.graph.symbols() {
            for edge in report.graph.outgoing_edges(symbol.id) {
                let target = report.graph.symbol(edge.to).ok_or("call target missing")?;
                assert_eq!(symbol.key.language, target.key.language);
            }
        }
        assert_eq!(report.metrics.rust_files, 1);
        assert_eq!(report.metrics.php_files, 1);
        assert!(!report.metrics.indexing.is_degraded());
        assert_eq!(report.metrics.indexing.scheduling.peak_active_workers, 1);
        assert_eq!(report.metrics.indexing.scheduling.parallel_parse_files, 0);
        Ok(())
    }

    #[test]
    fn shared_worker_policy_bounds_both_language_adapters() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        for index in 0..40 {
            fs::write(
                repository.path().join(format!("generated_{index:03}.rs")),
                format!("pub fn rust_{index}() {{}}\n"),
            )?;
            fs::write(
                repository.path().join(format!("Generated{index:03}.php")),
                format!("<?php function php_{index}(): void {{}}\n"),
            )?;
        }
        let budgets = IndexBudgets {
            max_workers: 2,
            ..IndexBudgets::default()
        };
        let report = index_repository_with_options(
            repository.path(),
            IndexOptions::new(budgets, IndexCancellation::default())?,
        )?;
        let scheduling = &report.metrics.indexing.scheduling;
        assert_eq!(scheduling.configured_max_workers, 2);
        assert!(scheduling.effective_worker_limit <= 2);
        assert!(scheduling.peak_active_workers <= scheduling.effective_worker_limit);
        for phase in &report.metrics.indexing.phases {
            assert!(phase.effective_workers <= scheduling.effective_worker_limit);
            assert!(phase.peak_active_workers <= phase.effective_workers);
        }
        if scheduling.effective_worker_limit > 1 {
            assert_eq!(scheduling.parallel_parse_files, 80);
            assert!((1..=2).contains(&scheduling.peak_active_workers));
        } else {
            assert_eq!(scheduling.sequential_parse_files, 80);
            assert!(scheduling.low_resource_mode);
        }
        assert_eq!(scheduling.peak_queue_depth, 0);
        report.graph.validate_consistency()?;
        Ok(())
    }

    #[test]
    fn memory_policy_can_force_single_worker_mode() {
        let budgets = IndexBudgets {
            max_workers: 8,
            memory_target_bytes: INDEX_WORKER_MEMORY_RESERVE_BYTES,
            ..IndexBudgets::default()
        };
        let policy = WorkerPolicy::from_budgets(budgets);
        assert_eq!(policy.source_memory_reserve_bytes, 0);
        assert_eq!(
            policy.worker_memory_reserve_bytes,
            INDEX_WORKER_MEMORY_RESERVE_BYTES
        );
        assert_eq!(policy.memory_limited_workers, 1);
        assert_eq!(policy.effective_worker_limit, 1);
        let scheduling = policy.scheduling(&[]);
        assert!(scheduling.low_resource_mode);
    }

    #[test]
    fn file_budget_publishes_useful_deterministic_degradation() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(repository.path().join("a.rs"), "pub fn alpha() {}\n")?;
        fs::write(repository.path().join("b.rs"), "pub fn beta() {}\n")?;
        let budgets = IndexBudgets {
            max_files: 1,
            ..IndexBudgets::default()
        };
        let options = IndexOptions::new(budgets, IndexCancellation::default())?;
        let first = index_repository_with_options(repository.path(), options.clone())?;
        let second = index_repository_with_options(repository.path(), options)?;

        assert_eq!(first.graph.file_count(), 1);
        assert_eq!(first.metrics.indexing.coverage.discovered_files, 2);
        assert_eq!(first.metrics.indexing.coverage.skipped_files, 1);
        assert!(
            first
                .metrics
                .indexing
                .capabilities
                .iter()
                .all(|capability| !capability.complete)
        );
        assert_eq!(
            first.metrics.indexing.degradations,
            second.metrics.indexing.degradations
        );
        assert_eq!(
            first.metrics.indexing.capabilities,
            second.metrics.indexing.capabilities
        );
        Ok(())
    }

    #[test]
    fn source_byte_budgets_skip_only_excess_files() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(repository.path().join("a.rs"), "pub fn retained() {}\n")?;
        fs::write(
            repository.path().join("b.rs"),
            "pub fn source_that_is_deliberately_too_large_for_this_test() {}\n",
        )?;
        let per_file = IndexBudgets {
            max_source_file_bytes: 32,
            ..IndexBudgets::default()
        };
        let report = index_repository_with_options(
            repository.path(),
            IndexOptions::new(per_file, IndexCancellation::default())?,
        )?;
        assert_eq!(report.graph.file_count(), 1);
        assert_eq!(report.graph.resolve_name("retained").len(), 1);
        assert!(report.metrics.indexing.degradations.iter().any(|item| {
            item.cause == IndexBudgetKind::SourceFileBytes
                && item
                    .affected_capabilities
                    .contains(&IndexCapability::FileInventory)
        }));

        let workspace = IndexBudgets {
            max_source_file_bytes: 32,
            max_workspace_source_bytes: 32,
            ..IndexBudgets::default()
        };
        fs::write(repository.path().join("b.rs"), "pub fn second() {}\n")?;
        let report = index_repository_with_options(
            repository.path(),
            IndexOptions::new(workspace, IndexCancellation::default())?,
        )?;
        assert_eq!(report.graph.file_count(), 1);
        assert!(report.metrics.indexing.degradations.iter().any(|item| {
            item.cause == IndexBudgetKind::WorkspaceSourceBytes
                && item
                    .affected_capabilities
                    .contains(&IndexCapability::FileInventory)
        }));
        Ok(())
    }

    #[test]
    fn edge_and_call_site_budgets_stop_before_graph_allocation() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(
            repository.path().join("service.php"),
            "<?php class Service { function one() { missingOne(); missingTwo(); } function two() {} }\n",
        )?;
        let budgets = IndexBudgets {
            max_edges: 1,
            max_call_sites: 1,
            ..IndexBudgets::default()
        };
        let report = index_repository_with_options(
            repository.path(),
            IndexOptions::new(budgets, IndexCancellation::default())?,
        )?;
        assert!(report.graph.edge_count() <= 1);
        assert!(report.graph.call_site_count() <= 1);
        assert!(report.metrics.indexing.coverage.omitted_edges > 0);
        assert!(report.metrics.indexing.coverage.omitted_call_sites > 0);
        assert!(report.metrics.indexing.degradations.iter().any(|item| {
            item.cause == IndexBudgetKind::Edges
                && item.affected_capabilities == [IndexCapability::Relationships]
        }));
        assert!(report.metrics.indexing.degradations.iter().any(|item| {
            item.cause == IndexBudgetKind::CallSites
                && item.affected_capabilities == [IndexCapability::CallSites]
        }));
        report.graph.validate_consistency()?;
        Ok(())
    }

    #[test]
    fn graph_budgets_keep_file_and_text_queries_useful() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(
            repository.path().join("lib.rs"),
            "pub fn alpha() { beta(); }\npub fn beta() {}\n",
        )?;
        let budgets = IndexBudgets {
            max_symbols: 1,
            max_edges: 1,
            max_call_sites: 1,
            ..IndexBudgets::default()
        };
        let report = index_repository_with_options(
            repository.path(),
            IndexOptions::new(budgets, IndexCancellation::default())?,
        )?;
        let identity = chakra_git::resolve_workspace_identity(repository.path())?;
        let engine = chakra_engine::WorkspaceEngine::new(identity);
        let mut update = engine.begin_update();
        update.replace_graph(report.graph);
        update.set_indexing(report.metrics.indexing.clone());
        update.set_status(WorkspaceStatus::Degraded);
        update.set_freshness(Freshness::Fresh);
        engine.publish(update)?;

        let repo_map = engine.repo_map(RepoMapRequest::default())?;
        assert_eq!(repo_map.data.files.len(), 1);
        let search = engine.search(SearchRequest {
            query: "beta".to_owned(),
            ..SearchRequest::default()
        })?;
        assert!(!search.data.matches.is_empty());
        let symbols = engine.symbol_search(SymbolSearchRequest {
            query: "alpha".to_owned(),
            ..SymbolSearchRequest::default()
        })?;
        assert_eq!(symbols.data.candidates.len(), 1);
        assert!(symbols.indexing.is_degraded());
        assert!(symbols.indexing.capabilities.iter().any(|coverage| {
            coverage.capability == IndexCapability::Relationships && !coverage.complete
        }));
        assert!(symbols.indexing.degradations.iter().any(|degradation| {
            degradation.cause == IndexBudgetKind::Symbols
                && degradation.affected_capabilities
                    == [IndexCapability::Relationships, IndexCapability::CallSites]
        }));
        Ok(())
    }

    #[test]
    fn cancellation_stops_before_parsing() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(repository.path().join("lib.rs"), "pub fn alpha() {}\n")?;
        let cancellation = IndexCancellation::default();
        cancellation.cancel();
        let result = index_repository_with_options(
            repository.path(),
            IndexOptions::new(IndexBudgets::default(), cancellation)?,
        );
        assert!(matches!(result, Err(WorkspaceIndexError::Cancelled)));
        Ok(())
    }

    #[test]
    fn non_utf8_sources_are_skipped_without_aborting_the_index() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(
            repository.path().join("lib.rs"),
            "pub fn retained_rust() {}\n",
        )?;
        fs::write(
            repository.path().join("service.php"),
            "<?php function retained_php(): void {}\n",
        )?;
        // Latin-1 bytes that are not valid UTF-8.
        fs::write(
            repository.path().join("legacy.php"),
            b"<?php // caf\xe9\r\nfunction lost_php(): void {}\n",
        )?;
        fs::write(
            repository.path().join("legacy.rs"),
            b"// caf\xe9\r\npub fn lost_rust() {}\n",
        )?;

        let report = index_repository(repository.path())?;
        assert_eq!(report.metrics.indexing.coverage.discovered_files, 4);
        assert_eq!(report.metrics.indexing.coverage.indexed_files, 2);
        assert_eq!(report.metrics.indexing.coverage.skipped_files, 2);
        assert_eq!(report.metrics.indexing.coverage.unreadable_files, 2);
        assert_eq!(report.graph.resolve_name("retained_rust").len(), 1);
        assert_eq!(report.graph.resolve_name("retained_php").len(), 1);
        assert!(report.graph.resolve_name("lost_rust").is_empty());
        assert!(report.graph.resolve_name("lost_php").is_empty());
        report.graph.validate_consistency()?;

        let scan = report.syntax_index.scan_repository(repository.path())?;
        assert_eq!(scan.unreadable_files, 2);
        assert_eq!(
            scan.unreadable_paths,
            vec![
                RepoRelativePath::new("legacy.php")?,
                RepoRelativePath::new("legacy.rs")?,
            ]
        );
        Ok(())
    }

    #[test]
    fn sources_vanishing_between_inventory_and_read_are_skipped() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(repository.path().join("lib.rs"), "pub fn retained() {}\n")?;
        let inventory = chakra_git::WorkspaceInventory {
            sources: vec![
                RepoRelativePath::new("lib.rs")?,
                RepoRelativePath::new("vanished.rs")?,
            ],
            metadata_inputs: vec![RepoRelativePath::new("missing/Cargo.toml")?],
        };
        let operation = OperationContext::from_cancellation(IndexCancellation::default());
        let scan = scan_discovered_sources_with_options(
            repository.path(),
            &IndexOptions::default(),
            &inventory,
            Duration::ZERO,
            &mut FilesystemSourceLoader,
            &operation,
        )?;
        assert_eq!(scan.discovered_files, 2);
        assert_eq!(scan.indexed_files, 1);
        assert_eq!(scan.unreadable_files, 2);
        assert_eq!(
            scan.unreadable_paths,
            vec![
                RepoRelativePath::new("vanished.rs")?,
                RepoRelativePath::new("missing/Cargo.toml")?,
            ]
        );
        assert_eq!(scan.sources.file_count(Language::Rust), 1);
        Ok(())
    }

    #[test]
    fn per_file_read_failures_skip_but_other_loader_errors_abort() -> Result<(), Box<dyn Error>> {
        struct FlakyLoader {
            failing: RepoRelativePath,
            error: fn(&RepoRelativePath) -> WorkspaceIndexError,
        }

        impl WorkspaceSourceLoader for FlakyLoader {
            fn observe(&mut self, _path: &RepoRelativePath, _metadata: &fs::Metadata) {}

            fn observe_metadata(&mut self, _path: &RepoRelativePath, _metadata: &fs::Metadata) {}

            fn load(
                &mut self,
                absolute: &Path,
                path: &RepoRelativePath,
                metadata: &fs::Metadata,
                max_bytes: u64,
            ) -> Result<Arc<str>, WorkspaceIndexError> {
                if *path == self.failing {
                    return Err((self.error)(path));
                }
                FilesystemSourceLoader.load(absolute, path, metadata, max_bytes)
            }
        }

        let repository = repository()?;
        fs::write(repository.path().join("lib.rs"), "pub fn retained() {}\n")?;
        fs::write(repository.path().join("broken.rs"), "pub fn lost() {}\n")?;
        let inventory = chakra_git::WorkspaceInventory {
            sources: vec![
                RepoRelativePath::new("broken.rs")?,
                RepoRelativePath::new("lib.rs")?,
            ],
            metadata_inputs: Vec::new(),
        };

        let mut read_failure = FlakyLoader {
            failing: RepoRelativePath::new("broken.rs")?,
            error: |path| WorkspaceIndexError::Read {
                path: path.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream did not contain valid UTF-8",
                ),
            },
        };
        let operation = OperationContext::from_cancellation(IndexCancellation::default());
        let scan = scan_discovered_sources_with_options(
            repository.path(),
            &IndexOptions::default(),
            &inventory,
            Duration::ZERO,
            &mut read_failure,
            &operation,
        )?;
        assert_eq!(scan.indexed_files, 1);
        assert_eq!(scan.unreadable_files, 1);
        assert_eq!(
            scan.unreadable_paths,
            vec![RepoRelativePath::new("broken.rs")?]
        );

        let mut update_failure = FlakyLoader {
            failing: RepoRelativePath::new("broken.rs")?,
            error: |path| {
                WorkspaceIndexError::Update(format!("source `{path}` changed while reading"))
            },
        };
        let result = scan_discovered_sources_with_options(
            repository.path(),
            &IndexOptions::default(),
            &inventory,
            Duration::ZERO,
            &mut update_failure,
            &operation,
        );
        assert!(matches!(result, Err(WorkspaceIndexError::Update(_))));
        Ok(())
    }
}
