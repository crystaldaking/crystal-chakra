//! Deterministic PHP syntax indexing with reusable per-file facts.

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
use chakra_domain::symbol::{
    CallResolution, CallTargetKind, Edge, EdgeKind, Language, SymbolKey, SymbolKind,
};
use chakra_engine::{
    BoundedGraphBuilder, CallSiteInput, ConsistencyError, GraphBuildLimits, GraphBuildReport,
    GraphError, SymbolGraph,
};
use thiserror::Error;
use tracing::{info, info_span, warn};

#[cfg(unix)]
use nix::sys::resource::{UsageWho, getrusage};
#[cfg(unix)]
use nix::sys::time::TimeValLike;

use crate::laravel::{FrameworkEndpoint, FrameworkSelector, LaravelFile, LaravelParser};
use crate::parser::{ParsedFile, PhpParser, TypeRelationKind};

const MAX_SOURCE_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REPOSITORY_SOURCE_BYTES: usize = 256 * 1024 * 1024;
const PHASE_RESOURCE_SAMPLE_THRESHOLD: u64 = 32;
const MAX_COMPOSER_METADATA_BYTES: usize = 1024 * 1024;

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
    pub laravel_detected: bool,
    pub framework_symbols: u64,
    pub framework_edges: u64,
    pub framework_truncated_files: u64,
    pub elapsed: Duration,
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
    pub next_index: Option<PhpSyntaxIndex>,
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
    pub laravel_detected: bool,
    pub framework_symbols: u64,
    pub framework_edges: u64,
    pub framework_truncated_files: u64,
    pub phases: Vec<IndexPhaseMeasurement>,
}

#[derive(Debug)]
pub struct IndexReport {
    pub repository_root: PathBuf,
    pub graph: SymbolGraph,
    pub metrics: IndexMetrics,
    pub syntax_index: PhpSyntaxIndex,
}

/// Latest PHP source text plus Composer/path metadata from the same scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhpSources {
    pub files: BTreeMap<RepoRelativePath, Arc<str>>,
    pub metadata: BTreeMap<RepoRelativePath, SourceMetadata>,
}

impl PhpSources {
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum PhpIndexError {
    #[error(transparent)]
    Discovery(#[from] chakra_git::DiscoveryError),
    #[error("failed to read PHP source {path}: {source}")]
    Read {
        path: RepoRelativePath,
        #[source]
        source: io::Error,
    },
    #[error("PHP source {path} exceeds the {limit}-byte indexing budget")]
    SourceTooLarge {
        path: RepoRelativePath,
        limit: usize,
    },
    #[error("indexed PHP sources exceed the {limit}-byte repository budget")]
    RepositoryTooLarge { limit: usize },
    #[error("Composer metadata exceeds the {limit}-byte indexing budget")]
    ComposerMetadataTooLarge { limit: usize },
    #[error("failed to read Composer metadata: {0}")]
    ComposerRead(#[source] io::Error),
    #[error("failed to parse Composer metadata: {0}")]
    ComposerInvalid(#[source] serde_json::Error),
    #[error("failed to parse PHP source: {0}")]
    Parse(String),
    #[error("failed to start a bounded PHP parser worker: {0}")]
    WorkerSpawn(#[source] io::Error),
    #[error("a bounded PHP parser worker panicked")]
    WorkerPanicked,
    #[error("PHP syntax index update failed: {0}")]
    Update(String),
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error("constructed PHP syntax graph is inconsistent: {0}")]
    Consistency(#[from] ConsistencyError),
    #[error("PHP syntax indexing was cancelled")]
    Cancelled,
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
    methods: HashMap<(String, CallTargetKind, String), Vec<SymbolAddress>>,
    type_relations: HashMap<String, Vec<(TypeRelationKind, String)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MethodLookup {
    Missing,
    Unique(String),
    Ambiguous,
}

impl SymbolCatalog {
    fn new(files: &BTreeMap<RepoRelativePath, Arc<ParsedFile>>) -> Self {
        let mut exact: HashMap<(String, SymbolKind), Vec<SymbolAddress>> = HashMap::new();
        let mut methods: HashMap<(String, CallTargetKind, String), Vec<SymbolAddress>> =
            HashMap::new();
        let mut type_relations: HashMap<String, Vec<(TypeRelationKind, String)>> = HashMap::new();
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
                let target_kind = match symbol.key.kind {
                    SymbolKind::Method => Some(CallTargetKind::Method),
                    SymbolKind::Test => Some(CallTargetKind::Test),
                    _ => None,
                };
                if let Some(target_kind) = target_kind
                    && let Some((container, name)) = symbol.key.qualified_name.rsplit_once("::")
                {
                    methods
                        .entry((container.to_owned(), target_kind, name.to_owned()))
                        .or_default()
                        .push(address.clone());
                }
            }
            for relation in &file.type_relations {
                let Some(from) = file.symbols.get(relation.from) else {
                    continue;
                };
                type_relations
                    .entry(from.key.qualified_name.clone())
                    .or_default()
                    .push((relation.kind, relation.target.clone()));
            }
        }
        for addresses in exact.values_mut() {
            addresses.sort();
        }
        for addresses in methods.values_mut() {
            addresses.sort();
        }
        for targets in type_relations.values_mut() {
            targets.sort();
            targets.dedup();
        }
        Self {
            exact,
            methods,
            type_relations,
        }
    }

    fn unique_exact(&self, qualified_name: &str, kind: SymbolKind) -> Option<SymbolAddress> {
        let matches = self.exact.get(&(qualified_name.to_owned(), kind))?;
        (matches.len() == 1).then(|| matches[0].clone())
    }

    fn method_qualifier(
        &self,
        receiver_type: &str,
        target_kind: CallTargetKind,
        method_name: &str,
    ) -> String {
        match self.lookup_method(receiver_type, target_kind, method_name, &mut HashSet::new()) {
            MethodLookup::Unique(container) => container,
            MethodLookup::Missing | MethodLookup::Ambiguous => receiver_type.to_owned(),
        }
    }

    fn lookup_method(
        &self,
        receiver_type: &str,
        target_kind: CallTargetKind,
        method_name: &str,
        visiting: &mut HashSet<String>,
    ) -> MethodLookup {
        if !visiting.insert(receiver_type.to_owned()) {
            return MethodLookup::Missing;
        }
        let exact = match self
            .methods
            .get(&(
                receiver_type.to_owned(),
                target_kind,
                method_name.to_owned(),
            ))
            .map(Vec::len)
            .unwrap_or(0)
        {
            0 => MethodLookup::Missing,
            1 => MethodLookup::Unique(receiver_type.to_owned()),
            _ => MethodLookup::Ambiguous,
        };
        if exact != MethodLookup::Missing {
            visiting.remove(receiver_type);
            return exact;
        }

        for kind in [
            TypeRelationKind::Trait,
            TypeRelationKind::Extends,
            TypeRelationKind::Implements,
        ] {
            let mut candidates = Vec::new();
            if let Some(relations) = self.type_relations.get(receiver_type) {
                for (_, target) in relations.iter().filter(|(relation, _)| *relation == kind) {
                    match self.lookup_method(target, target_kind, method_name, visiting) {
                        MethodLookup::Missing => {}
                        MethodLookup::Unique(container) => candidates.push(container),
                        MethodLookup::Ambiguous => {
                            visiting.remove(receiver_type);
                            return MethodLookup::Ambiguous;
                        }
                    }
                }
            }
            candidates.sort();
            candidates.dedup();
            match candidates.as_slice() {
                [] => {}
                [container] => {
                    visiting.remove(receiver_type);
                    return MethodLookup::Unique(container.clone());
                }
                _ => {
                    visiting.remove(receiver_type);
                    return MethodLookup::Ambiguous;
                }
            }
        }
        visiting.remove(receiver_type);
        MethodLookup::Missing
    }
}

#[derive(Debug, Clone)]
pub struct PhpSyntaxIndex {
    files: BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
    metadata: BTreeMap<RepoRelativePath, SourceMetadata>,
    relationships: BTreeMap<RepoRelativePath, Arc<RelationshipContribution>>,
    laravel_detected: bool,
    framework_files: BTreeMap<RepoRelativePath, Arc<LaravelFile>>,
    framework_relationships: BTreeMap<RepoRelativePath, Arc<RelationshipContribution>>,
    graph_limits: GraphBuildLimits,
    graph: SymbolGraph,
    graph_report: GraphBuildReport,
}

impl Default for PhpSyntaxIndex {
    fn default() -> Self {
        Self {
            files: BTreeMap::new(),
            metadata: BTreeMap::new(),
            relationships: BTreeMap::new(),
            laravel_detected: false,
            framework_files: BTreeMap::new(),
            framework_relationships: BTreeMap::new(),
            graph_limits: GraphBuildLimits::UNLIMITED,
            graph: SymbolGraph::new(),
            graph_report: GraphBuildReport::default(),
        }
    }
}

impl PhpSyntaxIndex {
    pub fn from_sources(
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
    ) -> Result<(Self, SymbolGraph), PhpIndexError> {
        let cancellation = IndexCancellation::default();
        let (index, graph, _) =
            Self::from_sources_bounded(sources, GraphBuildLimits::UNLIMITED, &cancellation)?;
        Ok((index, graph))
    }

    pub fn from_sources_bounded(
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), PhpIndexError> {
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
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), PhpIndexError> {
        let metadata = sources
            .keys()
            .map(|path| (path.clone(), SourceMetadata::path_fallback(path)))
            .collect();
        Self::from_classified_sources_scheduled(
            PhpSources {
                files: sources,
                metadata,
            },
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            false,
            cancellation,
        )
    }

    /// Builds a bounded initial index with optional Composer-detected Laravel
    /// enrichment. All framework facts share the ordinary graph budgets.
    pub fn from_sources_scheduled_with_laravel(
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        laravel_detected: bool,
        cancellation: &IndexCancellation,
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), PhpIndexError> {
        let metadata = sources
            .keys()
            .map(|path| (path.clone(), SourceMetadata::path_fallback(path)))
            .collect();
        Self::from_classified_sources_scheduled(
            PhpSources {
                files: sources,
                metadata,
            },
            graph_limits,
            worker_limit,
            parallel_file_threshold,
            laravel_detected,
            cancellation,
        )
    }

    pub fn from_classified_sources(
        sources: PhpSources,
    ) -> Result<(Self, SymbolGraph), PhpIndexError> {
        let cancellation = IndexCancellation::default();
        let (index, graph, _) = Self::from_classified_sources_scheduled(
            sources,
            GraphBuildLimits::UNLIMITED,
            1,
            usize::MAX,
            false,
            &cancellation,
        )?;
        Ok((index, graph))
    }

    /// Builds a classified initial index with bounded parser workers and
    /// optional Composer-detected Laravel enrichment.
    pub fn from_classified_sources_scheduled(
        sources: PhpSources,
        graph_limits: GraphBuildLimits,
        worker_limit: usize,
        parallel_file_threshold: usize,
        laravel_detected: bool,
        cancellation: &IndexCancellation,
    ) -> Result<(Self, SymbolGraph, LanguageBuildMetrics), PhpIndexError> {
        let PhpSources {
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
        let framework_files = parse_framework_files(&files, laravel_detected, cancellation)?;
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
        let retained_relationship_edges = relationships
            .values()
            .map(|contribution| contribution.edges.len() as u64)
            .sum::<u64>();
        let framework_relationships = build_all_framework_relationships(
            &files,
            &framework_files,
            &catalog,
            graph_limits
                .max_edges
                .saturating_sub(retained_relationship_edges),
            cancellation,
        )?;
        let relationship_items = relationships
            .values()
            .fold(0_u64, |count, contribution| {
                count
                    .saturating_add(contribution.edges.len() as u64)
                    .saturating_add(contribution.omitted_edges)
            })
            .saturating_add(
                framework_relationships
                    .values()
                    .fold(0_u64, |count, contribution| {
                        count
                            .saturating_add(contribution.edges.len() as u64)
                            .saturating_add(contribution.omitted_edges)
                    }),
            );
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
            laravel_detected,
            framework_files,
            framework_relationships,
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
        let framework_symbols = index.framework_symbol_count();
        let framework_edges = index.framework_edge_count();
        let framework_truncated_files = index.framework_truncated_files();
        Ok((
            index,
            graph,
            LanguageBuildMetrics {
                facts,
                graph: graph_report,
                laravel_detected,
                framework_symbols,
                framework_edges,
                framework_truncated_files,
                phases,
            },
        ))
    }

    #[cfg(test)]
    fn from_sources_with_laravel(
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
        laravel_detected: bool,
    ) -> Result<(Self, SymbolGraph), PhpIndexError> {
        let cancellation = IndexCancellation::default();
        let (index, graph, _) = Self::from_sources_scheduled_with_laravel(
            sources,
            GraphBuildLimits::UNLIMITED,
            1,
            usize::MAX,
            laravel_detected,
            &cancellation,
        )?;
        Ok((index, graph))
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

    pub fn reconcile_sources(
        &self,
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
    ) -> Result<ReconcileReport, PhpIndexError> {
        self.reconcile_sources_bounded(sources, self.graph_limits, &IndexCancellation::default())
    }

    pub fn reconcile_sources_bounded(
        &self,
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<ReconcileReport, PhpIndexError> {
        let metadata = sources
            .keys()
            .map(|path| (path.clone(), SourceMetadata::path_fallback(path)))
            .collect();
        self.reconcile_classified_sources_bounded(
            PhpSources {
                files: sources,
                metadata,
            },
            graph_limits,
            cancellation,
        )
    }

    pub fn reconcile_classified_sources(
        &self,
        sources: PhpSources,
    ) -> Result<ReconcileReport, PhpIndexError> {
        self.reconcile_classified_sources_bounded(
            sources,
            self.graph_limits,
            &IndexCancellation::default(),
        )
    }

    pub fn reconcile_classified_sources_bounded(
        &self,
        sources: PhpSources,
        graph_limits: GraphBuildLimits,
        cancellation: &IndexCancellation,
    ) -> Result<ReconcileReport, PhpIndexError> {
        let PhpSources {
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
            metrics.framework_truncated_files = self.framework_truncated_files();
            return Ok(ReconcileReport {
                graph: None,
                metrics,
                next_index: None,
                build_metrics: None,
            });
        }

        let mut next_files = self.files.clone();
        let mut next_framework_files = self.framework_files.clone();
        let parse_started = Instant::now();
        let mut parser = PhpParser::new().map_err(parse_error)?;
        let mut laravel_parser = self
            .laravel_detected
            .then(LaravelParser::new)
            .transpose()
            .map_err(parse_error)?;
        for path in &changed_paths {
            check_cancelled(cancellation)?;
            match sources.get(path) {
                Some(source) => {
                    if let Some(laravel_parser) = laravel_parser.as_mut() {
                        let framework = laravel_parser
                            .parse(path.clone(), source.as_ref())
                            .map_err(parse_error)?;
                        next_framework_files.insert(path.clone(), Arc::new(framework));
                        metrics.framework_files_reparsed += 1;
                    }
                    let parsed = parser
                        .parse(path.clone(), source.clone())
                        .map_err(parse_error)?;
                    next_files.insert(path.clone(), Arc::new(parsed));
                    metrics.reparsed_files += 1;
                }
                None => {
                    next_files.remove(path);
                    next_framework_files.remove(path);
                }
            }
        }
        let parse_elapsed = parse_started.elapsed();
        check_cancelled(cancellation)?;

        let mut stable_symbol_paths = BTreeSet::new();
        let mut unchanged_fact_paths = BTreeSet::new();
        let mut changed_dependencies = HashSet::new();
        let mut changed_callables = HashSet::new();
        let mut framework_symbols_changed = false;
        for path in &changed_paths {
            match (self.files.get(path), next_files.get(path)) {
                (Some(previous), Some(next)) if symbol_keys_equal(previous, next) => {
                    if framework_symbol_keys_equal(
                        self.framework_files.get(path),
                        next_framework_files.get(path),
                    ) {
                        stable_symbol_paths.insert(path.clone());
                    } else {
                        framework_symbols_changed = true;
                    }
                    if syntax_facts_equal(previous, next) {
                        unchanged_fact_paths.insert(path.clone());
                    }
                }
                (previous, next) => {
                    framework_symbols_changed |= !framework_symbol_keys_equal(
                        self.framework_files.get(path),
                        next_framework_files.get(path),
                    );
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
        let mut affected_framework_owners = changed_paths.clone();
        affected_framework_owners.extend(
            self.framework_relationships
                .iter()
                .filter(|(_, contribution)| {
                    contribution
                        .dependencies
                        .iter()
                        .any(|key| changed_dependencies.contains(key))
                })
                .map(|(path, _)| path.clone()),
        );
        let mut next_framework_relationships = self.framework_relationships.clone();
        for path in &affected_framework_owners {
            match (next_files.get(path), next_framework_files.get(path)) {
                (Some(file), Some(framework)) => {
                    next_framework_relationships.insert(
                        path.clone(),
                        Arc::new(framework_relationships_for_file(
                            path,
                            file,
                            framework,
                            &catalog,
                            graph_limits.max_edges,
                        )),
                    );
                    metrics.framework_relationship_files_recomputed += 1;
                }
                _ => {
                    next_framework_relationships.remove(path);
                }
            }
        }
        let retained_framework_edges = next_framework_relationships
            .values()
            .map(|contribution| contribution.edges.len() as u64)
            .sum::<u64>();
        let omitted_framework_edges = next_framework_relationships
            .values()
            .map(|contribution| contribution.omitted_edges)
            .sum::<u64>();
        if retained_edges.saturating_add(retained_framework_edges) > graph_limits.max_edges
            || omitted_edges.saturating_add(omitted_framework_edges) > 0
        {
            next_relationships = build_all_relationships(
                &next_files,
                &catalog,
                graph_limits.max_edges,
                cancellation,
            )?;
            let retained = next_relationships
                .values()
                .map(|contribution| contribution.edges.len() as u64)
                .sum::<u64>();
            next_framework_relationships = build_all_framework_relationships(
                &next_files,
                &next_framework_files,
                &catalog,
                graph_limits.max_edges.saturating_sub(retained),
                cancellation,
            )?;
            metrics.relationship_files_recomputed = next_files.len() as u64;
            metrics.framework_relationship_files_recomputed = next_framework_files.len() as u64;
        }
        let mut all_relationship_owners = affected_owners.clone();
        all_relationship_owners.extend(affected_framework_owners);
        let next = Self {
            files: next_files,
            metadata,
            relationships: next_relationships,
            laravel_detected: self.laravel_detected,
            framework_files: next_framework_files,
            framework_relationships: next_framework_relationships,
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
        let delta_candidate = !limits_changed
            && !metadata_changed
            && !framework_symbols_changed
            && complete_previous
            && delta_fits;
        let delta = if delta_candidate {
            next.materialize_graph_delta(
                &self.graph,
                &changed_paths,
                &all_relationship_owners,
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
                &all_relationship_owners,
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
            laravel_detected: next.laravel_detected,
            framework_symbols: next.framework_symbol_count(),
            framework_edges: next.framework_edge_count(),
            framework_truncated_files: next.framework_truncated_files(),
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
                    metrics
                        .relationship_files_recomputed
                        .saturating_add(metrics.framework_relationship_files_recomputed),
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
        metrics.framework_truncated_files = next.framework_truncated_files();
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
            .chain(self.framework_relationships.values())
            .map(|contribution| contribution.omitted_edges)
            .sum()
    }

    fn framework_truncated_files(&self) -> u64 {
        self.framework_files
            .values()
            .filter(|file| file.truncated)
            .count() as u64
    }

    fn framework_symbol_count(&self) -> u64 {
        self.framework_files
            .values()
            .map(|file| file.symbols.len() as u64)
            .sum()
    }

    fn framework_edge_count(&self) -> u64 {
        self.framework_relationships
            .values()
            .map(|contribution| contribution.edges.len() as u64)
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
                .sum::<u64>()
                .saturating_add(self.framework_symbol_count()),
            relationship_edges: self
                .relationships
                .values()
                .map(|relationships| relationships.edges.len() as u64)
                .sum::<u64>()
                .saturating_add(self.framework_edge_count()),
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
    ) -> Result<(SymbolGraph, GraphBuildReport), PhpIndexError> {
        let mut graph = BoundedGraphBuilder::new(self.graph_limits);
        let catalog = SymbolCatalog::new(&self.files);
        let mut method_qualifiers = HashMap::new();
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
            if let Some(framework) = self.framework_files.get(path) {
                for (index, symbol) in framework.symbols.iter().enumerate() {
                    let id = graph.add_symbol(
                        SymbolKey {
                            language: Language::Php,
                            qualified_name: symbol.qualified_name.clone(),
                            container: None,
                            kind: SymbolKind::Configuration,
                            path: path.clone(),
                        },
                        symbol.location.clone(),
                        symbol.signature.clone(),
                        Provenance::Heuristic,
                        Precision::Heuristic,
                    )?;
                    if let Some(id) = id {
                        ids.insert(framework_symbol_address(path, file, index), id);
                    }
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
        for (owner, contribution) in &self.framework_relationships {
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
                let qualifier = call_site.receiver_type.as_deref().map_or_else(
                    || call_site.qualifier.clone(),
                    |receiver_type| {
                        let key = (
                            receiver_type.to_owned(),
                            call_site.target_kind,
                            call_site.name.clone(),
                        );
                        Some(
                            method_qualifiers
                                .entry(key)
                                .or_insert_with(|| {
                                    catalog.method_qualifier(
                                        receiver_type,
                                        call_site.target_kind,
                                        &call_site.name,
                                    )
                                })
                                .clone(),
                        )
                    },
                );
                graph.add_call_site(CallSiteInput {
                    caller: *caller,
                    form: call_site.form,
                    target_kind: call_site.target_kind,
                    name: call_site.name.clone(),
                    qualifier,
                    receiver_type: call_site.receiver_type.clone(),
                    receiver_type_source: call_site.receiver_type_source,
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
    ) -> Result<Option<(SymbolGraph, GraphBuildReport)>, PhpIndexError> {
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
                let file = self
                    .files
                    .get(path)
                    .ok_or_else(|| PhpIndexError::Update(format!("missing changed file {path}")))?;
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
                let framework_ids: Vec<_> = graph
                    .symbols_in_file(path)
                    .skip(file.symbols.len())
                    .map(|symbol| symbol.id)
                    .collect();
                for (id, symbol) in framework_ids.into_iter().zip(
                    self.framework_files
                        .get(path)
                        .into_iter()
                        .flat_map(|framework| &framework.symbols),
                ) {
                    graph.replace_symbol_payload(
                        id,
                        SymbolKey {
                            language: Language::Php,
                            qualified_name: symbol.qualified_name.clone(),
                            container: None,
                            kind: SymbolKind::Configuration,
                            path: path.clone(),
                        },
                        symbol.location.clone(),
                        symbol.signature.clone(),
                        Provenance::Heuristic,
                        Precision::Heuristic,
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
            if let Some(framework) = self.framework_files.get(path) {
                for symbol in &framework.symbols {
                    graph.add_symbol(
                        SymbolKey {
                            language: Language::Php,
                            qualified_name: symbol.qualified_name.clone(),
                            container: None,
                            kind: SymbolKind::Configuration,
                            path: path.clone(),
                        },
                        symbol.location.clone(),
                        symbol.signature.clone(),
                        Provenance::Heuristic,
                        Precision::Heuristic,
                    )?;
                }
            }
        }
        for owner in relationship_owners {
            check_cancelled(cancellation)?;
            for contribution in [
                self.relationships.get(owner),
                self.framework_relationships.get(owner),
            ]
            .into_iter()
            .flatten()
            {
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
        }
        let catalog = SymbolCatalog::new(&self.files);
        let mut method_qualifiers = HashMap::new();
        for path in call_owners {
            check_cancelled(cancellation)?;
            let Some(file) = self.files.get(path) else {
                continue;
            };
            for call_site in &file.calls {
                let Some(caller) = graph.symbols_in_file(path).nth(call_site.caller) else {
                    continue;
                };
                let qualifier = call_site.receiver_type.as_deref().map_or_else(
                    || call_site.qualifier.clone(),
                    |receiver_type| {
                        let key = (
                            receiver_type.to_owned(),
                            call_site.target_kind,
                            call_site.name.clone(),
                        );
                        Some(
                            method_qualifiers
                                .entry(key)
                                .or_insert_with(|| {
                                    catalog.method_qualifier(
                                        receiver_type,
                                        call_site.target_kind,
                                        &call_site.name,
                                    )
                                })
                                .clone(),
                        )
                    },
                );
                graph.add_call_site(CallSiteInput {
                    caller: caller.id,
                    form: call_site.form,
                    target_kind: call_site.target_kind,
                    name: call_site.name.clone(),
                    qualifier,
                    receiver_type: call_site.receiver_type.clone(),
                    receiver_type_source: call_site.receiver_type_source,
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
                (next.symbols.len() as u64).saturating_add(
                    self.framework_files
                        .get(path)
                        .map_or(0, |framework| framework.symbols.len() as u64),
                )
            })
            .sum();
        let rebuilt_relationship_edges: u64 = relationship_owners
            .iter()
            .map(|path| {
                self.relationships
                    .get(path)
                    .map_or(0, |relationships| relationships.edges.len() as u64)
                    .saturating_add(
                        self.framework_relationships
                            .get(path)
                            .map_or(0, |relationships| relationships.edges.len() as u64),
                    )
            })
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
) -> Result<(BTreeMap<RepoRelativePath, Arc<ParsedFile>>, ParseSchedule), PhpIndexError> {
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
        let mut parser = PhpParser::new().map_err(parse_error)?;
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
    let outputs = thread::scope(|scope| -> Result<Vec<ParseWorkerOutput>, PhpIndexError> {
        let mut workers = Vec::with_capacity(worker_count);
        let mut spawn_error = None;
        for worker in 0..worker_count {
            let name = format!("chakra-php-parser-{worker}");
            match thread::Builder::new().name(name).spawn_scoped(scope, || {
                let active = active_workers.fetch_add(1, Ordering::Relaxed) + 1;
                peak_active_workers.fetch_max(active, Ordering::Relaxed);
                let output = (|| {
                    let mut parser = match PhpParser::new() {
                        Ok(parser) => parser,
                        Err(error) => {
                            return ParseWorkerOutput {
                                parser_error: Some(error.to_string()),
                                parsed: Vec::new(),
                            };
                        }
                    };
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
            return Err(PhpIndexError::WorkerPanicked);
        }
        if let Some(error) = spawn_error {
            return Err(PhpIndexError::WorkerSpawn(error));
        }
        Ok(outputs)
    })?;
    check_cancelled(cancellation)?;
    if let Some(error) = outputs
        .iter()
        .find_map(|output| output.parser_error.clone())
    {
        return Err(PhpIndexError::Parse(error));
    }

    let mut ordered: Vec<Option<Result<ParsedFile, String>>> =
        std::iter::repeat_with(|| None).take(inputs.len()).collect();
    for output in outputs {
        for (index, parsed) in output.parsed {
            let Some(slot) = ordered.get_mut(index) else {
                return Err(PhpIndexError::Update(
                    "parser worker returned an out-of-range source index".to_owned(),
                ));
            };
            *slot = Some(parsed);
        }
    }
    let mut files = BTreeMap::new();
    for ((path, _), parsed) in inputs.into_iter().zip(ordered) {
        let parsed = parsed.ok_or_else(|| {
            PhpIndexError::Update("parser worker omitted an admitted source".to_owned())
        })?;
        let parsed = parsed.map_err(PhpIndexError::Parse)?;
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

fn check_cancelled(cancellation: &IndexCancellation) -> Result<(), PhpIndexError> {
    if cancellation.is_cancelled() {
        Err(PhpIndexError::Cancelled)
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
        language: Some(Language::Php),
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
        language: Some(Language::Php),
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
) -> Result<BTreeMap<RepoRelativePath, Arc<RelationshipContribution>>, PhpIndexError> {
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

fn parse_framework_files(
    files: &BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
    laravel_detected: bool,
    cancellation: &IndexCancellation,
) -> Result<BTreeMap<RepoRelativePath, Arc<LaravelFile>>, PhpIndexError> {
    if !laravel_detected {
        return Ok(BTreeMap::new());
    }
    let mut parser = LaravelParser::new().map_err(parse_error)?;
    let mut framework_files = BTreeMap::new();
    for (path, file) in files {
        check_cancelled(cancellation)?;
        let parsed = parser
            .parse(path.clone(), file.source.as_ref())
            .map_err(parse_error)?;
        framework_files.insert(path.clone(), Arc::new(parsed));
    }
    Ok(framework_files)
}

fn build_all_framework_relationships(
    files: &BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
    framework_files: &BTreeMap<RepoRelativePath, Arc<LaravelFile>>,
    catalog: &SymbolCatalog,
    limit: u64,
    cancellation: &IndexCancellation,
) -> Result<BTreeMap<RepoRelativePath, Arc<RelationshipContribution>>, PhpIndexError> {
    let mut relationships = BTreeMap::new();
    let mut retained = 0_u64;
    for (path, framework) in framework_files {
        check_cancelled(cancellation)?;
        let Some(file) = files.get(path) else {
            continue;
        };
        let contribution = framework_relationships_for_file(
            path,
            file,
            framework,
            catalog,
            limit.saturating_sub(retained),
        );
        retained = retained.saturating_add(contribution.edges.len() as u64);
        relationships.insert(path.clone(), Arc::new(contribution));
    }
    Ok(relationships)
}

fn framework_symbol_address(
    path: &RepoRelativePath,
    file: &ParsedFile,
    index: usize,
) -> SymbolAddress {
    SymbolAddress {
        path: path.clone(),
        index: file.symbols.len().saturating_add(index),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FrameworkLookup {
    Missing,
    Unique(SymbolAddress),
    Ambiguous,
}

fn lookup_framework_selector(
    catalog: &SymbolCatalog,
    selector: &FrameworkSelector,
) -> FrameworkLookup {
    let mut matches = Vec::new();
    for kind in &selector.kinds {
        if let Some(addresses) = catalog.exact.get(&(selector.qualified_name.clone(), *kind)) {
            matches.extend(addresses.iter().cloned());
        }
    }
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [] => FrameworkLookup::Missing,
        [address] => FrameworkLookup::Unique(address.clone()),
        _ => FrameworkLookup::Ambiguous,
    }
}

fn framework_endpoint_address(
    endpoint: &FrameworkEndpoint,
    path: &RepoRelativePath,
    file: &ParsedFile,
    framework: &LaravelFile,
    catalog: &SymbolCatalog,
) -> Option<SymbolAddress> {
    match endpoint {
        FrameworkEndpoint::Synthetic(index) => framework
            .symbols
            .get(*index)
            .map(|_| framework_symbol_address(path, file, *index)),
        FrameworkEndpoint::Existing(alternatives) => {
            for selector in alternatives {
                match lookup_framework_selector(catalog, selector) {
                    FrameworkLookup::Missing => {}
                    FrameworkLookup::Unique(address) => return Some(address),
                    FrameworkLookup::Ambiguous => return None,
                }
            }
            None
        }
    }
}

fn add_framework_dependencies(
    endpoint: &FrameworkEndpoint,
    dependencies: &mut HashSet<DependencyKey>,
) {
    if let FrameworkEndpoint::Existing(alternatives) = endpoint {
        for selector in alternatives {
            for kind in &selector.kinds {
                dependencies.insert(DependencyKey::Exact(selector.qualified_name.clone(), *kind));
            }
        }
    }
}

fn framework_relationships_for_file(
    path: &RepoRelativePath,
    file: &ParsedFile,
    framework: &LaravelFile,
    catalog: &SymbolCatalog,
    edge_limit: u64,
) -> RelationshipContribution {
    let mut contribution = RelationshipContribution::default();
    for relation in &framework.relations {
        add_framework_dependencies(&relation.from, &mut contribution.dependencies);
        add_framework_dependencies(&relation.to, &mut contribution.dependencies);
        let (Some(from), Some(to)) = (
            framework_endpoint_address(&relation.from, path, file, framework, catalog),
            framework_endpoint_address(&relation.to, path, file, framework, catalog),
        ) else {
            continue;
        };
        contribution.push_edge(
            RelationshipEdge {
                kind: relation.kind,
                from,
                to,
                provenance: Provenance::Heuristic,
                precision: Precision::Heuristic,
                location: Some(relation.location.clone()),
            },
            edge_limit,
        );
    }
    contribution
}

fn parse_error(error: impl std::fmt::Display) -> PhpIndexError {
    PhpIndexError::Parse(error.to_string())
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

fn symbol_keys_equal(left: &ParsedFile, right: &ParsedFile) -> bool {
    left.symbols.len() == right.symbols.len()
        && left
            .symbols
            .iter()
            .zip(&right.symbols)
            .all(|(left, right)| left.key == right.key)
}

fn syntax_facts_equal(left: &ParsedFile, right: &ParsedFile) -> bool {
    left.symbols == right.symbols
        && left.calls == right.calls
        && left.named_relations == right.named_relations
        && left.has_errors == right.has_errors
}

fn framework_symbol_keys_equal(
    left: Option<&Arc<LaravelFile>>,
    right: Option<&Arc<LaravelFile>>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.symbols.len() == right.symbols.len()
                && left
                    .symbols
                    .iter()
                    .zip(&right.symbols)
                    .all(|(left, right)| left.qualified_name == right.qualified_name)
        }
        _ => false,
    }
}

fn exported_callables(file: &ParsedFile) -> HashSet<(u8, String)> {
    file.symbols
        .iter()
        .filter_map(|symbol| {
            callable_target_kind(symbol.key.kind).map(|kind| {
                let name = symbol
                    .key
                    .qualified_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(&symbol.key.qualified_name);
                callable_dependency(kind, name)
            })
        })
        .collect()
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
        CallTargetKind::Test => 2,
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
    catalog: &SymbolCatalog,
    edge_limit: u64,
) -> RelationshipContribution {
    let mut contribution = RelationshipContribution::default();
    let address = |index| SymbolAddress {
        path: path.clone(),
        index,
    };
    for (index, symbol) in file.symbols.iter().enumerate() {
        let Some(parent) = symbol.parent.map(&address) else {
            continue;
        };
        contribution.push_edge(
            RelationshipEdge {
                kind: EdgeKind::Contains,
                from: parent,
                to: address(index),
                provenance: Provenance::TreeSitter,
                precision: Precision::Syntax,
                location: None,
            },
            edge_limit,
        );
    }
    for relation in &file.named_relations {
        for target_kind in &relation.target_kinds {
            contribution
                .dependencies
                .insert(DependencyKey::Exact(relation.target.clone(), *target_kind));
            let Some(target) = catalog.unique_exact(&relation.target, *target_kind) else {
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
            break;
        }
    }
    contribution
}

fn read_sources(repository_root: &Path) -> Result<PhpSources, PhpIndexError> {
    let files = chakra_git::discover_classified_sources(repository_root, Language::Php)?;
    let mut sources = BTreeMap::new();
    let mut metadata = BTreeMap::new();
    let mut total_bytes = 0_usize;
    for discovered in files {
        let path = discovered.path;
        let file = fs::File::open(repository_root.join(path.as_str())).map_err(|source| {
            PhpIndexError::Read {
                path: path.clone(),
                source,
            }
        })?;
        let mut source = String::new();
        file.take((MAX_SOURCE_FILE_BYTES + 1) as u64)
            .read_to_string(&mut source)
            .map_err(|source| PhpIndexError::Read {
                path: path.clone(),
                source,
            })?;
        if source.len() > MAX_SOURCE_FILE_BYTES {
            return Err(PhpIndexError::SourceTooLarge {
                path,
                limit: MAX_SOURCE_FILE_BYTES,
            });
        }
        total_bytes =
            total_bytes
                .checked_add(source.len())
                .ok_or(PhpIndexError::RepositoryTooLarge {
                    limit: MAX_REPOSITORY_SOURCE_BYTES,
                })?;
        if total_bytes > MAX_REPOSITORY_SOURCE_BYTES {
            return Err(PhpIndexError::RepositoryTooLarge {
                limit: MAX_REPOSITORY_SOURCE_BYTES,
            });
        }
        metadata.insert(path.clone(), discovered.metadata);
        sources.insert(path, Arc::<str>::from(source));
    }
    Ok(PhpSources {
        files: sources,
        metadata,
    })
}

fn composer_declares_laravel(repository_root: &Path) -> Result<bool, PhpIndexError> {
    let path = repository_root.join("composer.json");
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(PhpIndexError::ComposerRead(error)),
    };
    let mut source = String::new();
    file.take((MAX_COMPOSER_METADATA_BYTES + 1) as u64)
        .read_to_string(&mut source)
        .map_err(PhpIndexError::ComposerRead)?;
    if source.len() > MAX_COMPOSER_METADATA_BYTES {
        return Err(PhpIndexError::ComposerMetadataTooLarge {
            limit: MAX_COMPOSER_METADATA_BYTES,
        });
    }
    let metadata: serde_json::Value =
        serde_json::from_str(&source).map_err(PhpIndexError::ComposerInvalid)?;
    let Some(require) = metadata
        .get("require")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(false);
    };
    Ok([
        "laravel/framework",
        "laravel/lumen-framework",
        "illuminate/foundation",
    ]
    .iter()
    .any(|package| require.contains_key(*package)))
}

/// Detects whether Composer metadata opts the repository into Laravel syntax
/// enrichment. Callers decide whether invalid optional metadata is fatal.
pub fn detect_laravel(repository_root: &Path) -> Result<bool, PhpIndexError> {
    composer_declares_laravel(repository_root)
}

pub fn index_repository(root: &Path) -> Result<IndexReport, PhpIndexError> {
    let started = Instant::now();
    let repository_root = chakra_git::resolve_repository_root(root)?;
    let span = info_span!("php_repository_index", root = %repository_root.display());
    let _entered = span.enter();
    let laravel_detected = match composer_declares_laravel(&repository_root) {
        Ok(detected) => detected,
        Err(error) => {
            warn!(
                error = %error,
                "Laravel enrichment disabled because Composer metadata is unavailable or invalid"
            );
            false
        }
    };
    let sources = read_sources(&repository_root)?;
    let discovered_files = sources.len() as u64;
    let cancellation = IndexCancellation::default();
    let (syntax_index, graph, _) = PhpSyntaxIndex::from_classified_sources_scheduled(
        sources,
        GraphBuildLimits::UNLIMITED,
        1,
        usize::MAX,
        laravel_detected,
        &cancellation,
    )?;
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
        laravel_detected,
        framework_symbols: syntax_index.framework_symbol_count(),
        framework_edges: syntax_index.framework_edge_count(),
        framework_truncated_files: syntax_index.framework_truncated_files(),
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
        laravel_detected = metrics.laravel_detected,
        framework_symbols = metrics.framework_symbols,
        framework_edges = metrics.framework_edges,
        framework_truncated_files = metrics.framework_truncated_files,
        elapsed_micros = metrics.elapsed.as_micros(),
        "PHP syntax index completed"
    );
    Ok(IndexReport {
        repository_root,
        graph,
        metrics,
        syntax_index,
    })
}

pub fn scan_repository_sources(repository_root: &Path) -> Result<PhpSources, PhpIndexError> {
    read_sources(repository_root)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::process::Command;

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
                RepoRelativePath::new(format!("src/Generated{index:03}.php"))?,
                Arc::<str>::from(format!(
                    "<?php namespace Generated\\N{index}; function caller_{index}(): void {{ helper_{index}(); }} function helper_{index}(): void {{}}\n"
                )),
            );
        }
        let cancellation = IndexCancellation::default();
        let (_, sequential, sequential_metrics) = PhpSyntaxIndex::from_sources_scheduled(
            sources.clone(),
            GraphBuildLimits::UNLIMITED,
            1,
            1,
            &cancellation,
        )?;
        let (_, parallel, parallel_metrics) = PhpSyntaxIndex::from_sources_scheduled(
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

    fn in_memory_sources(
        sources: &[(&str, &str)],
    ) -> Result<BTreeMap<RepoRelativePath, Arc<str>>, Box<dyn Error>> {
        sources
            .iter()
            .map(|(path, source)| Ok((RepoRelativePath::new(*path)?, Arc::<str>::from(*source))))
            .collect()
    }

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
    fn resolves_namespaced_php_functions_without_cross_namespace_ambiguity()
    -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::create_dir_all(repository.path().join("src"))?;
        fs::write(
            repository.path().join("src/service.php"),
            r#"<?php
namespace App;
class Service { public function refund(): void { helper(); } }
function helper(): void {}
"#,
        )?;
        fs::write(
            repository.path().join("src/other.php"),
            "<?php namespace Other; function helper(): void {}\n",
        )?;
        let report = index_repository(repository.path())?;
        assert_eq!(report.metrics.parsed_files, 2);
        let refund = report.graph.resolve_name("refund");
        assert_eq!(refund.len(), 1);
        let calls = report.graph.outgoing_edges(refund[0]);
        assert_eq!(
            calls
                .iter()
                .filter(|edge| edge.kind == EdgeKind::Calls)
                .count(),
            1
        );
        let call_site = report
            .graph
            .call_sites_from(refund[0])
            .next()
            .ok_or("helper call site missing")?;
        assert_eq!(
            call_site.resolution,
            chakra_domain::symbol::CallResolution::Resolved {
                target: calls
                    .iter()
                    .find(|edge| edge.kind == EdgeKind::Calls)
                    .ok_or("helper call edge missing")?
                    .to,
            }
        );
        let (candidates, truncated) = report.graph.call_candidates(call_site, 10);
        assert!(candidates.is_empty());
        assert!(!truncated);
        assert_eq!(call_site.precision, Precision::Syntax);
        Ok(())
    }

    #[test]
    fn unchanged_php_is_not_reparsed() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(
            repository.path().join("service.php"),
            "<?php function pay() {}\n",
        )?;
        let report = index_repository(repository.path())?;
        let sources = scan_repository_sources(repository.path())?;
        let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
        assert!(reconciled.graph.is_none());
        assert_eq!(reconciled.metrics.reparsed_files, 0);
        assert_eq!(reconciled.metrics.unchanged_files, 1);
        Ok(())
    }

    #[test]
    fn receiver_types_prevent_same_name_method_fanout() -> Result<(), Box<dyn Error>> {
        let sources = in_memory_sources(&[
            (
                "src/targets.php",
                r#"<?php
namespace App\Domain;
class TransactionStatusService { public function syncStatus(): void {} }
class ExpirePendingTransactionsJob { public function handle(): void {} }
class UnrelatedJob { public function handle(): void {} }
class UnrelatedStatusService { public function syncStatus(): void {} }
"#,
            ),
            (
                "tests/FeatureTest.php",
                r#"<?php
namespace Tests;
use App\Domain\TransactionStatusService as StatusService;
use App\Domain\ExpirePendingTransactionsJob;
class FeatureTest {
    public function parameter(StatusService $service): void { $service->syncStatus(); }
    public function locator(): void { app(StatusService::class)->syncStatus(); }
    public function local(): void { (new ExpirePendingTransactionsJob)->handle(); }
    public function dynamic($service, $job): void {
        $service->syncStatus();
        $job->handle();
    }
}
"#,
            ),
        ])?;
        let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;

        let sync_status = graph.resolve_name("App::Domain::TransactionStatusService::syncStatus");
        assert_eq!(sync_status.len(), 1);
        assert_eq!(
            graph
                .incoming_edges(sync_status[0])
                .iter()
                .filter(|edge| edge.kind == EdgeKind::Calls)
                .count(),
            2
        );
        let job_handle = graph.resolve_name("App::Domain::ExpirePendingTransactionsJob::handle");
        assert_eq!(job_handle.len(), 1);
        assert_eq!(
            graph
                .incoming_edges(job_handle[0])
                .iter()
                .filter(|edge| edge.kind == EdgeKind::Calls)
                .count(),
            1
        );
        let unrelated_handle = graph.resolve_name("App::Domain::UnrelatedJob::handle");
        assert_eq!(unrelated_handle.len(), 1);
        assert!(
            graph
                .incoming_edges(unrelated_handle[0])
                .iter()
                .all(|edge| edge.kind != EdgeKind::Calls)
        );
        assert_eq!(
            graph
                .symbols()
                .iter()
                .flat_map(|symbol| graph.call_sites_from(symbol.id))
                .filter(|call| {
                    call.resolution == chakra_domain::symbol::CallResolution::Unresolved
                        && matches!(call.name.as_str(), "syncStatus" | "handle")
                })
                .count(),
            2
        );
        Ok(())
    }

    #[test]
    fn php_test_relations_require_resolved_receivers_and_are_deduplicated()
    -> Result<(), Box<dyn Error>> {
        let sources = in_memory_sources(&[
            (
                "src/targets.php",
                r#"<?php
namespace App\Domain;
class TransactionStatusService { public function syncStatus(): void {} }
class ExpirePendingTransactionsJob { public function handle(): void {} }
class UnrelatedJob { public function handle(): void {} }
class UnrelatedStatusService { public function syncStatus(): void {} }
"#,
            ),
            (
                "tests/RelationshipTest.php",
                r#"<?php
namespace Tests;
use App\Domain\TransactionStatusService;
use App\Domain\ExpirePendingTransactionsJob;
class ExpirationTest {
    public function testExpiresPending(ExpirePendingTransactionsJob $job): void {
        $job->handle();
        $job->handle();
    }
    public function testDynamicHandle($job): void { $job->handle(); }
}
class StatusTest {
    public function testSyncStatus(TransactionStatusService $service): void {
        $service->syncStatus();
    }
}
"#,
            ),
        ])?;
        let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;

        let job_handle = graph.resolve_name("App::Domain::ExpirePendingTransactionsJob::handle");
        assert_eq!(job_handle.len(), 1);
        let job_calls: Vec<_> = graph
            .incoming_edges(job_handle[0])
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .collect();
        assert_eq!(job_calls.len(), 2);
        let job_tests: Vec<_> = graph
            .incoming_edges(job_handle[0])
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Tests)
            .collect();
        assert_eq!(job_tests.len(), 1);
        let test_symbol = graph
            .symbol(job_tests[0].from)
            .ok_or("expiration test symbol missing")?;
        assert_eq!(
            test_symbol.key.qualified_name,
            "Tests::ExpirationTest::testExpiresPending"
        );
        let evidence = graph
            .call_site_for_edge(job_tests[0])
            .ok_or("representative test call-site evidence missing")?;
        assert_eq!(
            evidence.receiver_type.as_deref(),
            Some("App::Domain::ExpirePendingTransactionsJob")
        );
        assert_eq!(
            evidence.receiver_type_source,
            Some(chakra_domain::symbol::ReceiverTypeSource::Parameter)
        );

        let sync_status = graph.resolve_name("App::Domain::TransactionStatusService::syncStatus");
        assert_eq!(sync_status.len(), 1);
        assert_eq!(
            graph
                .incoming_edges(sync_status[0])
                .iter()
                .filter(|edge| edge.kind == EdgeKind::Tests)
                .count(),
            1
        );

        for unrelated in [
            "App::Domain::UnrelatedJob::handle",
            "App::Domain::UnrelatedStatusService::syncStatus",
        ] {
            let target = graph.resolve_name(unrelated);
            assert_eq!(target.len(), 1);
            assert!(
                graph
                    .incoming_edges(target[0])
                    .iter()
                    .all(|edge| edge.kind != EdgeKind::Tests)
            );
        }

        let dynamic_test = graph
            .resolve_name("Tests::ExpirationTest::testDynamicHandle")
            .into_iter()
            .next()
            .ok_or("dynamic test symbol missing")?;
        assert!(
            graph
                .outgoing_edges(dynamic_test)
                .iter()
                .all(|edge| edge.kind != EdgeKind::Tests)
        );
        let dynamic_call = graph
            .call_sites_from(dynamic_test)
            .find(|call| call.name == "handle")
            .ok_or("dynamic test call site missing")?;
        assert_eq!(
            dynamic_call.resolution,
            chakra_domain::symbol::CallResolution::Unresolved
        );
        Ok(())
    }

    #[test]
    fn resolves_php_method_precedence_across_inheritance_and_traits() -> Result<(), Box<dyn Error>>
    {
        let sources = in_memory_sources(&[
            (
                "src/types.php",
                r#"<?php
namespace App;
interface Runner {
    public function run(): void;
    public function fromParent(): void;
}
trait SharedBehavior { public function fromTrait(): void {} }
class ParentWorker {
    public function fromParent(): void {}
    public function fromTrait(): void {}
}
class ChildWorker extends ParentWorker implements Runner {
    use SharedBehavior;
    public function run(): void {}
}
"#,
            ),
            (
                "src/caller.php",
                r#"<?php
namespace App;
class Caller {
    public function call(ChildWorker $worker, Runner $contract): void {
        $worker->run();
        $worker->fromParent();
        $worker->fromTrait();
        $contract->run();
    }
}
"#,
            ),
        ])?;
        let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;

        for (target, expected) in [
            ("App::ChildWorker::run", 1),
            ("App::Runner::run", 1),
            ("App::ParentWorker::fromParent", 1),
            ("App::SharedBehavior::fromTrait", 1),
            ("App::Runner::fromParent", 0),
            ("App::ParentWorker::fromTrait", 0),
        ] {
            let ids = graph.resolve_name(target);
            assert_eq!(ids.len(), 1, "missing target {target}");
            assert_eq!(
                graph
                    .incoming_edges(ids[0])
                    .iter()
                    .filter(|edge| edge.kind == EdgeKind::Calls)
                    .count(),
                expected,
                "unexpected callers for {target}"
            );
        }
        Ok(())
    }

    #[test]
    fn receiver_resolution_republishes_without_reparsing_unchanged_callers()
    -> Result<(), Box<dyn Error>> {
        let initial = in_memory_sources(&[
            (
                "src/service.php",
                "<?php namespace App; class Service { public function run(): void {} }",
            ),
            (
                "src/caller.php",
                "<?php namespace App; class Caller { public function call(Service $service): void { $service->run(); } }",
            ),
        ])?;
        let (index, graph) = PhpSyntaxIndex::from_sources(initial)?;
        let run = graph.resolve_name("App::Service::run");
        assert_eq!(run.len(), 1);
        assert_eq!(
            graph
                .incoming_edges(run[0])
                .iter()
                .filter(|edge| edge.kind == EdgeKind::Calls)
                .count(),
            1
        );

        let changed = in_memory_sources(&[
            (
                "src/service.php",
                "<?php namespace App; class Service { public function renamed(): void {} }",
            ),
            (
                "src/caller.php",
                "<?php namespace App; class Caller { public function call(Service $service): void { $service->run(); } }",
            ),
        ])?;
        let reconciled = index.reconcile_sources(changed)?;
        assert_eq!(reconciled.metrics.reparsed_files, 1);
        assert_eq!(reconciled.metrics.relationship_files_recomputed, 1);
        let graph = reconciled.graph.ok_or("changed graph missing")?;
        assert!(graph.resolve_name("App::Service::run").is_empty());
        let call = graph
            .symbols()
            .iter()
            .flat_map(|symbol| graph.call_sites_from(symbol.id))
            .find(|call| call.name == "run")
            .ok_or("run call site missing")?;
        assert_eq!(
            call.resolution,
            chakra_domain::symbol::CallResolution::Unresolved
        );
        Ok(())
    }

    #[test]
    fn synthetic_receiver_resolution_records_bounded_measurement() -> Result<(), Box<dyn Error>> {
        const CALLS: usize = 256;
        let mut source = String::from("<?php namespace Bench;\n");
        for index in 0..CALLS {
            source.push_str(&format!(
                "class Service{index} {{ public function call{index}(): void {{}} }}\n"
            ));
        }
        source.push_str("class Caller { public function invoke(");
        for index in 0..CALLS {
            if index > 0 {
                source.push(',');
            }
            source.push_str(&format!("Service{index} $service{index}"));
        }
        source.push_str("): void {\n");
        for index in 0..CALLS {
            source.push_str(&format!("$service{index}->call{index}();\n"));
        }
        source.push_str("}}\n");
        let sources = BTreeMap::from([(
            RepoRelativePath::new("bench.php")?,
            Arc::<str>::from(source),
        )]);

        let started = Instant::now();
        let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;
        let elapsed = started.elapsed();
        assert_eq!(graph.call_site_count(), CALLS as u64);
        assert_eq!(graph.unresolved_call_site_count(), 0);
        assert_eq!(graph.ambiguous_call_site_count(), 0);
        eprintln!(
            "php_receiver_resolution: calls={CALLS}, symbols={}, edges={}, elapsed={elapsed:?}",
            graph.symbol_count(),
            graph.edge_count()
        );
        Ok(())
    }

    #[test]
    fn malformed_cyclic_hierarchy_terminates_without_inventing_a_target()
    -> Result<(), Box<dyn Error>> {
        let sources = in_memory_sources(&[(
            "cycle.php",
            r#"<?php
namespace App;
class CycleA extends CycleB {}
class CycleB extends CycleA {}
class Caller { public function call(CycleA $value): void { $value->missing(); } }
"#,
        )])?;
        let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;
        let call = graph
            .symbols()
            .iter()
            .flat_map(|symbol| graph.call_sites_from(symbol.id))
            .find(|call| call.name == "missing")
            .ok_or("missing call site not retained")?;
        assert_eq!(
            call.resolution,
            chakra_domain::symbol::CallResolution::Unresolved
        );
        Ok(())
    }

    #[test]
    fn laravel_fact_extraction_stops_at_the_per_file_budget() -> Result<(), Box<dyn Error>> {
        let mut source = String::from(
            r#"<?php
use Illuminate\Support\Facades\Route;
final class Controller { public function __invoke(): void {} }
"#,
        );
        for index in 0..1_100 {
            source.push_str(&format!(
                "Route::get('/route-{index}', Controller::class);\n"
            ));
        }
        let sources = BTreeMap::from([(
            RepoRelativePath::new("routes/web.php")?,
            Arc::<str>::from(source),
        )]);
        let (index, graph) = PhpSyntaxIndex::from_sources_with_laravel(sources, true)?;
        assert_eq!(index.framework_truncated_files(), 1);
        assert!(index.framework_symbol_count() <= 2_048);
        assert!(index.framework_edge_count() <= 2_048);
        assert!(
            graph
                .symbols()
                .iter()
                .filter(|symbol| symbol.key.kind == SymbolKind::Configuration)
                .count()
                <= 2_048
        );
        Ok(())
    }

    #[test]
    fn invalid_composer_metadata_disables_only_optional_laravel_enrichment()
    -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(repository.path().join("composer.json"), "{ invalid json")?;
        fs::write(
            repository.path().join("service.php"),
            "<?php function stillIndexed(): void {}",
        )?;
        let report = index_repository(repository.path())?;
        assert!(!report.metrics.laravel_detected);
        assert_eq!(report.metrics.framework_edges, 0);
        assert_eq!(report.graph.resolve_name("stillIndexed").len(), 1);
        Ok(())
    }

    #[test]
    fn composer_metadata_change_republishes_without_reparsing() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::create_dir_all(repository.path().join("app"))?;
        fs::write(
            repository.path().join("composer.json"),
            r#"{"name":"before/package","autoload":{"psr-4":{"App\\":"app/"}}}"#,
        )?;
        fs::write(
            repository.path().join("app/Service.php"),
            "<?php namespace App; class Service {}\n",
        )?;
        let report = index_repository(repository.path())?;
        let path = RepoRelativePath::new("app/Service.php")?;
        assert_eq!(
            report
                .graph
                .file_metadata(&path)
                .and_then(|metadata| metadata.package.as_ref())
                .map(|package| package.name.as_str()),
            Some("before/package")
        );

        fs::write(
            repository.path().join("composer.json"),
            r#"{"name":"after/package","autoload":{"psr-4":{"App\\":"app/"}}}"#,
        )?;
        let sources = scan_repository_sources(repository.path())?;
        let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
        assert_eq!(reconciled.metrics.reparsed_files, 0);
        let graph = reconciled.graph.ok_or("metadata-only graph missing")?;
        assert_eq!(
            graph
                .file_metadata(&path)
                .and_then(|metadata| metadata.package.as_ref())
                .map(|package| package.name.as_str()),
            Some("after/package")
        );
        Ok(())
    }
}
