//! Language-neutral syntax indexing driver (issue #94): cold-build and
//! reconcile scheduling, bounded parser workers, metrics, limits,
//! relationship materialization, and graph publication. Language-specific
//! seams enter only through [`LanguageHooks`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::marker::PhantomData;
use std::path::Path;
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
    BoundedGraphBuilder, CallSiteInput, GraphBuildLimits, GraphBuildReport, SymbolGraph,
};
use chakra_git::resolve_repository_root;
use tracing::{info, info_span};

#[cfg(unix)]
use nix::sys::resource::{UsageWho, getrusage};
#[cfg(unix)]
use nix::sys::time::TimeValLike;

use crate::error::LanguageIndexError;
use crate::facts::{ParsedFile, SymbolDraft};
use crate::hooks::{LanguageHooks, LanguageParser};
use crate::metrics::{
    IndexMetrics, IndexReport, LanguageBuildMetrics, ParseSchedule, ReconcileMetrics,
    ReconcileReport, SyntaxFactCounts,
};

/// Per-file source budget enforced while reading repository sources.
pub const MAX_SOURCE_FILE_BYTES: usize = 8 * 1024 * 1024;
/// Whole-repository source budget enforced while reading repository sources.
pub const MAX_REPOSITORY_SOURCE_BYTES: usize = 256 * 1024 * 1024;
const PHASE_RESOURCE_SAMPLE_THRESHOLD: u64 = 32;

/// Latest source text plus role/package metadata from the same scan.
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SymbolAddress {
    path: RepoRelativePath,
    index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DependencyKey {
    Exact(String, SymbolKind),
}

#[derive(Debug, Clone)]
struct RelationshipEdge {
    kind: EdgeKind,
    from: SymbolAddress,
    to: SymbolAddress,
    provenance: Provenance,
    precision: Precision,
    location: Option<SourceRange>,
}

#[derive(Debug, Clone, Default)]
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
struct SymbolCatalog {
    exact: HashMap<(String, SymbolKind), Vec<SymbolAddress>>,
}

impl SymbolCatalog {
    fn new(files: &BTreeMap<RepoRelativePath, Arc<ParsedFile>>) -> Self {
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
        Self { exact }
    }

    fn unique_exact(&self, qualified_name: &str, kind: SymbolKind) -> Option<SymbolAddress> {
        unique(self.exact.get(&(qualified_name.to_owned(), kind)))
    }
}

/// Reusable per-file syntax facts and per-owner relationship contributions.
///
/// Entity ids are intentionally absent here: they are revision-scoped and
/// assigned only while a complete immutable graph is materialized.
#[derive(Debug, Clone)]
pub struct LanguageSyntaxIndex<H: LanguageHooks> {
    files: BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
    metadata: BTreeMap<RepoRelativePath, SourceMetadata>,
    hooks: PhantomData<H>,
    relationships: BTreeMap<RepoRelativePath, Arc<RelationshipContribution>>,
    graph_limits: GraphBuildLimits,
    graph: SymbolGraph,
    graph_report: GraphBuildReport,
}

impl<H: LanguageHooks> Default for LanguageSyntaxIndex<H> {
    fn default() -> Self {
        Self {
            files: BTreeMap::new(),
            metadata: BTreeMap::new(),
            relationships: BTreeMap::new(),
            graph_limits: GraphBuildLimits::UNLIMITED,
            graph: SymbolGraph::new(),
            graph_report: GraphBuildReport::default(),
            hooks: PhantomData,
        }
    }
}

impl<H: LanguageHooks> LanguageSyntaxIndex<H> {
    pub fn from_sources(
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
    ) -> Result<(Self, SymbolGraph), LanguageIndexError> {
        let cancellation = IndexCancellation::default();
        let (index, graph, _) =
            Self::from_sources_bounded(sources, GraphBuildLimits::UNLIMITED, &cancellation)?;
        Ok((index, graph))
    }

    pub fn from_sources_bounded(
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), LanguageIndexError> {
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
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), LanguageIndexError> {
        let metadata = sources
            .keys()
            .map(|path| (path.clone(), SourceMetadata::path_fallback(path)))
            .collect();
        Self::from_classified_sources_scheduled(
            LanguageSources {
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
        sources: LanguageSources,
    ) -> Result<(Self, SymbolGraph), LanguageIndexError> {
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
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        cancellation: &IndexCancellation,
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), LanguageIndexError> {
        let LanguageSources {
            files: sources,
            metadata,
        } = sources;
        let metadata = normalized_metadata(&sources, metadata);
        check_cancelled(cancellation)?;
        let parse_started = PhaseTimer::start();
        let (files, parse_schedule) = parse_sources_scheduled::<H>(
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
        Self::assemble(files, metadata, graph_limits, parse_phase, cancellation)
    }

    /// Rebuilds the index from previously exported per-file parse facts for
    /// `known_files`, parsing only the remaining sources (issue #39). Every
    /// known file is re-checked against the current source text; anything
    /// else is parsed. The result is identical to a cold build of the same
    /// classified sources.
    pub fn restore_classified_sources_scheduled(
        sources: LanguageSources,
        known_files: &BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        cancellation: &IndexCancellation,
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), LanguageIndexError> {
        let LanguageSources {
            files: sources,
            metadata,
        } = sources;
        let metadata = normalized_metadata(&sources, metadata);
        check_cancelled(cancellation)?;
        let mut files = BTreeMap::new();
        let mut misses = BTreeMap::new();
        for (path, source) in sources {
            match known_files.get(&path) {
                Some(known) if known.source.as_ref() == source.as_ref() => {
                    files.insert(path, Arc::clone(known));
                }
                _ => {
                    misses.insert(path, source);
                }
            }
        }
        let parse_started = PhaseTimer::start();
        let (parsed, parse_schedule) = parse_sources_scheduled::<H>(
            misses,
            worker_limit.max(1),
            parallel_file_threshold,
            cancellation,
        )?;
        let parsed_source_bytes = parsed.values().fold(0_u64, |bytes, file| {
            bytes.saturating_add(file.source.len() as u64)
        });
        let parsed_files = parsed.len() as u64;
        for (path, file) in parsed {
            files.insert(path, file);
        }
        let parse_phase = measured_phase(
            IndexPhase::ParseExtraction,
            parse_started,
            parsed_files,
            parsed_source_bytes,
            parse_schedule.effective_workers,
            parse_schedule.peak_active_workers,
            parse_schedule.peak_queue_depth,
        );
        Self::assemble(files, metadata, graph_limits, parse_phase, cancellation)
    }

    /// Shared post-parse pipeline: workspace-evidence hooks, symbol catalog,
    /// relationship contributions, bounded graph materialization, and build
    /// metrics. Used by both the cold build and the cache restore path.
    fn assemble(
        mut files: BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
        metadata: BTreeMap<RepoRelativePath, SourceMetadata>,
        graph_limits: GraphBuildLimits,
        parse_phase: IndexPhaseMeasurement,
        cancellation: &IndexCancellation,
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), LanguageIndexError> {
        check_cancelled(cancellation)?;
        H::post_parse(&mut files);
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
            hooks: PhantomData,
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

    /// Per-file parse facts of the current index, keyed by path. Entity ids
    /// are absent: they are revision-scoped and assigned only while a
    /// complete immutable graph is materialized.
    pub fn parsed_files(&self) -> &BTreeMap<RepoRelativePath, Arc<ParsedFile>> {
        &self.files
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
    ) -> Result<ReconcileReport<H>, LanguageIndexError> {
        self.reconcile_sources_bounded(sources, self.graph_limits, &IndexCancellation::default())
    }

    pub fn reconcile_sources_bounded(
        &self,
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<ReconcileReport<H>, LanguageIndexError> {
        let metadata = sources
            .keys()
            .map(|path| (path.clone(), SourceMetadata::path_fallback(path)))
            .collect();
        self.reconcile_classified_sources_bounded(
            LanguageSources {
                files: sources,
                metadata,
            },
            graph_limits,
            cancellation,
        )
    }

    pub fn reconcile_classified_sources(
        &self,
        sources: LanguageSources,
    ) -> Result<ReconcileReport<H>, LanguageIndexError> {
        self.reconcile_classified_sources_bounded(
            sources,
            self.graph_limits,
            &IndexCancellation::default(),
        )
    }

    pub fn reconcile_classified_sources_bounded(
        &self,
        sources: LanguageSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<ReconcileReport<H>, LanguageIndexError> {
        let LanguageSources {
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
        let metadata_changed = self.metadata != metadata;
        if changed_paths.is_empty() && !limits_changed && !metadata_changed {
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
        let mut parser = H::new_parser()?;
        for path in &changed_paths {
            check_cancelled(cancellation)?;
            match sources.get(path) {
                Some(source) => {
                    let parsed = parser.parse(path.clone(), source.clone())?;
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
        let files_before_post_parse = next_files.clone();
        H::post_parse(&mut next_files);
        // A workspace-evidence hook may change retained facts in a file whose
        // source was not edited (for example C++ qualified-callable
        // reclassification). Those files must participate in the structural
        // graph delta even though they were not reparsed.
        let mut structural_changed_paths = changed_paths.clone();
        for (path, next) in &next_files {
            if files_before_post_parse.get(path) != Some(next) {
                structural_changed_paths.insert(path.clone());
            }
        }
        for path in files_before_post_parse.keys() {
            if !next_files.contains_key(path) {
                structural_changed_paths.insert(path.clone());
            }
        }

        let mut stable_symbol_paths = BTreeSet::new();
        let mut unchanged_fact_paths = BTreeSet::new();
        let mut changed_dependencies = HashSet::new();
        let mut changed_callable_names = HashSet::new();
        for path in &structural_changed_paths {
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
                        changed_callable_names.extend(exported_callable_names(previous));
                    }
                    if let Some(next) = next {
                        changed_dependencies.extend(exported_dependencies(next));
                        changed_callable_names.extend(exported_callable_names(next));
                    }
                }
            }
        }

        let mut affected_owners: BTreeSet<_> = structural_changed_paths
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
        let mut affected_call_owners: BTreeSet<_> = structural_changed_paths
            .difference(&unchanged_fact_paths)
            .cloned()
            .collect();
        // Match by simple name, not by (kind, name): workspace evidence can
        // change a call's stored domain without any edit to the file that
        // contains the call, and the next drafts — not the previously stored
        // ones — decide which call sites are rebuilt.
        affected_call_owners.extend(
            next_files
                .iter()
                .filter(|(_, file)| {
                    file.calls
                        .iter()
                        .any(|call| changed_callable_names.contains(&call.name))
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
            hooks: PhantomData,
        };
        let materialize_started = Instant::now();
        let next_facts = next.fact_counts();
        let complete_previous = self.graph_report.omitted_symbols == 0
            && self.graph_report.omitted_edges == 0
            && self.graph_report.omitted_call_sites == 0;
        let delta_fits = next_facts.symbols <= graph_limits.max_symbols
            && next_facts.call_sites <= graph_limits.max_call_sites;
        let delta_candidate =
            !limits_changed && !metadata_changed && complete_previous && delta_fits;
        let delta = if delta_candidate {
            next.materialize_graph_delta(
                &self.graph,
                &structural_changed_paths,
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
                &structural_changed_paths,
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
    ) -> Result<(SymbolGraph, GraphBuildReport), LanguageIndexError> {
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

    fn materialize_graph_delta(
        &self,
        previous: &SymbolGraph,
        changed_paths: &BTreeSet<RepoRelativePath>,
        relationship_owners: &BTreeSet<RepoRelativePath>,
        call_owners: &BTreeSet<RepoRelativePath>,
        stable_symbol_paths: &BTreeSet<RepoRelativePath>,
        cancellation: &IndexCancellation,
    ) -> Result<Option<(SymbolGraph, GraphBuildReport)>, LanguageIndexError> {
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
                    LanguageIndexError::Update(format!("missing changed file {path}"))
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
        structural_changed_paths: &BTreeSet<RepoRelativePath>,
        source_changed_paths: &BTreeSet<RepoRelativePath>,
        relationship_owners: &BTreeSet<RepoRelativePath>,
        call_owners: &BTreeSet<RepoRelativePath>,
        stable_symbol_paths: &BTreeSet<RepoRelativePath>,
    ) -> IndexPublicationMetrics {
        let rebuilt_files = structural_changed_paths
            .iter()
            .filter(|path| self.files.contains_key(*path))
            .count() as u64;
        let rebuilt_source_bytes = source_changed_paths
            .iter()
            .filter_map(|path| self.files.get(path))
            .map(|file| file.source.len() as u64)
            .sum();
        let rebuilt_symbols = structural_changed_paths
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

#[doc(hidden)]
pub fn parse_sources_scheduled_observed<H: LanguageHooks>(
    sources: BTreeMap<RepoRelativePath, Arc<str>>,
    worker_limit: usize,
    parallel_file_threshold: usize,
    cancellation: &IndexCancellation,
    worker_started: Option<&(dyn Fn() + Sync)>,
) -> Result<
    (
        BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
        crate::metrics::ParseSchedule,
    ),
    LanguageIndexError,
> {
    parse_sources_scheduled_inner::<H>(
        sources,
        worker_limit,
        parallel_file_threshold,
        cancellation,
        worker_started,
    )
}

fn parse_sources_scheduled<H: LanguageHooks>(
    sources: BTreeMap<RepoRelativePath, Arc<str>>,
    worker_limit: usize,
    parallel_file_threshold: usize,
    cancellation: &IndexCancellation,
) -> Result<(BTreeMap<RepoRelativePath, Arc<ParsedFile>>, ParseSchedule), LanguageIndexError> {
    parse_sources_scheduled_inner::<H>(
        sources,
        worker_limit,
        parallel_file_threshold,
        cancellation,
        None,
    )
}

fn parse_sources_scheduled_inner<H: LanguageHooks>(
    sources: BTreeMap<RepoRelativePath, Arc<str>>,
    worker_limit: usize,
    parallel_file_threshold: usize,
    cancellation: &IndexCancellation,
    worker_started: Option<&(dyn Fn() + Sync)>,
) -> Result<(BTreeMap<RepoRelativePath, Arc<ParsedFile>>, ParseSchedule), LanguageIndexError> {
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
        let mut parser = H::new_parser()?;
        let mut files = BTreeMap::new();
        for (path, source) in inputs {
            check_cancelled(cancellation)?;
            let parsed = parser.parse(path.clone(), source)?;
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
    let outputs = thread::scope(
        |scope| -> Result<Vec<ParseWorkerOutput>, LanguageIndexError> {
            let mut workers = Vec::with_capacity(worker_count);
            let mut spawn_error = None;
            for worker in 0..worker_count {
                let name = format!("chakra-{}-parser-{worker}", H::WORKER_NAME);
                match thread::Builder::new().name(name).spawn_scoped(scope, || {
                    let active = active_workers.fetch_add(1, Ordering::Relaxed) + 1;
                    peak_active_workers.fetch_max(active, Ordering::Relaxed);
                    let output = (|| {
                        let mut parser = match H::new_parser() {
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
                return Err(LanguageIndexError::WorkerPanicked);
            }
            if let Some(error) = spawn_error {
                return Err(LanguageIndexError::WorkerSpawn(error));
            }
            Ok(outputs)
        },
    )?;
    check_cancelled(cancellation)?;
    if let Some(error) = outputs
        .iter()
        .find_map(|output| output.parser_error.clone())
    {
        return Err(LanguageIndexError::Parse(error));
    }

    let mut ordered: Vec<Option<Result<ParsedFile, String>>> =
        std::iter::repeat_with(|| None).take(inputs.len()).collect();
    for output in outputs {
        for (index, parsed) in output.parsed {
            let Some(slot) = ordered.get_mut(index) else {
                return Err(LanguageIndexError::Update(
                    "parser worker returned an out-of-range source index".to_owned(),
                ));
            };
            *slot = Some(parsed);
        }
    }
    let mut files = BTreeMap::new();
    for ((path, _), parsed) in inputs.into_iter().zip(ordered) {
        let parsed = parsed.ok_or_else(|| {
            LanguageIndexError::Update("parser worker omitted an admitted source".to_owned())
        })?;
        let parsed = parsed.map_err(LanguageIndexError::Parse)?;
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

fn check_cancelled(cancellation: &IndexCancellation) -> Result<(), LanguageIndexError> {
    if cancellation.is_cancelled() {
        Err(LanguageIndexError::Cancelled)
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
        language: Some(Language::Go),
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
        language: Some(Language::Go),
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
    catalog: &SymbolCatalog,
    limit: u64,
    cancellation: &IndexCancellation,
) -> Result<BTreeMap<RepoRelativePath, Arc<RelationshipContribution>>, LanguageIndexError> {
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

fn unique(matches: Option<&Vec<SymbolAddress>>) -> Option<SymbolAddress> {
    let matches = matches?;
    (matches.len() == 1).then(|| matches[0].clone())
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
        && left.named_relations == right.named_relations
        && left.has_errors == right.has_errors
}

fn exported_callable_names(file: &ParsedFile) -> HashSet<String> {
    file.symbols
        .iter()
        .filter(|symbol| callable_target_kind(symbol.key.kind).is_some())
        .map(|symbol| symbol_name(symbol).to_owned())
        .collect()
}

/// Mirrors the graph's callable-domain mapping (engine `call_target_kind`):
/// module/property/configuration entities are Configuration callables, which
/// Terraform traversals and similar configuration references resolve
/// against. Languages without configuration call sites are unaffected.
fn callable_target_kind(kind: SymbolKind) -> Option<chakra_domain::symbol::CallTargetKind> {
    use chakra_domain::symbol::CallTargetKind;
    match kind {
        SymbolKind::Function => Some(CallTargetKind::Function),
        SymbolKind::Method => Some(CallTargetKind::Method),
        SymbolKind::Test => Some(CallTargetKind::Test),
        SymbolKind::Module | SymbolKind::Property | SymbolKind::Configuration => {
            Some(CallTargetKind::Configuration)
        }
        _ => None,
    }
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
    catalog: &SymbolCatalog,
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

    for relation in &file.named_relations {
        let mut target = None;
        for candidate in &relation.candidates {
            for kind in &relation.target_kinds {
                contribution
                    .dependencies
                    .insert(DependencyKey::Exact(candidate.clone(), *kind));
                target = target.or_else(|| catalog.unique_exact(candidate, *kind));
            }
            if target.is_some() {
                break;
            }
        }
        let Some(target) = target else {
            continue;
        };
        contribution.push_edge(
            RelationshipEdge {
                kind: relation.kind,
                from: address(relation.from),
                to: target,
                provenance: Provenance::TreeSitter,
                precision: Precision::Heuristic,
                location: None,
            },
            edge_limit,
        );
    }

    contribution
}

fn read_sources<H: LanguageHooks>(
    repository_root: &Path,
) -> Result<LanguageSources, LanguageIndexError> {
    let files = H::discover_sources(repository_root)?;
    let mut sources = BTreeMap::new();
    let mut metadata = BTreeMap::new();
    let mut total_bytes = 0_usize;
    for discovered in files {
        let path = discovered.path;
        let file = fs::File::open(repository_root.join(path.as_str())).map_err(|source| {
            LanguageIndexError::Read {
                path: path.clone(),
                source,
            }
        })?;
        let mut source = String::new();
        file.take((MAX_SOURCE_FILE_BYTES + 1) as u64)
            .read_to_string(&mut source)
            .map_err(|source| LanguageIndexError::Read {
                path: path.clone(),
                source,
            })?;
        if source.len() > MAX_SOURCE_FILE_BYTES {
            return Err(LanguageIndexError::SourceTooLarge {
                path,
                limit: MAX_SOURCE_FILE_BYTES,
            });
        }
        total_bytes = total_bytes.checked_add(source.len()).ok_or(
            LanguageIndexError::RepositoryTooLarge {
                limit: MAX_REPOSITORY_SOURCE_BYTES,
            },
        )?;
        if total_bytes > MAX_REPOSITORY_SOURCE_BYTES {
            return Err(LanguageIndexError::RepositoryTooLarge {
                limit: MAX_REPOSITORY_SOURCE_BYTES,
            });
        }
        metadata.insert(path.clone(), discovered.metadata);
        sources.insert(path, Arc::<str>::from(source));
    }
    Ok(LanguageSources {
        files: sources,
        metadata,
    })
}

/// Builds a complete Go syntax index from the actual materialized Git
/// worktree. The caller owns atomic publication into `WorkspaceEngine`.
pub fn index_repository<H: LanguageHooks>(
    root: &Path,
) -> Result<IndexReport<H>, LanguageIndexError> {
    let started = Instant::now();
    let repository_root = resolve_repository_root(root)?;
    let span = info_span!("language_repository_index", language = H::WORKER_NAME, root = %repository_root.display());
    let _entered = span.enter();
    let sources = read_sources::<H>(&repository_root)?;
    let discovered_files = sources.files.len() as u64;
    let (syntax_index, graph) = LanguageSyntaxIndex::<H>::from_classified_sources(sources)?;
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
        "syntax index completed"
    );
    Ok(IndexReport {
        repository_root,
        graph,
        metrics,
        syntax_index,
    })
}

/// Reads the latest Git-aware Go file inventory and exact contents.
pub fn scan_repository_sources<H: LanguageHooks>(
    repository_root: &Path,
) -> Result<LanguageSources, LanguageIndexError> {
    read_sources::<H>(repository_root)
}
