//! Bounded composition of independently parsed language indexes.

mod resources;
mod scan;
mod status;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chakra_domain::indexing::{
    IndexBudgetError, IndexBudgets, IndexCancellation, IndexDegradation, IndexMemoryMetrics,
    IndexPhase, IndexPhaseMeasurement, IndexPublicationMetrics, IndexSchedulingMetrics,
    IndexingStatus,
};
use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;
use chakra_domain::project::{ProjectModel, ProjectModelImpact, ProjectUnitChangeCounts};
use chakra_domain::symbol::Language;
use chakra_engine::{
    ConsistencyError, GraphBuildLimits, GraphBuildReport, GraphError, ProviderInput, SymbolGraph,
};
use thiserror::Error;
use tracing::{info, warn};

use crate::adapter::{
    AdapterFrameworkMetrics, AdapterReconcileMetrics, DependencyEvidence, LanguageSources,
    SyntaxLanguageAdapter, default_adapters,
};

use resources::{process_cpu_micros, process_peak_rss_bytes, process_rss_bytes};
use scan::check_cancelled;
use status::{
    IndexingParts, LanguageIndexingFacts, build_indexing_status, indexing_semantically_equal,
};

pub(crate) use scan::{WorkspaceSourceLoader, scan_discovered_sources_with_options};
pub use scan::{scan_repository_sources, scan_repository_sources_with_options};

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
    /// Typed Cargo/Composer project model built from this exact scan's
    /// manifest evidence (issue #41).
    pub project_model: ProjectModel,
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
        })
    }
}

/// Typed invalidation picture of one reconciliation's external manifest /
/// config / project-scope inputs (issue #40). All counters are per
/// reconciliation; the live index accumulates them for diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DependencyImpactMetrics {
    /// Project units whose own manifest evidence changed.
    pub impacted_units: u64,
    /// Unchanged units declaring dependency edges on an impacted unit.
    pub impacted_dependents: u64,
    /// Manifests whose recorded probe/parse issue state changed.
    pub manifest_issue_changes: u64,
    /// Per-reason unit change counts.
    pub unit_changes: ProjectUnitChangeCounts,
}

impl DependencyImpactMetrics {
    fn from_impact(impact: &ProjectModelImpact) -> Self {
        Self {
            impacted_units: impact.changes.len() as u64 + impact.changes_omitted,
            impacted_dependents: impact.dependents.len() as u64 + impact.dependents_omitted,
            manifest_issue_changes: impact.manifest_issue_changes.len() as u64
                + impact.manifest_issue_changes_omitted,
            unit_changes: impact.counts(),
        }
    }

    /// Adds another reconciliation's impact counters into `self`.
    pub fn accumulate(&mut self, other: &Self) {
        self.impacted_units = self.impacted_units.saturating_add(other.impacted_units);
        self.impacted_dependents = self
            .impacted_dependents
            .saturating_add(other.impacted_dependents);
        self.manifest_issue_changes = self
            .manifest_issue_changes
            .saturating_add(other.manifest_issue_changes);
        let (left, right) = (&mut self.unit_changes, &other.unit_changes);
        left.added = left.added.saturating_add(right.added);
        left.removed = left.removed.saturating_add(right.removed);
        left.definition_changed = left
            .definition_changed
            .saturating_add(right.definition_changed);
        left.source_roots_changed = left
            .source_roots_changed
            .saturating_add(right.source_roots_changed);
        left.dependencies_changed = left
            .dependencies_changed
            .saturating_add(right.dependencies_changed);
        left.membership_changed = left
            .membership_changed
            .saturating_add(right.membership_changed);
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
    /// Retained files whose manifest-derived metadata record was replaced
    /// without a source reparse (issue #40).
    pub metadata_files_recomputed: u64,
    pub framework_files_reparsed: u64,
    pub framework_relationship_files_recomputed: u64,
    pub framework_truncated_files: u64,
    /// Framework-enrichment configuration toggles applied (issue #40).
    pub framework_config_changes: u64,
    /// Typed external-input invalidation picture (issue #40).
    pub dependency_impact: DependencyImpactMetrics,
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
    /// The scan's typed project model when it differs from the currently
    /// published one (issue #41). `None` means the model is unchanged.
    pub project_model: Option<ProjectModel>,
    /// Typed record of which project units the scan's manifest/config diff
    /// invalidated, and which units depend on them (issue #40). `None` when
    /// the project model did not change or the diff is empty.
    pub dependency_impact: Option<ProjectModelImpact>,
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
    /// Typed project model built from the cold scan's manifest evidence
    /// (issue #41).
    pub project_model: ProjectModel,
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
    project_model: ProjectModel,
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

    /// Typed project model captured by this index's latest scan (issue #41).
    pub fn project_model(&self) -> &ProjectModel {
        &self.project_model
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
        // Typed dependency tracking (issue #40): diff the scan's project
        // model against the published one before adapters reconcile, so the
        // invalidation record, framework evidence, and metrics all describe
        // the same manifest/config change.
        let project_model_changed = self.project_model != scan.project_model;
        let dependency_impact = if project_model_changed {
            let impact = scan.project_model.impact_since(&self.project_model);
            (!impact.is_empty()).then_some(impact)
        } else {
            None
        };
        let mut reconciled = Vec::with_capacity(self.adapters.len());
        for (state, limits) in self.adapters.iter().zip(limits.iter().copied()) {
            let language = state.adapter.language();
            let dependencies = DependencyEvidence {
                framework_detected: (language == Language::Php)
                    .then(|| chakra_language_php::laravel_detected_from_model(&scan.project_model)),
            };
            let sources = scan.sources.take(language);
            reconciled.push(
                state
                    .adapter
                    .reconcile(sources, limits, dependencies, cancellation)
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
        if let Some(impact) = &dependency_impact {
            metrics.dependency_impact = DependencyImpactMetrics::from_impact(impact);
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
        if !graph_changed && !indexing_changed && !provider_inputs_changed && !project_model_changed
        {
            return Ok(ReconcileReport {
                graph: None,
                metrics,
                next_index: None,
                indexing: self.indexing.clone(),
                project_model: None,
                dependency_impact: None,
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
            provider_inputs: scan.provider_inputs.clone(),
            project_model: scan.project_model.clone(),
        };
        Ok(ReconcileReport {
            graph,
            metrics,
            next_index: Some(next),
            indexing,
            project_model: project_model_changed.then(|| scan.project_model.clone()),
            dependency_impact,
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
    combined.metadata_files_recomputed += metrics.metadata_files_recomputed;
    combined.framework_files_reparsed += metrics.framework_files_reparsed;
    combined.framework_relationship_files_recomputed +=
        metrics.framework_relationship_files_recomputed;
    combined.framework_truncated_files += metrics.framework_truncated_files;
    combined.framework_config_changes += metrics.framework_config_changes;
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
                    &repository_root,
                    &options.cancellation,
                )
                .map_err(|source| WorkspaceIndexError::Adapter {
                    language,
                    source: Box::new(source),
                })?,
        );
        rss_peak = max_option(rss_peak, process_rss_bytes());
    }
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
    let syntax_index = WorkspaceSyntaxIndex {
        adapters: built_adapters
            .into_iter()
            .zip(limits.iter().copied())
            .map(|(adapter, limits)| WorkspaceAdapterState { adapter, limits })
            .collect(),
        budgets,
        indexing: indexing.clone(),
        provider_inputs: scan.provider_inputs.clone(),
        project_model: scan.project_model.clone(),
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
        project_model: syntax_index.project_model.clone(),
        syntax_index,
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

fn max_option(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::process::Command;

    use chakra_domain::indexing::{IndexBudgetKind, IndexCapability};
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
    fn phase_measurements_carry_the_language_of_their_adapter() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(
            repository.path().join("main.go"),
            "package main\n\nfunc main() {}\n",
        )?;
        fs::write(repository.path().join("tool.py"), "def tool():\n    pass\n")?;

        let report = index_repository(repository.path())?;
        let parse_languages: Vec<_> = report
            .metrics
            .indexing
            .phases
            .iter()
            .filter(|phase| phase.phase == IndexPhase::ParseExtraction)
            .map(|phase| phase.language)
            .collect();
        assert!(
            parse_languages.contains(&Some(Language::Go)),
            "Go parse phase missing or misattributed: {parse_languages:?}"
        );
        assert!(
            parse_languages.contains(&Some(Language::Python)),
            "Python parse phase missing or misattributed: {parse_languages:?}"
        );
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
}
