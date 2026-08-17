//! Deterministic PHP syntax indexing with reusable per-file facts.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::indexing::{IndexCancellation, IndexPhase, IndexPhaseMeasurement};
use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{EdgeKind, Language, SymbolKind};
use chakra_engine::{
    BoundedGraphBuilder, CallSiteInput, ConsistencyError, GraphBuildLimits, GraphBuildReport,
    GraphError, SymbolGraph,
};
use thiserror::Error;
use tracing::{info, info_span};

use crate::parser::{ParsedFile, PhpParser};

const MAX_SOURCE_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REPOSITORY_SOURCE_BYTES: usize = 256 * 1024 * 1024;

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileMetrics {
    pub scanned_files: u64,
    pub unchanged_files: u64,
    pub reparsed_files: u64,
    pub created_files: u64,
    pub modified_files: u64,
    pub deleted_files: u64,
    pub relationship_files_recomputed: u64,
    pub syntax_error_files: u64,
    pub truncated_call_sites: u64,
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
    pub phases: Vec<IndexPhaseMeasurement>,
}

#[derive(Debug)]
pub struct IndexReport {
    pub repository_root: PathBuf,
    pub graph: SymbolGraph,
    pub metrics: IndexMetrics,
    pub syntax_index: PhpSyntaxIndex,
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
    #[error("failed to parse PHP source: {0}")]
    Parse(String),
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
        let matches = self.exact.get(&(qualified_name.to_owned(), kind))?;
        (matches.len() == 1).then(|| matches[0].clone())
    }
}

#[derive(Debug, Clone)]
pub struct PhpSyntaxIndex {
    files: BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
    relationships: BTreeMap<RepoRelativePath, Arc<RelationshipContribution>>,
    graph_limits: GraphBuildLimits,
}

impl Default for PhpSyntaxIndex {
    fn default() -> Self {
        Self {
            files: BTreeMap::new(),
            relationships: BTreeMap::new(),
            graph_limits: GraphBuildLimits::UNLIMITED,
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
        check_cancelled(cancellation)?;
        let mut parser = PhpParser::new().map_err(parse_error)?;
        let mut files = BTreeMap::new();
        let parse_started = Instant::now();
        for (path, source) in sources {
            check_cancelled(cancellation)?;
            let parsed = parser.parse(path.clone(), source).map_err(parse_error)?;
            files.insert(path, Arc::new(parsed));
        }
        let parse_elapsed = parse_started.elapsed();
        check_cancelled(cancellation)?;
        let catalog_started = Instant::now();
        let catalog = SymbolCatalog::new(&files);
        let catalog_elapsed = catalog_started.elapsed();
        let relationships_started = Instant::now();
        let relationships =
            build_all_relationships(&files, &catalog, graph_limits.max_edges, cancellation)?;
        let relationships_elapsed = relationships_started.elapsed();
        let index = Self {
            files,
            relationships,
            graph_limits,
        };
        let facts = index.fact_counts();
        let materialize_started = Instant::now();
        let (graph, graph_report) = index.materialize_graph_bounded(cancellation)?;
        let materialize_elapsed = materialize_started.elapsed();
        let phases = vec![
            phase(
                IndexPhase::ParseExtraction,
                parse_elapsed,
                facts.files,
                facts.source_bytes,
            ),
            phase(IndexPhase::SymbolCatalog, catalog_elapsed, facts.symbols, 0),
            phase(
                IndexPhase::Relationships,
                relationships_elapsed,
                facts
                    .relationship_edges
                    .saturating_add(facts.omitted_relationship_edges),
                0,
            ),
            phase(
                IndexPhase::GraphMaterialization,
                materialize_elapsed,
                graph_report
                    .retained_symbols
                    .saturating_add(graph_report.retained_edges)
                    .saturating_add(graph_report.retained_call_sites),
                0,
            ),
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
        if changed_paths.is_empty() && !limits_changed {
            metrics.syntax_error_files = self.syntax_error_files();
            metrics.truncated_call_sites = self.truncated_call_sites();
            return Ok(ReconcileReport {
                graph: None,
                metrics,
                next_index: None,
                build_metrics: None,
            });
        }

        let mut next_files = self.files.clone();
        let mut changed_dependencies = HashSet::new();
        for path in &changed_paths {
            if let Some(previous) = self.files.get(path) {
                changed_dependencies.extend(exported_dependencies(previous));
            }
        }
        let parse_started = Instant::now();
        let mut parser = PhpParser::new().map_err(parse_error)?;
        for path in &changed_paths {
            check_cancelled(cancellation)?;
            match sources.get(path) {
                Some(source) => {
                    let parsed = parser
                        .parse(path.clone(), source.clone())
                        .map_err(parse_error)?;
                    changed_dependencies.extend(exported_dependencies(&parsed));
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

        let mut affected_owners = changed_paths.clone();
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
            relationships: next_relationships,
            graph_limits,
        };
        let materialize_started = Instant::now();
        let (graph, graph_report) = next.materialize_graph_bounded(cancellation)?;
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
                    graph_report
                        .retained_symbols
                        .saturating_add(graph_report.retained_edges)
                        .saturating_add(graph_report.retained_call_sites),
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
    ) -> Result<(SymbolGraph, GraphBuildReport), PhpIndexError> {
        let mut graph = BoundedGraphBuilder::new(self.graph_limits);
        let mut ids = BTreeMap::new();
        for (path, file) in &self.files {
            check_cancelled(cancellation)?;
            graph.add_file(path.clone(), file.source.clone())?;
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
        for contribution in self.relationships.values() {
            check_cancelled(cancellation)?;
            graph.omit_edges_for_edge_budget(contribution.omitted_edges);
            for edge in &contribution.edges {
                let (Some(from), Some(to)) = (ids.get(&edge.from), ids.get(&edge.to)) else {
                    graph.omit_edges_for_symbol_budget(1);
                    continue;
                };
                graph.add_edge(
                    edge.kind,
                    *from,
                    *to,
                    edge.provenance,
                    edge.precision,
                    edge.location.clone(),
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
                    receiver_hint: call_site.receiver_hint.clone(),
                    location: call_site.location.clone(),
                    provenance: Provenance::TreeSitter,
                    precision: Precision::Syntax,
                })?;
            }
        }
        let (mut graph, report) = graph.finish();
        graph.set_truncated_call_sites(report.omitted_call_sites);
        Ok((graph, report))
    }
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
        work_items,
        bytes,
    }
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

fn read_sources(
    repository_root: &Path,
) -> Result<BTreeMap<RepoRelativePath, Arc<str>>, PhpIndexError> {
    let files = chakra_git::discover_language_files(repository_root, Language::Php)?;
    let mut sources = BTreeMap::new();
    let mut total_bytes = 0_usize;
    for path in files {
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
        sources.insert(path, Arc::<str>::from(source));
    }
    Ok(sources)
}

pub fn index_repository(root: &Path) -> Result<IndexReport, PhpIndexError> {
    let started = Instant::now();
    let repository_root = chakra_git::resolve_repository_root(root)?;
    let span = info_span!("php_repository_index", root = %repository_root.display());
    let _entered = span.enter();
    let sources = read_sources(&repository_root)?;
    let discovered_files = sources.len() as u64;
    let (syntax_index, graph) = PhpSyntaxIndex::from_sources(sources)?;
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
        "PHP syntax index completed"
    );
    Ok(IndexReport {
        repository_root,
        graph,
        metrics,
        syntax_index,
    })
}

pub fn scan_repository_sources(
    repository_root: &Path,
) -> Result<BTreeMap<RepoRelativePath, Arc<str>>, PhpIndexError> {
    read_sources(repository_root)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::process::Command;

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
    fn indexes_php_relationships_and_ambiguity_without_claiming_precision()
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
            0
        );
        let call_site = report
            .graph
            .call_sites_from(refund[0])
            .next()
            .ok_or("helper call site missing")?;
        assert_eq!(
            call_site.resolution,
            chakra_domain::symbol::CallResolution::Ambiguous { candidates: 2 }
        );
        let (candidates, truncated) = report.graph.call_candidates(call_site, 10);
        assert_eq!(candidates.len(), 2);
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
        let reconciled = report.syntax_index.reconcile_sources(sources)?;
        assert!(reconciled.graph.is_none());
        assert_eq!(reconciled.metrics.reparsed_files, 0);
        assert_eq!(reconciled.metrics.unchanged_files, 1);
        Ok(())
    }
}
