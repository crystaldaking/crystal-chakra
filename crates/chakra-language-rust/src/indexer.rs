//! Deterministic Rust syntax indexing with reusable per-file facts.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use chakra_domain::indexing::{
    IndexCancellation, IndexPhase, IndexPhaseMeasurement, IndexPublicationMetrics,
};
use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::source::SourceMetadata;
use chakra_domain::symbol::{CallResolution, Edge, EdgeKind, Language, SymbolKind};
use chakra_engine::{
    BoundedGraphBuilder, CallSiteInput, ConsistencyError, GraphBuildLimits, GraphBuildReport,
    GraphError, SymbolGraph,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, info_span};

#[cfg(unix)]
use nix::sys::resource::{UsageWho, getrusage};
#[cfg(unix)]
use nix::sys::time::TimeValLike;

use crate::discovery::{DiscoveryError, discover_rust_sources, resolve_repository_root};
use crate::parser::{ParsedFile, RustParser, SymbolDraft};

const MAX_SOURCE_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REPOSITORY_SOURCE_BYTES: usize = 256 * 1024 * 1024;
const PHASE_RESOURCE_SAMPLE_THRESHOLD: u64 = 32;

#[derive(Debug, Clone, Copy)]
struct PhaseTimer {
    wall: Instant,
    cpu_micros: Option<u64>,
}

impl PhaseTimer {
    fn start() -> Self {
        Self {
            wall: Instant::now(),
            cpu_micros: process_cpu_micros(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ParseSchedule {
    effective_workers: u64,
    peak_active_workers: u64,
    peak_queue_depth: u64,
}

/// Measurements captured during a deterministic initial syntax index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexMetrics {
    pub discovered_files: u64,
    pub parsed_files: u64,
    pub syntax_error_files: u64,
    /// Legacy eager-resolution truncation counter. Lazy candidates are now
    /// bounded at query time, so new indexes report zero here.
    pub truncated_call_sites: u64,
    pub symbols: u64,
    pub edges: u64,
    pub call_sites: u64,
    pub ambiguous_call_sites: u64,
    pub unresolved_call_sites: u64,
    pub elapsed: Duration,
}

/// Work performed by one content reconciliation.
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
    pub syntax_error_files: u64,
    pub truncated_call_sites: u64,
    pub publication: IndexPublicationMetrics,
}

/// Result of reconciling exact worktree contents with the reusable index.
#[derive(Debug)]
pub struct ReconcileReport {
    /// Present when source content or manifest-derived metadata changed and
    /// a new graph is ready.
    pub graph: Option<SymbolGraph>,
    pub metrics: ReconcileMetrics,
    pub next_index: Option<RustSyntaxIndex>,
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

/// Complete private initial index, ready for atomic publication by the
/// workspace engine owner. `syntax_index` remains private to the live owner
/// after the first graph is published.
#[derive(Debug)]
pub struct IndexReport {
    pub repository_root: PathBuf,
    pub graph: SymbolGraph,
    pub metrics: IndexMetrics,
    pub syntax_index: RustSyntaxIndex,
}

/// Latest Rust source text plus role/package metadata from the same scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RustSources {
    pub files: BTreeMap<RepoRelativePath, Arc<str>>,
    pub metadata: BTreeMap<RepoRelativePath, SourceMetadata>,
}

impl RustSources {
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Failure to discover, read, parse, or validate the Rust syntax index.
#[derive(Debug, Error)]
pub enum RustIndexError {
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    #[error("failed to read Rust source {path}: {source}")]
    Read {
        path: RepoRelativePath,
        #[source]
        source: io::Error,
    },
    #[error("Rust source {path} exceeds the {limit}-byte indexing budget")]
    SourceTooLarge {
        path: RepoRelativePath,
        limit: usize,
    },
    #[error("indexed Rust sources exceed the {limit}-byte repository budget")]
    RepositoryTooLarge { limit: usize },
    #[error("failed to parse Rust source: {0}")]
    Parse(String),
    #[error("failed to start a bounded Rust parser worker: {0}")]
    WorkerSpawn(#[source] io::Error),
    #[error("a bounded Rust parser worker panicked")]
    WorkerPanicked,
    #[error("Rust syntax index update failed: {0}")]
    Update(String),
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error("constructed syntax graph is inconsistent: {0}")]
    Consistency(#[from] ConsistencyError),
    #[error("Rust syntax indexing was cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct SymbolAddress {
    path: RepoRelativePath,
    index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum DependencyKey {
    Exact(String, SymbolKind),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelationshipEdge {
    kind: EdgeKind,
    from: SymbolAddress,
    to: SymbolAddress,
    provenance: Provenance,
    precision: Precision,
    location: Option<SourceRange>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RelationshipContribution {
    dependencies: HashSet<DependencyKey>,
    edges: Vec<RelationshipEdge>,
    omitted_edges: u64,
}

impl RelationshipContribution {
    fn push_edge(&mut self, edge: RelationshipEdge, limit: u64) {
        if self.edges.len() as u64 >= limit {
            self.omitted_edges = self.omitted_edges.saturating_add(1);
        } else {
            self.edges.push(edge);
        }
    }
}

#[derive(Debug)]
struct SymbolCatalog<'a> {
    files: &'a BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
    exact: HashMap<(String, SymbolKind), Vec<SymbolAddress>>,
}

impl<'a> SymbolCatalog<'a> {
    fn new(files: &'a BTreeMap<RepoRelativePath, Arc<ParsedFile>>) -> Self {
        let mut exact: HashMap<(String, SymbolKind), Vec<SymbolAddress>> = HashMap::new();
        for (path, file) in files {
            for (index, symbol) in file.symbols.iter().enumerate() {
                let address = SymbolAddress {
                    path: path.clone(),
                    index,
                };
                exact
                    .entry((symbol.key.qualified_name.clone(), symbol.key.kind))
                    .or_default()
                    .push(address.clone());
            }
        }
        for addresses in exact.values_mut() {
            addresses.sort();
        }
        Self { files, exact }
    }

    fn symbol(&self, address: &SymbolAddress) -> Option<&SymbolDraft> {
        self.files.get(&address.path)?.symbols.get(address.index)
    }

    fn unique_exact(&self, qualified_name: &str, kind: SymbolKind) -> Option<SymbolAddress> {
        unique(self.exact.get(&(qualified_name.to_owned(), kind)))
    }
}

/// Reusable per-file syntax facts and per-owner relationship contributions.
///
/// Entity ids are intentionally absent here: they are revision-scoped and
/// assigned only while a complete immutable graph is materialized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustSyntaxIndex {
    files: BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
    metadata: BTreeMap<RepoRelativePath, SourceMetadata>,
    relationships: BTreeMap<RepoRelativePath, Arc<RelationshipContribution>>,
    graph_limits: GraphBuildLimits,
    graph: SymbolGraph,
    graph_report: GraphBuildReport,
}

impl Default for RustSyntaxIndex {
    fn default() -> Self {
        Self {
            files: BTreeMap::new(),
            metadata: BTreeMap::new(),
            relationships: BTreeMap::new(),
            graph_limits: GraphBuildLimits::UNLIMITED,
            graph: SymbolGraph::new(),
            graph_report: GraphBuildReport::default(),
        }
    }
}

impl RustSyntaxIndex {
    pub fn from_sources(
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
    ) -> Result<(Self, SymbolGraph), RustIndexError> {
        let cancellation = IndexCancellation::default();
        let (index, graph, _) =
            Self::from_sources_bounded(sources, GraphBuildLimits::UNLIMITED, &cancellation)?;
        Ok((index, graph))
    }

    pub fn from_sources_bounded(
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), RustIndexError> {
        Self::from_sources_scheduled(sources, graph_limits, 1, usize::MAX, cancellation)
    }

    /// Builds an initial index with bounded parser workers. Each worker owns
    /// one Tree-sitter parser; parsed files are reduced in lexical path order.
    pub fn from_sources_scheduled(
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        cancellation: &IndexCancellation,
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), RustIndexError> {
        let metadata = sources
            .keys()
            .map(|path| (path.clone(), SourceMetadata::path_fallback(path)))
            .collect();
        Self::from_classified_sources_scheduled(
            RustSources {
                files: sources,
                metadata,
            },
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            cancellation,
        )
    }

    pub fn from_classified_sources(
        sources: RustSources,
    ) -> Result<(Self, SymbolGraph), RustIndexError> {
        let cancellation = IndexCancellation::default();
        let (index, graph, _) = Self::from_classified_sources_scheduled(
            sources,
            GraphBuildLimits::UNLIMITED,
            1,
            usize::MAX,
            &cancellation,
        )?;
        Ok((index, graph))
    }

    /// Builds a bounded classified index while preserving the source scan's
    /// role and package metadata.
    pub fn from_classified_sources_scheduled(
        sources: RustSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        cancellation: &IndexCancellation,
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), RustIndexError> {
        let RustSources {
            files: sources,
            metadata,
        } = sources;
        let metadata = normalized_metadata(&sources, metadata);
        check_cancelled(cancellation)?;
        let parse_started = PhaseTimer::start();
        let (files, parse_schedule) = parse_sources_scheduled(
            sources,
            worker_limit.max(1),
            parallel_file_threshold,
            cancellation,
        )?;
        let parsed_source_bytes = files.values().fold(0_u64, |bytes, file| {
            bytes.saturating_add(file.source.len() as u64)
        });
        let parse_phase = measured_phase(
            IndexPhase::ParseExtraction,
            parse_started,
            files.len() as u64,
            parsed_source_bytes,
            parse_schedule.effective_workers,
            parse_schedule.peak_active_workers,
            parse_schedule.peak_queue_depth,
        );
        check_cancelled(cancellation)?;
        let catalog_started = PhaseTimer::start();
        let catalog = SymbolCatalog::new(&files);
        let catalog_phase = measured_phase(
            IndexPhase::SymbolCatalog,
            catalog_started,
            files.values().map(|file| file.symbols.len() as u64).sum(),
            0,
            1,
            1,
            0,
        );
        let relationships_started = PhaseTimer::start();
        let relationships =
            build_all_relationships(&files, &catalog, graph_limits.max_edges, cancellation)?;
        let relationship_items = relationships.values().fold(0_u64, |count, contribution| {
            count
                .saturating_add(contribution.edges.len() as u64)
                .saturating_add(contribution.omitted_edges)
        });
        let relationships_phase = measured_phase(
            IndexPhase::Relationships,
            relationships_started,
            relationship_items,
            0,
            1,
            1,
            0,
        );
        let mut index = Self {
            files,
            metadata,
            relationships,
            graph_limits,
            graph: SymbolGraph::new(),
            graph_report: GraphBuildReport::default(),
        };
        let facts = index.fact_counts();
        let materialize_started = PhaseTimer::start();
        let (graph, graph_report) = index.materialize_graph_bounded(cancellation)?;
        index.graph = graph.clone();
        index.graph_report = graph_report;
        let materialize_phase = measured_phase(
            IndexPhase::GraphMaterialization,
            materialize_started,
            graph_report
                .retained_symbols
                .saturating_add(graph_report.retained_edges)
                .saturating_add(graph_report.retained_call_sites),
            0,
            1,
            1,
            0,
        );
        let phases = vec![
            parse_phase,
            catalog_phase,
            relationships_phase,
            materialize_phase,
        ];
        Ok((
            index,
            graph,
            LanguageBuildMetrics {
                facts,
                graph: graph_report,
                phases,
            },
        ))
    }

    pub fn paths(&self) -> Vec<RepoRelativePath> {
        self.files.keys().cloned().collect()
    }

    pub fn graph(&self) -> &SymbolGraph {
        &self.graph
    }

    pub fn graph_report(&self) -> GraphBuildReport {
        self.graph_report
    }

    /// Reconciles exact discovered source contents. Unchanged text is not
    /// reparsed. Changed files and relationship owners are prepared in a
    /// private copy and become the reusable state only after graph validation.
    pub fn reconcile_sources(
        &self,
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
    ) -> Result<ReconcileReport, RustIndexError> {
        self.reconcile_sources_bounded(sources, self.graph_limits, &IndexCancellation::default())
    }

    pub fn reconcile_sources_bounded(
        &self,
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<ReconcileReport, RustIndexError> {
        let metadata = sources
            .keys()
            .map(|path| (path.clone(), SourceMetadata::path_fallback(path)))
            .collect();
        self.reconcile_classified_sources_bounded(
            RustSources {
                files: sources,
                metadata,
            },
            graph_limits,
            cancellation,
        )
    }

    pub fn reconcile_classified_sources(
        &self,
        sources: RustSources,
    ) -> Result<ReconcileReport, RustIndexError> {
        self.reconcile_classified_sources_bounded(
            sources,
            self.graph_limits,
            &IndexCancellation::default(),
        )
    }

    pub fn reconcile_classified_sources_bounded(
        &self,
        sources: RustSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<ReconcileReport, RustIndexError> {
        let RustSources {
            files: sources,
            metadata,
        } = sources;
        let metadata = normalized_metadata(&sources, metadata);
        check_cancelled(cancellation)?;
        let limits_changed = graph_limits != self.graph_limits;
        let mut metrics = ReconcileMetrics {
            scanned_files: sources.len() as u64,
            ..ReconcileMetrics::default()
        };
        let mut changed_paths = BTreeSet::new();
        for (path, source) in &sources {
            match self.files.get(path) {
                Some(current) if current.source.as_ref() == source.as_ref() => {
                    metrics.unchanged_files += 1;
                }
                Some(_) => {
                    metrics.modified_files += 1;
                    changed_paths.insert(path.clone());
                }
                None => {
                    metrics.created_files += 1;
                    changed_paths.insert(path.clone());
                }
            }
        }
        for path in self.files.keys() {
            if !sources.contains_key(path) {
                metrics.deleted_files += 1;
                changed_paths.insert(path.clone());
            }
        }
        // Manifest-derived metadata is tracked per retained path so an
        // external manifest/config edit invalidates exactly the files whose
        // derived metadata changed instead of the whole language (issue #40).
        let metadata_changed_paths: BTreeSet<RepoRelativePath> = sources
            .keys()
            .filter(|path| self.files.contains_key(*path))
            .filter(|path| self.metadata.get(*path) != metadata.get(*path))
            .cloned()
            .collect();
        metrics.metadata_files_recomputed = metadata_changed_paths.len() as u64;
        if changed_paths.is_empty() && !limits_changed && metadata_changed_paths.is_empty() {
            metrics.syntax_error_files = self.syntax_error_files();
            metrics.truncated_call_sites = self.truncated_call_sites();
            metrics.publication = self.reuse_all_publication();
            return Ok(ReconcileReport {
                graph: None,
                metrics,
                next_index: None,
                build_metrics: None,
            });
        }

        let mut next_files = self.files.clone();
        let parse_started = Instant::now();
        let mut parser = RustParser::new().map_err(parse_error)?;
        for path in &changed_paths {
            check_cancelled(cancellation)?;
            match sources.get(path) {
                Some(source) => {
                    let parsed = parser
                        .parse(path.clone(), source.clone())
                        .map_err(parse_error)?;
                    next_files.insert(path.clone(), Arc::new(parsed));
                    metrics.reparsed_files += 1;
                }
                None => {
                    next_files.remove(path);
                }
            }
        }
        let parse_elapsed = parse_started.elapsed();
        check_cancelled(cancellation)?;

        let mut stable_symbol_paths = BTreeSet::new();
        let mut unchanged_fact_paths = BTreeSet::new();
        let mut changed_dependencies = HashSet::new();
        let mut changed_callables = HashSet::new();
        for path in &changed_paths {
            match (self.files.get(path), next_files.get(path)) {
                (Some(previous), Some(next)) if symbol_keys_equal(previous, next) => {
                    stable_symbol_paths.insert(path.clone());
                    if syntax_facts_equal(previous, next) {
                        unchanged_fact_paths.insert(path.clone());
                    }
                }
                (previous, next) => {
                    if let Some(previous) = previous {
                        changed_dependencies.extend(exported_dependencies(previous));
                        changed_callables.extend(exported_callables(previous));
                    }
                    if let Some(next) = next {
                        changed_dependencies.extend(exported_dependencies(next));
                        changed_callables.extend(exported_callables(next));
                    }
                }
            }
        }

        let mut affected_owners: BTreeSet<_> = changed_paths
            .difference(&unchanged_fact_paths)
            .cloned()
            .collect();
        affected_owners.extend(
            self.relationships
                .iter()
                .filter(|(_, contribution)| {
                    contribution
                        .dependencies
                        .iter()
                        .any(|key| changed_dependencies.contains(key))
                })
                .map(|(path, _)| path.clone()),
        );
        let mut affected_call_owners: BTreeSet<_> = changed_paths
            .difference(&unchanged_fact_paths)
            .cloned()
            .collect();
        affected_call_owners.extend(
            self.files
                .iter()
                .filter(|(_, file)| {
                    file.calls.iter().any(|call| {
                        changed_callables
                            .contains(&callable_dependency(call.target_kind, &call.name))
                    })
                })
                .map(|(path, _)| path.clone()),
        );

        let catalog_started = Instant::now();
        let catalog = SymbolCatalog::new(&next_files);
        let catalog_elapsed = catalog_started.elapsed();
        check_cancelled(cancellation)?;
        let relationships_started = Instant::now();
        let mut next_relationships = if limits_changed || self.omitted_relationship_edges() > 0 {
            metrics.relationship_files_recomputed = next_files.len() as u64;
            build_all_relationships(&next_files, &catalog, graph_limits.max_edges, cancellation)?
        } else {
            let mut relationships = self.relationships.clone();
            for path in &affected_owners {
                match next_files.get(path) {
                    Some(file) => {
                        relationships.insert(
                            path.clone(),
                            Arc::new(relationships_for_file(
                                path,
                                file,
                                &catalog,
                                graph_limits.max_edges,
                            )),
                        );
                        metrics.relationship_files_recomputed += 1;
                    }
                    None => {
                        relationships.remove(path);
                    }
                }
            }
            relationships
        };
        let retained_edges: u64 = next_relationships
            .values()
            .map(|contribution| contribution.edges.len() as u64)
            .sum();
        let omitted_edges: u64 = next_relationships
            .values()
            .map(|contribution| contribution.omitted_edges)
            .sum();
        if retained_edges > graph_limits.max_edges || omitted_edges > 0 {
            next_relationships = build_all_relationships(
                &next_files,
                &catalog,
                graph_limits.max_edges,
                cancellation,
            )?;
            metrics.relationship_files_recomputed = next_files.len() as u64;
        }
        let relationships_elapsed = relationships_started.elapsed();

        let next = Self {
            files: next_files,
            metadata,
            relationships: next_relationships,
            graph_limits,
            graph: SymbolGraph::new(),
            graph_report: GraphBuildReport::default(),
        };
        let materialize_started = Instant::now();
        let next_facts = next.fact_counts();
        let complete_previous = self.graph_report.omitted_symbols == 0
            && self.graph_report.omitted_edges == 0
            && self.graph_report.omitted_call_sites == 0;
        let delta_fits = next_facts.symbols <= graph_limits.max_symbols
            && next_facts.call_sites <= graph_limits.max_call_sites;
        let delta_candidate = !limits_changed && complete_previous && delta_fits;
        let delta = if delta_candidate {
            next.materialize_graph_delta(
                &self.graph,
                &changed_paths,
                &metadata_changed_paths,
                &affected_owners,
                &affected_call_owners,
                &stable_symbol_paths,
                cancellation,
            )?
        } else {
            None
        };
        let structurally_incremental = delta.is_some();
        let (graph, graph_report) = if let Some(delta) = delta {
            delta
        } else {
            next.materialize_graph_bounded(cancellation)?
        };
        let mut next = next;
        next.graph = graph.clone();
        next.graph_report = graph_report;
        metrics.publication = if structurally_incremental {
            next.delta_publication(
                self,
                &changed_paths,
                &affected_owners,
                &affected_call_owners,
                &stable_symbol_paths,
            )
        } else {
            next.full_publication()
        };
        let facts = next.fact_counts();
        let build_metrics = LanguageBuildMetrics {
            facts,
            graph: graph_report,
            phases: vec![
                phase(
                    IndexPhase::ParseExtraction,
                    parse_elapsed,
                    metrics.reparsed_files,
                    changed_paths
                        .iter()
                        .filter_map(|path| next.files.get(path))
                        .map(|file| file.source.len() as u64)
                        .sum(),
                ),
                phase(IndexPhase::SymbolCatalog, catalog_elapsed, facts.symbols, 0),
                phase(
                    IndexPhase::Relationships,
                    relationships_elapsed,
                    metrics.relationship_files_recomputed,
                    0,
                ),
                phase(
                    IndexPhase::GraphMaterialization,
                    materialize_started.elapsed(),
                    if metrics.publication.structurally_incremental {
                        metrics
                            .publication
                            .rebuilt_files
                            .saturating_add(metrics.publication.rebuilt_symbols)
                            .saturating_add(metrics.publication.rebuilt_edges)
                            .saturating_add(metrics.publication.rebuilt_call_sites)
                            .saturating_add(metrics.publication.copied_symbols)
                            .saturating_add(metrics.publication.copied_edges)
                            .saturating_add(metrics.publication.copied_call_sites)
                    } else {
                        graph_report
                            .retained_symbols
                            .saturating_add(graph_report.retained_edges)
                            .saturating_add(graph_report.retained_call_sites)
                    },
                    0,
                ),
            ],
        };
        metrics.syntax_error_files = next.syntax_error_files();
        metrics.truncated_call_sites = next.truncated_call_sites();
        Ok(ReconcileReport {
            graph: Some(graph),
            metrics,
            next_index: Some(next),
            build_metrics: Some(build_metrics),
        })
    }

    fn syntax_error_files(&self) -> u64 {
        self.files.values().filter(|file| file.has_errors).count() as u64
    }

    fn truncated_call_sites(&self) -> u64 {
        0
    }

    fn omitted_relationship_edges(&self) -> u64 {
        self.relationships
            .values()
            .map(|contribution| contribution.omitted_edges)
            .sum()
    }

    pub fn fact_counts(&self) -> SyntaxFactCounts {
        SyntaxFactCounts {
            files: self.files.len() as u64,
            source_bytes: self
                .files
                .values()
                .map(|file| file.source.len() as u64)
                .sum(),
            syntax_error_files: self.syntax_error_files(),
            symbols: self
                .files
                .values()
                .map(|file| file.symbols.len() as u64)
                .sum(),
            relationship_edges: self
                .relationships
                .values()
                .map(|relationships| relationships.edges.len() as u64)
                .sum(),
            omitted_relationship_edges: self.omitted_relationship_edges(),
            call_sites: self
                .files
                .values()
                .map(|file| file.calls.len() as u64)
                .sum(),
        }
    }

    fn materialize_graph_bounded(
        &self,
        cancellation: &IndexCancellation,
    ) -> Result<(SymbolGraph, GraphBuildReport), RustIndexError> {
        let mut graph = BoundedGraphBuilder::new(self.graph_limits);
        let mut ids = BTreeMap::new();
        for (path, file) in &self.files {
            check_cancelled(cancellation)?;
            let metadata = self
                .metadata
                .get(path)
                .cloned()
                .unwrap_or_else(|| SourceMetadata::path_fallback(path));
            graph.add_file_with_metadata_and_diagnostics(
                path.clone(),
                file.source.clone(),
                metadata,
                file.diagnostics.clone(),
                file.diagnostic_count,
            )?;
            for (index, symbol) in file.symbols.iter().enumerate() {
                let id = graph.add_symbol(
                    symbol.key.clone(),
                    symbol.location.clone(),
                    symbol.signature.clone(),
                    Provenance::TreeSitter,
                    Precision::Syntax,
                )?;
                if let Some(id) = id {
                    ids.insert(
                        SymbolAddress {
                            path: path.clone(),
                            index,
                        },
                        id,
                    );
                }
            }
        }
        for (owner, contribution) in &self.relationships {
            check_cancelled(cancellation)?;
            graph.omit_edges_for_edge_budget(contribution.omitted_edges);
            for edge in &contribution.edges {
                let (Some(from), Some(to)) = (ids.get(&edge.from), ids.get(&edge.to)) else {
                    graph.omit_edges_for_symbol_budget(1);
                    continue;
                };
                graph.add_edge_owned_by(
                    owner.clone(),
                    Edge {
                        kind: edge.kind,
                        from: *from,
                        to: *to,
                        provenance: edge.provenance,
                        precision: edge.precision,
                        location: edge.location.clone(),
                    },
                )?;
            }
        }
        for (path, file) in &self.files {
            check_cancelled(cancellation)?;
            if graph.omitted_symbols() > 0 {
                graph.omit_call_sites_for_symbol_budget(file.calls.len() as u64);
                continue;
            }
            for call_site in &file.calls {
                let Some(caller) = ids.get(&SymbolAddress {
                    path: path.clone(),
                    index: call_site.caller,
                }) else {
                    graph.omit_call_sites_for_symbol_budget(1);
                    continue;
                };
                graph.add_call_site(CallSiteInput {
                    caller: *caller,
                    form: call_site.form,
                    target_kind: call_site.target_kind,
                    name: call_site.name.clone(),
                    qualifier: call_site.qualifier.clone(),
                    receiver_type: None,
                    receiver_type_source: None,
                    receiver_hint: call_site.receiver_hint.clone(),
                    location: call_site.location.clone(),
                    provenance: Provenance::TreeSitter,
                    precision: Precision::Syntax,
                })?;
            }
        }
        let (mut graph, report) = graph.finish();
        graph.set_truncated_call_sites(report.omitted_call_sites)?;
        Ok((graph, report))
    }

    #[allow(clippy::too_many_arguments)]
    fn materialize_graph_delta(
        &self,
        previous: &SymbolGraph,
        changed_paths: &BTreeSet<RepoRelativePath>,
        metadata_changed_paths: &BTreeSet<RepoRelativePath>,
        relationship_owners: &BTreeSet<RepoRelativePath>,
        call_owners: &BTreeSet<RepoRelativePath>,
        stable_symbol_paths: &BTreeSet<RepoRelativePath>,
        cancellation: &IndexCancellation,
    ) -> Result<Option<(SymbolGraph, GraphBuildReport)>, RustIndexError> {
        let mut graph = previous.clone();
        for owner in relationship_owners {
            check_cancelled(cancellation)?;
            graph.remove_relationships_in_file(owner)?;
        }
        for owner in call_owners {
            check_cancelled(cancellation)?;
            graph.remove_call_sites_in_file(owner)?;
        }
        for path in changed_paths {
            check_cancelled(cancellation)?;
            if stable_symbol_paths.contains(path) {
                let file = self.files.get(path).ok_or_else(|| {
                    RustIndexError::Update(format!("missing changed file {path}"))
                })?;
                let ids: Vec<_> = graph
                    .symbols_in_file(path)
                    .map(|symbol| symbol.id)
                    .collect();
                graph.replace_file_source_and_diagnostics(
                    path,
                    file.source.clone(),
                    file.diagnostics.clone(),
                    file.diagnostic_count,
                )?;
                for (id, symbol) in ids.into_iter().zip(&file.symbols) {
                    graph.replace_symbol_payload(
                        id,
                        symbol.key.clone(),
                        symbol.location.clone(),
                        symbol.signature.clone(),
                        Provenance::TreeSitter,
                        Precision::Syntax,
                    )?;
                }
            } else {
                graph.remove_file(path)?;
            }
        }
        for path in changed_paths {
            if stable_symbol_paths.contains(path) {
                continue;
            }
            let Some(file) = self.files.get(path) else {
                continue;
            };
            let metadata = self
                .metadata
                .get(path)
                .cloned()
                .unwrap_or_else(|| SourceMetadata::path_fallback(path));
            graph.add_file_with_metadata_and_diagnostics(
                path.clone(),
                file.source.clone(),
                metadata,
                file.diagnostics.clone(),
                file.diagnostic_count,
            )?;
            for symbol in &file.symbols {
                graph.add_symbol(
                    symbol.key.clone(),
                    symbol.location.clone(),
                    symbol.signature.clone(),
                    Provenance::TreeSitter,
                    Precision::Syntax,
                )?;
            }
        }
        // Manifest/config-driven metadata changes update exactly the affected
        // file records in place: symbol ids, edges, and call sites are
        // preserved because metadata never participates in identity.
        for path in metadata_changed_paths {
            check_cancelled(cancellation)?;
            let Some(metadata) = self.metadata.get(path) else {
                continue;
            };
            if graph
                .file_metadata(path)
                .is_some_and(|current| current == metadata)
            {
                continue;
            }
            graph.replace_file_metadata(path, metadata.clone())?;
        }
        for owner in relationship_owners {
            check_cancelled(cancellation)?;
            let Some(contribution) = self.relationships.get(owner) else {
                continue;
            };
            for edge in &contribution.edges {
                let (Some(from), Some(to)) = (
                    entity_for_address(&graph, &edge.from),
                    entity_for_address(&graph, &edge.to),
                ) else {
                    continue;
                };
                graph.add_edge_owned_by(
                    owner.clone(),
                    Edge {
                        kind: edge.kind,
                        from,
                        to,
                        provenance: edge.provenance,
                        precision: edge.precision,
                        location: edge.location.clone(),
                    },
                )?;
                if graph.edge_count() > self.graph_limits.max_edges {
                    return Ok(None);
                }
            }
        }
        for path in call_owners {
            check_cancelled(cancellation)?;
            let Some(file) = self.files.get(path) else {
                continue;
            };
            for call_site in &file.calls {
                let Some(caller) = graph.symbols_in_file(path).nth(call_site.caller) else {
                    continue;
                };
                graph.add_call_site(CallSiteInput {
                    caller: caller.id,
                    form: call_site.form,
                    target_kind: call_site.target_kind,
                    name: call_site.name.clone(),
                    qualifier: call_site.qualifier.clone(),
                    receiver_type: None,
                    receiver_type_source: None,
                    receiver_hint: call_site.receiver_hint.clone(),
                    location: call_site.location.clone(),
                    provenance: Provenance::TreeSitter,
                    precision: Precision::Syntax,
                })?;
                if graph.edge_count() > self.graph_limits.max_edges {
                    return Ok(None);
                }
            }
        }
        graph.set_truncated_call_sites(0)?;
        let report = GraphBuildReport {
            retained_symbols: graph.symbol_count(),
            retained_edges: graph.edge_count(),
            retained_call_sites: graph.call_site_count(),
            ..GraphBuildReport::default()
        };
        Ok(Some((graph, report)))
    }

    fn reuse_all_publication(&self) -> IndexPublicationMetrics {
        let facts = self.fact_counts();
        IndexPublicationMetrics {
            structurally_incremental: true,
            reused_files: facts.files,
            reused_source_bytes: facts.source_bytes,
            reused_symbols: self.graph.symbol_count(),
            reused_edges: self.graph.edge_count(),
            reused_call_sites: self.graph.call_site_count(),
            ..IndexPublicationMetrics::default()
        }
    }

    fn full_publication(&self) -> IndexPublicationMetrics {
        let facts = self.fact_counts();
        IndexPublicationMetrics {
            rebuilt_files: facts.files,
            rebuilt_source_bytes: facts.source_bytes,
            rebuilt_symbols: self.graph.symbol_count(),
            rebuilt_edges: self.graph.edge_count(),
            rebuilt_call_sites: self.graph.call_site_count(),
            ..IndexPublicationMetrics::default()
        }
    }

    fn delta_publication(
        &self,
        previous: &Self,
        changed_paths: &BTreeSet<RepoRelativePath>,
        relationship_owners: &BTreeSet<RepoRelativePath>,
        call_owners: &BTreeSet<RepoRelativePath>,
        stable_symbol_paths: &BTreeSet<RepoRelativePath>,
    ) -> IndexPublicationMetrics {
        let rebuilt_files = changed_paths
            .iter()
            .filter(|path| self.files.contains_key(*path))
            .count() as u64;
        let rebuilt_source_bytes = changed_paths
            .iter()
            .filter_map(|path| self.files.get(path))
            .map(|file| file.source.len() as u64)
            .sum();
        let rebuilt_symbols = changed_paths
            .iter()
            .map(|path| {
                let Some(next) = self.files.get(path) else {
                    return 0;
                };
                if stable_symbol_paths.contains(path) {
                    return previous.files.get(path).map_or(0, |previous| {
                        previous
                            .symbols
                            .iter()
                            .zip(&next.symbols)
                            .filter(|(left, right)| left != right)
                            .count() as u64
                    });
                }
                next.symbols.len() as u64
            })
            .sum();
        let rebuilt_relationship_edges: u64 = relationship_owners
            .iter()
            .filter_map(|path| self.relationships.get(path))
            .map(|relationships| relationships.edges.len() as u64)
            .sum();
        let rebuilt_call_sites: u64 = call_owners
            .iter()
            .filter_map(|path| self.files.get(path))
            .map(|file| file.calls.len() as u64)
            .sum();
        let rebuilt_call_edges: u64 = call_owners
            .iter()
            .flat_map(|path| self.graph.symbols_in_file(path))
            .flat_map(|symbol| {
                self.graph.call_sites_from(symbol.id).map(move |call_site| {
                    if matches!(call_site.resolution, CallResolution::Resolved { .. }) {
                        1 + u64::from(symbol.key.kind == SymbolKind::Test)
                    } else {
                        0
                    }
                })
            })
            .sum();
        let rebuilt_edges = rebuilt_relationship_edges.saturating_add(rebuilt_call_edges);
        let facts = self.fact_counts();
        IndexPublicationMetrics {
            structurally_incremental: true,
            reused_files: facts.files.saturating_sub(rebuilt_files),
            rebuilt_files,
            reused_source_bytes: facts.source_bytes.saturating_sub(rebuilt_source_bytes),
            rebuilt_source_bytes,
            reused_symbols: self.graph.symbol_count().saturating_sub(rebuilt_symbols),
            rebuilt_symbols,
            reused_edges: self.graph.edge_count().saturating_sub(rebuilt_edges),
            rebuilt_edges,
            copied_edges: self
                .graph
                .adjacency_entries_copied()
                .saturating_sub(previous.graph.adjacency_entries_copied()),
            reused_call_sites: self
                .graph
                .call_site_count()
                .saturating_sub(rebuilt_call_sites),
            rebuilt_call_sites,
            ..IndexPublicationMetrics::default()
        }
    }
}

#[derive(Debug)]
struct ParseWorkerOutput {
    parser_error: Option<String>,
    parsed: Vec<(usize, Result<ParsedFile, String>)>,
}

fn parse_sources_scheduled(
    sources: BTreeMap<RepoRelativePath, Arc<str>>,
    worker_limit: usize,
    parallel_file_threshold: usize,
    cancellation: &IndexCancellation,
) -> Result<(BTreeMap<RepoRelativePath, Arc<ParsedFile>>, ParseSchedule), RustIndexError> {
    parse_sources_scheduled_inner(
        sources,
        worker_limit,
        parallel_file_threshold,
        cancellation,
        None,
    )
}

fn parse_sources_scheduled_inner(
    sources: BTreeMap<RepoRelativePath, Arc<str>>,
    worker_limit: usize,
    parallel_file_threshold: usize,
    cancellation: &IndexCancellation,
    worker_started: Option<&(dyn Fn() + Sync)>,
) -> Result<(BTreeMap<RepoRelativePath, Arc<ParsedFile>>, ParseSchedule), RustIndexError> {
    let inputs: Vec<_> = sources.into_iter().collect();
    if inputs.is_empty() {
        return Ok((
            BTreeMap::new(),
            ParseSchedule {
                effective_workers: 0,
                peak_active_workers: 0,
                peak_queue_depth: 0,
            },
        ));
    }
    let worker_count = worker_limit.min(inputs.len()).max(1);
    if worker_count == 1 || inputs.len() < parallel_file_threshold {
        let mut parser = RustParser::new().map_err(parse_error)?;
        let mut files = BTreeMap::new();
        for (path, source) in inputs {
            check_cancelled(cancellation)?;
            let parsed = parser.parse(path.clone(), source).map_err(parse_error)?;
            files.insert(path, Arc::new(parsed));
        }
        return Ok((
            files,
            ParseSchedule {
                effective_workers: 1,
                peak_active_workers: 1,
                peak_queue_depth: 0,
            },
        ));
    }

    let cursor = AtomicUsize::new(0);
    let active_workers = AtomicUsize::new(0);
    let peak_active_workers = AtomicUsize::new(0);
    let outputs = thread::scope(|scope| -> Result<Vec<ParseWorkerOutput>, RustIndexError> {
        let mut workers = Vec::with_capacity(worker_count);
        let mut spawn_error = None;
        for worker in 0..worker_count {
            let name = format!("chakra-rust-parser-{worker}");
            match thread::Builder::new().name(name).spawn_scoped(scope, || {
                let active = active_workers.fetch_add(1, Ordering::Relaxed) + 1;
                peak_active_workers.fetch_max(active, Ordering::Relaxed);
                let output = (|| {
                    let mut parser = match RustParser::new() {
                        Ok(parser) => parser,
                        Err(error) => {
                            return ParseWorkerOutput {
                                parser_error: Some(error.to_string()),
                                parsed: Vec::new(),
                            };
                        }
                    };
                    if let Some(worker_started) = worker_started {
                        worker_started();
                    }
                    let mut parsed = Vec::with_capacity(
                        inputs.len().saturating_add(worker_count - 1) / worker_count,
                    );
                    loop {
                        if cancellation.is_cancelled() {
                            break;
                        }
                        let index = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some((path, source)) = inputs.get(index) else {
                            break;
                        };
                        let result = parser
                            .parse(path.clone(), source.clone())
                            .map_err(|error| error.to_string());
                        parsed.push((index, result));
                    }
                    ParseWorkerOutput {
                        parser_error: None,
                        parsed,
                    }
                })();
                active_workers.fetch_sub(1, Ordering::Relaxed);
                output
            }) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    spawn_error = Some(error);
                    break;
                }
            }
        }
        let mut outputs = Vec::with_capacity(workers.len());
        let mut worker_panicked = false;
        for worker in workers {
            match worker.join() {
                Ok(output) => outputs.push(output),
                Err(_) => worker_panicked = true,
            }
        }
        if worker_panicked {
            return Err(RustIndexError::WorkerPanicked);
        }
        if let Some(error) = spawn_error {
            return Err(RustIndexError::WorkerSpawn(error));
        }
        Ok(outputs)
    })?;
    check_cancelled(cancellation)?;
    if let Some(error) = outputs
        .iter()
        .find_map(|output| output.parser_error.clone())
    {
        return Err(RustIndexError::Parse(error));
    }

    let mut ordered: Vec<Option<Result<ParsedFile, String>>> =
        std::iter::repeat_with(|| None).take(inputs.len()).collect();
    for output in outputs {
        for (index, parsed) in output.parsed {
            let Some(slot) = ordered.get_mut(index) else {
                return Err(RustIndexError::Update(
                    "parser worker returned an out-of-range source index".to_owned(),
                ));
            };
            *slot = Some(parsed);
        }
    }
    let mut files = BTreeMap::new();
    for ((path, _), parsed) in inputs.into_iter().zip(ordered) {
        let parsed = parsed.ok_or_else(|| {
            RustIndexError::Update("parser worker omitted an admitted source".to_owned())
        })?;
        let parsed = parsed.map_err(RustIndexError::Parse)?;
        files.insert(path, Arc::new(parsed));
    }
    Ok((
        files,
        ParseSchedule {
            effective_workers: worker_count as u64,
            peak_active_workers: peak_active_workers.load(Ordering::Relaxed) as u64,
            // Work is claimed through an atomic cursor, so no retained task queue
            // exists and its truthful observed depth is zero.
            peak_queue_depth: 0,
        },
    ))
}

fn check_cancelled(cancellation: &IndexCancellation) -> Result<(), RustIndexError> {
    if cancellation.is_cancelled() {
        Err(RustIndexError::Cancelled)
    } else {
        Ok(())
    }
}

fn phase(
    phase: IndexPhase,
    elapsed: Duration,
    work_items: u64,
    bytes: u64,
) -> IndexPhaseMeasurement {
    IndexPhaseMeasurement {
        phase,
        language: Some(Language::Rust),
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
    started: PhaseTimer,
    work_items: u64,
    bytes: u64,
    effective_workers: u64,
    peak_active_workers: u64,
    peak_queue_depth: u64,
) -> IndexPhaseMeasurement {
    let elapsed_micros = started.wall.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
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
        language: Some(Language::Rust),
        elapsed_micros,
        cpu_micros,
        cpu_utilization_per_mille,
        work_items,
        bytes,
        effective_workers: if work_items == 0 {
            0
        } else {
            effective_workers
        },
        peak_active_workers: if work_items == 0 {
            0
        } else {
            peak_active_workers
        },
        peak_queue_depth,
        rss_bytes: (work_items >= PHASE_RESOURCE_SAMPLE_THRESHOLD)
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

fn build_all_relationships(
    files: &BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
    catalog: &SymbolCatalog<'_>,
    limit: u64,
    cancellation: &IndexCancellation,
) -> Result<BTreeMap<RepoRelativePath, Arc<RelationshipContribution>>, RustIndexError> {
    let mut relationships = BTreeMap::new();
    let mut retained = 0_u64;
    for (path, file) in files {
        check_cancelled(cancellation)?;
        let remaining = limit.saturating_sub(retained);
        let contribution = relationships_for_file(path, file, catalog, remaining);
        retained = retained.saturating_add(contribution.edges.len() as u64);
        relationships.insert(path.clone(), Arc::new(contribution));
    }
    Ok(relationships)
}

fn parse_error(error: impl std::fmt::Display) -> RustIndexError {
    RustIndexError::Parse(error.to_string())
}

fn unique(matches: Option<&Vec<SymbolAddress>>) -> Option<SymbolAddress> {
    let matches = matches?;
    (matches.len() == 1).then(|| matches[0].clone())
}

fn qualified(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}::{name}")
    }
}

fn symbol_name(symbol: &SymbolDraft) -> &str {
    symbol
        .key
        .qualified_name
        .rsplit("::")
        .next()
        .unwrap_or(&symbol.key.qualified_name)
}

fn exported_dependencies(file: &ParsedFile) -> HashSet<DependencyKey> {
    let mut keys = HashSet::new();
    for symbol in &file.symbols {
        keys.insert(DependencyKey::Exact(
            symbol.key.qualified_name.clone(),
            symbol.key.kind,
        ));
    }
    keys
}

fn normalized_metadata(
    sources: &BTreeMap<RepoRelativePath, Arc<str>>,
    metadata: BTreeMap<RepoRelativePath, SourceMetadata>,
) -> BTreeMap<RepoRelativePath, SourceMetadata> {
    sources
        .keys()
        .map(|path| {
            let metadata = metadata
                .get(path)
                .cloned()
                .unwrap_or_else(|| SourceMetadata::path_fallback(path));
            (path.clone(), metadata)
        })
        .collect()
}

fn symbol_keys_equal(left: &ParsedFile, right: &ParsedFile) -> bool {
    left.symbols.len() == right.symbols.len()
        && left
            .symbols
            .iter()
            .zip(&right.symbols)
            .all(|(left, right)| left.key == right.key)
}

fn syntax_facts_equal(left: &ParsedFile, right: &ParsedFile) -> bool {
    left.module_path == right.module_path
        && left.symbols == right.symbols
        && left.calls == right.calls
        && left.implementations == right.implementations
        && left.has_errors == right.has_errors
}

fn exported_callables(file: &ParsedFile) -> HashSet<(u8, String)> {
    file.symbols
        .iter()
        .filter_map(|symbol| {
            callable_target_kind(symbol.key.kind)
                .map(|kind| callable_dependency(kind, symbol_name(symbol)))
        })
        .collect()
}

fn callable_target_kind(kind: SymbolKind) -> Option<chakra_domain::symbol::CallTargetKind> {
    use chakra_domain::symbol::CallTargetKind;
    match kind {
        SymbolKind::Function => Some(CallTargetKind::Function),
        SymbolKind::Method => Some(CallTargetKind::Method),
        SymbolKind::Test => Some(CallTargetKind::Test),
        _ => None,
    }
}

fn callable_dependency(kind: chakra_domain::symbol::CallTargetKind, name: &str) -> (u8, String) {
    use chakra_domain::symbol::CallTargetKind;
    let kind = match kind {
        CallTargetKind::Function => 0,
        CallTargetKind::Method => 1,
        CallTargetKind::FunctionOrMethod => 2,
        CallTargetKind::Test => 3,
        CallTargetKind::Configuration => 4,
    };
    (kind, name.to_owned())
}

fn entity_for_address(
    graph: &SymbolGraph,
    address: &SymbolAddress,
) -> Option<chakra_domain::symbol::EntityId> {
    graph
        .symbols_in_file(&address.path)
        .nth(address.index)
        .map(|symbol| symbol.id)
}

fn relationships_for_file(
    path: &RepoRelativePath,
    file: &ParsedFile,
    catalog: &SymbolCatalog<'_>,
    edge_limit: u64,
) -> RelationshipContribution {
    let mut contribution = RelationshipContribution::default();
    let address = |index| SymbolAddress {
        path: path.clone(),
        index,
    };

    let physical_module = if file.module_path.is_empty() {
        None
    } else {
        let name = file.module_path.join("::");
        contribution
            .dependencies
            .insert(DependencyKey::Exact(name.clone(), SymbolKind::Module));
        catalog.unique_exact(&name, SymbolKind::Module)
    };
    for (index, symbol) in file.symbols.iter().enumerate() {
        let parent = symbol
            .parent
            .map(&address)
            .or_else(|| physical_module.clone());
        let Some(parent) = parent else {
            continue;
        };
        let child = address(index);
        if parent != child {
            let physical = symbol.parent.is_none();
            contribution.push_edge(
                RelationshipEdge {
                    kind: EdgeKind::Contains,
                    from: parent,
                    to: child,
                    provenance: if physical {
                        Provenance::Heuristic
                    } else {
                        Provenance::TreeSitter
                    },
                    precision: if physical {
                        Precision::Heuristic
                    } else {
                        Precision::Syntax
                    },
                    location: None,
                },
                edge_limit,
            );
        }
    }

    for implementation in &file.implementations {
        let module = implementation.module_path.join("::");
        let mut target = None;
        if let Some(target_lookup) = implementation.target_lookup.as_ref() {
            for kind in [SymbolKind::Struct, SymbolKind::Enum] {
                let name = qualified(&module, target_lookup);
                contribution
                    .dependencies
                    .insert(DependencyKey::Exact(name.clone(), kind));
                target = target.or_else(|| catalog.unique_exact(&name, kind));
            }
        }
        if let Some(target) = target.clone() {
            contribution.push_edge(
                RelationshipEdge {
                    kind: EdgeKind::Contains,
                    from: target,
                    to: address(implementation.symbol),
                    provenance: Provenance::TreeSitter,
                    precision: Precision::Heuristic,
                    location: None,
                },
                edge_limit,
            );
        }

        let Some(trait_lookup) = implementation.trait_lookup.as_ref() else {
            continue;
        };
        let trait_name = qualified(&module, trait_lookup);
        contribution
            .dependencies
            .insert(DependencyKey::Exact(trait_name.clone(), SymbolKind::Trait));
        let Some(trait_address) = catalog.unique_exact(&trait_name, SymbolKind::Trait) else {
            continue;
        };
        if let Some(target) = target {
            contribution.push_edge(
                RelationshipEdge {
                    kind: EdgeKind::Implements,
                    from: target,
                    to: trait_address.clone(),
                    provenance: Provenance::TreeSitter,
                    precision: Precision::Heuristic,
                    location: None,
                },
                edge_limit,
            );
        }
        let Some(trait_symbol) = catalog.symbol(&trait_address) else {
            continue;
        };
        for (method_index, method) in file.symbols.iter().enumerate() {
            if method.parent != Some(implementation.symbol) || method.key.kind != SymbolKind::Method
            {
                continue;
            }
            let trait_method_name =
                qualified(&trait_symbol.key.qualified_name, symbol_name(method));
            contribution.dependencies.insert(DependencyKey::Exact(
                trait_method_name.clone(),
                SymbolKind::Method,
            ));
            if let Some(trait_method) = catalog.unique_exact(&trait_method_name, SymbolKind::Method)
            {
                contribution.push_edge(
                    RelationshipEdge {
                        kind: EdgeKind::Implements,
                        from: address(method_index),
                        to: trait_method,
                        provenance: Provenance::TreeSitter,
                        precision: Precision::Heuristic,
                        location: None,
                    },
                    edge_limit,
                );
            }
        }
    }

    contribution
}

fn read_sources(repository_root: &Path) -> Result<RustSources, RustIndexError> {
    let files = discover_rust_sources(repository_root)?;
    let mut sources = BTreeMap::new();
    let mut metadata = BTreeMap::new();
    let mut total_bytes = 0_usize;
    for discovered in files {
        let path = discovered.path;
        let file = fs::File::open(repository_root.join(path.as_str())).map_err(|source| {
            RustIndexError::Read {
                path: path.clone(),
                source,
            }
        })?;
        let mut source = String::new();
        file.take((MAX_SOURCE_FILE_BYTES + 1) as u64)
            .read_to_string(&mut source)
            .map_err(|source| RustIndexError::Read {
                path: path.clone(),
                source,
            })?;
        if source.len() > MAX_SOURCE_FILE_BYTES {
            return Err(RustIndexError::SourceTooLarge {
                path,
                limit: MAX_SOURCE_FILE_BYTES,
            });
        }
        total_bytes =
            total_bytes
                .checked_add(source.len())
                .ok_or(RustIndexError::RepositoryTooLarge {
                    limit: MAX_REPOSITORY_SOURCE_BYTES,
                })?;
        if total_bytes > MAX_REPOSITORY_SOURCE_BYTES {
            return Err(RustIndexError::RepositoryTooLarge {
                limit: MAX_REPOSITORY_SOURCE_BYTES,
            });
        }
        metadata.insert(path.clone(), discovered.metadata);
        sources.insert(path, Arc::<str>::from(source));
    }
    Ok(RustSources {
        files: sources,
        metadata,
    })
}

/// Builds a complete Rust syntax index from the actual materialized Git
/// worktree. The caller owns atomic publication into `WorkspaceEngine`.
pub fn index_repository(root: &Path) -> Result<IndexReport, RustIndexError> {
    let started = Instant::now();
    let repository_root = resolve_repository_root(root)?;
    let span = info_span!("rust_repository_index", root = %repository_root.display());
    let _entered = span.enter();
    let sources = read_sources(&repository_root)?;
    let discovered_files = sources.files.len() as u64;
    let (syntax_index, graph) = RustSyntaxIndex::from_classified_sources(sources)?;
    let metrics = IndexMetrics {
        discovered_files,
        parsed_files: discovered_files,
        syntax_error_files: syntax_index.syntax_error_files(),
        truncated_call_sites: syntax_index.truncated_call_sites(),
        symbols: graph.symbol_count(),
        edges: graph.edge_count(),
        call_sites: graph.call_site_count(),
        ambiguous_call_sites: graph.ambiguous_call_site_count(),
        unresolved_call_sites: graph.unresolved_call_site_count(),
        elapsed: started.elapsed(),
    };
    info!(
        files = metrics.parsed_files,
        syntax_error_files = metrics.syntax_error_files,
        truncated_call_sites = metrics.truncated_call_sites,
        symbols = metrics.symbols,
        edges = metrics.edges,
        call_sites = metrics.call_sites,
        ambiguous_call_sites = metrics.ambiguous_call_sites,
        unresolved_call_sites = metrics.unresolved_call_sites,
        elapsed_micros = metrics.elapsed.as_micros(),
        "Rust syntax index completed"
    );
    Ok(IndexReport {
        repository_root,
        graph,
        metrics,
        syntax_index,
    })
}

/// Reads the latest Git-aware Rust file inventory and exact contents.
pub fn scan_repository_sources(repository_root: &Path) -> Result<RustSources, RustIndexError> {
    read_sources(repository_root)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs::{self, File};
    use std::process::Command;
    use std::sync::Barrier;

    use tempfile::TempDir;

    use super::*;

    fn graph_snapshot(graph: &SymbolGraph) -> Vec<String> {
        let mut snapshot = vec![format!("files:{:?}", graph.file_summaries())];
        for symbol in graph.symbols() {
            snapshot.push(format!("symbol:{symbol:?}"));
            snapshot.push(format!("outgoing:{:?}", graph.outgoing_edges(symbol.id)));
            snapshot.push(format!(
                "calls:{:?}",
                graph.call_sites_from(symbol.id).collect::<Vec<_>>()
            ));
        }
        snapshot
    }

    #[test]
    fn bounded_parallel_parsing_is_deterministic() -> Result<(), Box<dyn Error>> {
        let mut sources = BTreeMap::new();
        for index in 0..64 {
            sources.insert(
                RepoRelativePath::new(format!("src/generated_{index:03}.rs"))?,
                Arc::<str>::from(format!(
                    "pub fn caller_{index}() {{ helper_{index}(); }}\npub fn helper_{index}() {{}}\n"
                )),
            );
        }
        let cancellation = IndexCancellation::default();
        let (_, sequential, sequential_metrics) = RustSyntaxIndex::from_sources_scheduled(
            sources.clone(),
            GraphBuildLimits::UNLIMITED,
            1,
            1,
            &cancellation,
        )?;
        let (_, parallel, parallel_metrics) = RustSyntaxIndex::from_sources_scheduled(
            sources,
            GraphBuildLimits::UNLIMITED,
            4,
            1,
            &cancellation,
        )?;

        assert_eq!(graph_snapshot(&sequential), graph_snapshot(&parallel));
        assert_eq!(sequential_metrics.facts, parallel_metrics.facts);
        assert_eq!(sequential_metrics.graph, parallel_metrics.graph);
        let parallel_parse = parallel_metrics
            .phases
            .iter()
            .find(|phase| phase.phase == IndexPhase::ParseExtraction)
            .ok_or("parallel parse phase missing")?;
        assert_eq!(parallel_parse.effective_workers, 4);
        assert!(
            (1..=parallel_parse.effective_workers).contains(&parallel_parse.peak_active_workers)
        );
        assert_eq!(parallel_parse.peak_queue_depth, 0);
        assert_eq!(parallel_parse.work_items, 64);
        Ok(())
    }

    #[test]
    fn cancelling_parallel_parse_joins_every_scoped_worker() -> Result<(), Box<dyn Error>> {
        const WORKERS: usize = 4;
        let mut sources = BTreeMap::new();
        for index in 0..64 {
            sources.insert(
                RepoRelativePath::new(format!("src/cancel_{index:03}.rs"))?,
                Arc::<str>::from(format!("pub fn item_{index}() {{}}\n")),
            );
        }
        let cancellation = IndexCancellation::default();
        let worker_cancellation = cancellation.clone();
        let started = Arc::new(Barrier::new(WORKERS + 1));
        let release = Arc::new(Barrier::new(WORKERS + 1));
        let worker_started = started.clone();
        let worker_release = release.clone();
        let indexing = std::thread::spawn(move || {
            let observer = || {
                worker_started.wait();
                worker_release.wait();
            };
            parse_sources_scheduled_inner(
                sources,
                WORKERS,
                1,
                &worker_cancellation,
                Some(&observer),
            )
        });

        started.wait();
        cancellation.cancel();
        release.wait();
        let result = indexing.join().map_err(|_| "index owner panicked")?;
        assert!(matches!(result, Err(RustIndexError::Cancelled)));
        Ok(())
    }

    #[test]
    fn duplicate_call_fanout_is_stored_linearly() -> Result<(), Box<dyn Error>> {
        const TARGETS: usize = 256;
        const CALLS: usize = 256;

        let mut source = String::new();
        for index in 0..TARGETS {
            source.push_str(&format!("mod target_{index} {{ pub fn target() {{}} }}\n"));
        }
        for index in 0..CALLS {
            source.push_str(&format!("pub fn caller_{index}() {{ target(); }}\n"));
        }
        let mut sources = BTreeMap::new();
        sources.insert(
            RepoRelativePath::new("src/lib.rs")?,
            Arc::<str>::from(source),
        );

        let started = Instant::now();
        let (_, graph) = RustSyntaxIndex::from_sources(sources)?;
        let elapsed = started.elapsed();
        let call_edges = graph
            .symbols()
            .iter()
            .flat_map(|symbol| graph.outgoing_edges(symbol.id))
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .count();

        assert_eq!(graph.call_site_count(), CALLS as u64);
        assert_eq!(graph.ambiguous_call_site_count(), CALLS as u64);
        assert_eq!(graph.unresolved_call_site_count(), 0);
        assert_eq!(call_edges, 0, "ambiguous calls must not fan out into edges");
        assert_eq!(graph.truncated_call_sites(), 0);
        eprintln!(
            "lazy_call_sites: targets={TARGETS}, calls={CALLS}, call_sites={}, call_edges={call_edges}, eager_edge_product={}, elapsed={elapsed:?}",
            graph.call_site_count(),
            TARGETS * CALLS,
        );
        Ok(())
    }

    #[test]
    fn truncated_catalog_never_turns_ambiguity_into_a_unique_call() -> Result<(), Box<dyn Error>> {
        let mut sources = BTreeMap::new();
        sources.insert(
            RepoRelativePath::new("src/a_caller.rs")?,
            Arc::<str>::from("pub fn invoke() { target(); }\n"),
        );
        sources.insert(
            RepoRelativePath::new("src/b_target.rs")?,
            Arc::<str>::from("pub fn target() {}\n"),
        );
        sources.insert(
            RepoRelativePath::new("src/c_target.rs")?,
            Arc::<str>::from("pub fn target() {}\n"),
        );
        let cancellation = IndexCancellation::default();
        let (_, graph, metrics) = RustSyntaxIndex::from_sources_bounded(
            sources,
            GraphBuildLimits {
                max_symbols: 2,
                max_edges: 10,
                max_call_sites: 10,
            },
            &cancellation,
        )?;

        assert_eq!(metrics.graph.omitted_symbols, 1);
        assert_eq!(metrics.graph.call_sites_omitted_by_symbol_budget, 1);
        assert_eq!(graph.call_site_count(), 0);
        let call_edges = graph
            .symbols()
            .iter()
            .flat_map(|symbol| graph.outgoing_edges(symbol.id))
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .count();
        assert_eq!(call_edges, 0);
        graph.validate_consistency()?;
        Ok(())
    }

    #[test]
    fn metadata_change_republishes_without_reparsing_source() -> Result<(), Box<dyn Error>> {
        let path = RepoRelativePath::new("src/lib.rs")?;
        let source = Arc::<str>::from("pub fn stable() {}\n");
        let initial = RustSources {
            files: BTreeMap::from([(path.clone(), source.clone())]),
            metadata: BTreeMap::from([(path.clone(), SourceMetadata::path_fallback(&path))]),
        };
        let (index, _) = RustSyntaxIndex::from_classified_sources(initial)?;
        let changed = RustSources {
            files: BTreeMap::from([(path.clone(), source)]),
            metadata: BTreeMap::from([(
                path.clone(),
                SourceMetadata {
                    role: chakra_domain::source::SourceRole::Production,
                    classification: chakra_domain::source::SourceClassification::CargoMetadata,
                    package: Some(chakra_domain::source::SourcePackage {
                        name: "app".to_owned(),
                        root: None,
                    }),
                },
            )]),
        };
        let reconciled = index.reconcile_classified_sources(changed)?;
        assert_eq!(reconciled.metrics.reparsed_files, 0);
        let graph = reconciled.graph.ok_or("metadata-only graph missing")?;
        assert_eq!(
            graph
                .file_metadata(&path)
                .and_then(|metadata| metadata.package.as_ref())
                .map(|package| package.name.as_str()),
            Some("app")
        );
        Ok(())
    }

    #[test]
    fn rejects_a_source_larger_than_the_file_budget() -> Result<(), Box<dyn Error>> {
        let repository = TempDir::new()?;
        let status = Command::new("git")
            .current_dir(repository.path())
            .args(["init", "--quiet"])
            .status()?;
        assert!(status.success());
        fs::create_dir_all(repository.path().join("src"))?;
        let file = File::create(repository.path().join("src/large.rs"))?;
        file.set_len((MAX_SOURCE_FILE_BYTES + 1) as u64)?;

        let error = match read_sources(repository.path()) {
            Ok(_) => return Err("oversized source was indexed".into()),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            RustIndexError::SourceTooLarge { limit, .. } if limit == MAX_SOURCE_FILE_BYTES
        ));
        Ok(())
    }
}
