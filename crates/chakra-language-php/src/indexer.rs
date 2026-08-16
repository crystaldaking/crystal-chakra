//! Deterministic PHP syntax indexing with reusable per-file facts.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{EdgeKind, EntityId, Language, SymbolKind};
use chakra_engine::{ConsistencyError, GraphError, SymbolGraph};
use thiserror::Error;
use tracing::{info, info_span};

use crate::parser::{ParsedFile, PhpParser, SymbolDraft};

const MAX_CANDIDATES_PER_CALL_SITE: usize = 64;
const MAX_SOURCE_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REPOSITORY_SOURCE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexMetrics {
    pub discovered_files: u64,
    pub parsed_files: u64,
    pub syntax_error_files: u64,
    pub truncated_call_sites: u64,
    pub symbols: u64,
    pub edges: u64,
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
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SymbolAddress {
    path: RepoRelativePath,
    index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DependencyKey {
    Exact(String, SymbolKind),
    CallableName(String),
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
    truncated_call_sites: u64,
}

#[derive(Debug)]
struct SymbolCatalog<'a> {
    files: &'a BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
    exact: HashMap<(String, SymbolKind), Vec<SymbolAddress>>,
    callable: HashMap<String, Vec<SymbolAddress>>,
}

impl<'a> SymbolCatalog<'a> {
    fn new(files: &'a BTreeMap<RepoRelativePath, Arc<ParsedFile>>) -> Self {
        let mut exact: HashMap<(String, SymbolKind), Vec<SymbolAddress>> = HashMap::new();
        let mut callable: HashMap<String, Vec<SymbolAddress>> = HashMap::new();
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
                if is_callable(symbol.key.kind) {
                    callable
                        .entry(symbol_name(symbol).to_owned())
                        .or_default()
                        .push(address);
                }
            }
        }
        for addresses in exact.values_mut() {
            addresses.sort();
        }
        for addresses in callable.values_mut() {
            addresses.sort();
        }
        Self {
            files,
            exact,
            callable,
        }
    }

    fn symbol(&self, address: &SymbolAddress) -> Option<&SymbolDraft> {
        self.files.get(&address.path)?.symbols.get(address.index)
    }

    fn unique_exact(&self, qualified_name: &str, kind: SymbolKind) -> Option<SymbolAddress> {
        let matches = self.exact.get(&(qualified_name.to_owned(), kind))?;
        (matches.len() == 1).then(|| matches[0].clone())
    }

    fn call_candidates(&self, name: &str, qualifier: Option<&str>) -> (Vec<SymbolAddress>, bool) {
        let mut candidates = self.callable.get(name).cloned().unwrap_or_default();
        if let Some(qualifier) = qualifier {
            let qualified: Vec<_> = candidates
                .iter()
                .filter(|address| {
                    self.symbol(address).is_some_and(|symbol| {
                        symbol.key.container.as_deref() == Some(qualifier)
                            || symbol.key.qualified_name.rsplit_once("::").is_some_and(
                                |(container, _)| {
                                    container == qualifier
                                        || container.rsplit("::").next() == Some(qualifier)
                                },
                            )
                    })
                })
                .cloned()
                .collect();
            if !qualified.is_empty() {
                candidates = qualified;
            }
        }
        candidates.sort();
        let truncated = candidates.len() > MAX_CANDIDATES_PER_CALL_SITE;
        candidates.truncate(MAX_CANDIDATES_PER_CALL_SITE);
        (candidates, truncated)
    }
}

#[derive(Debug, Clone, Default)]
pub struct PhpSyntaxIndex {
    files: BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
    relationships: BTreeMap<RepoRelativePath, Arc<RelationshipContribution>>,
}

impl PhpSyntaxIndex {
    pub fn from_sources(
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
    ) -> Result<(Self, SymbolGraph), PhpIndexError> {
        let mut parser = PhpParser::new().map_err(parse_error)?;
        let mut files = BTreeMap::new();
        for (path, source) in sources {
            let parsed = parser.parse(path.clone(), source).map_err(parse_error)?;
            files.insert(path, Arc::new(parsed));
        }
        let catalog = SymbolCatalog::new(&files);
        let relationships = files
            .iter()
            .map(|(path, file)| {
                (
                    path.clone(),
                    Arc::new(relationships_for_file(path, file, &catalog)),
                )
            })
            .collect();
        let index = Self {
            files,
            relationships,
        };
        let graph = index.materialize_graph()?;
        Ok((index, graph))
    }

    pub fn paths(&self) -> Vec<RepoRelativePath> {
        self.files.keys().cloned().collect()
    }

    pub fn reconcile_sources(
        &self,
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
    ) -> Result<ReconcileReport, PhpIndexError> {
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
        if changed_paths.is_empty() {
            metrics.syntax_error_files = self.syntax_error_files();
            metrics.truncated_call_sites = self.truncated_call_sites();
            return Ok(ReconcileReport {
                graph: None,
                metrics,
                next_index: None,
            });
        }

        let mut next_files = self.files.clone();
        let mut changed_dependencies = HashSet::new();
        for path in &changed_paths {
            if let Some(previous) = self.files.get(path) {
                changed_dependencies.extend(exported_dependencies(previous));
            }
        }
        let mut parser = PhpParser::new().map_err(parse_error)?;
        for path in &changed_paths {
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
        let catalog = SymbolCatalog::new(&next_files);
        let mut next_relationships = self.relationships.clone();
        for path in &affected_owners {
            match next_files.get(path) {
                Some(file) => {
                    next_relationships.insert(
                        path.clone(),
                        Arc::new(relationships_for_file(path, file, &catalog)),
                    );
                    metrics.relationship_files_recomputed += 1;
                }
                None => {
                    next_relationships.remove(path);
                }
            }
        }
        let next = Self {
            files: next_files,
            relationships: next_relationships,
        };
        let graph = next.materialize_graph()?;
        metrics.syntax_error_files = next.syntax_error_files();
        metrics.truncated_call_sites = next.truncated_call_sites();
        Ok(ReconcileReport {
            graph: Some(graph),
            metrics,
            next_index: Some(next),
        })
    }

    fn syntax_error_files(&self) -> u64 {
        self.files.values().filter(|file| file.has_errors).count() as u64
    }

    fn truncated_call_sites(&self) -> u64 {
        self.relationships
            .values()
            .map(|contribution| contribution.truncated_call_sites)
            .sum()
    }

    fn materialize_graph(&self) -> Result<SymbolGraph, PhpIndexError> {
        let mut graph = SymbolGraph::new();
        let mut ids = BTreeMap::new();
        for (path, file) in &self.files {
            graph.add_file(path.clone(), file.source.clone())?;
            for (index, symbol) in file.symbols.iter().enumerate() {
                let id = graph.add_symbol(
                    symbol.key.clone(),
                    symbol.location.clone(),
                    symbol.signature.clone(),
                    Provenance::TreeSitter,
                    Precision::Syntax,
                )?;
                ids.insert(
                    SymbolAddress {
                        path: path.clone(),
                        index,
                    },
                    id,
                );
            }
        }
        for contribution in self.relationships.values() {
            for edge in &contribution.edges {
                let from = address_id(&ids, &edge.from)?;
                let to = address_id(&ids, &edge.to)?;
                graph.add_edge(
                    edge.kind,
                    from,
                    to,
                    edge.provenance,
                    edge.precision,
                    edge.location.clone(),
                )?;
            }
        }
        graph.set_truncated_call_sites(self.truncated_call_sites());
        graph.validate_consistency()?;
        Ok(graph)
    }
}

fn address_id(
    ids: &BTreeMap<SymbolAddress, EntityId>,
    address: &SymbolAddress,
) -> Result<EntityId, PhpIndexError> {
    ids.get(address).copied().ok_or_else(|| {
        PhpIndexError::Update(format!(
            "relationship references missing symbol {}#{}",
            address.path, address.index
        ))
    })
}

fn parse_error(error: impl std::fmt::Display) -> PhpIndexError {
    PhpIndexError::Parse(error.to_string())
}

fn symbol_name(symbol: &SymbolDraft) -> &str {
    symbol
        .key
        .qualified_name
        .rsplit("::")
        .next()
        .unwrap_or(&symbol.key.qualified_name)
}

fn is_callable(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
    )
}

fn exported_dependencies(file: &ParsedFile) -> HashSet<DependencyKey> {
    let mut keys = HashSet::new();
    for symbol in &file.symbols {
        keys.insert(DependencyKey::Exact(
            symbol.key.qualified_name.clone(),
            symbol.key.kind,
        ));
        if is_callable(symbol.key.kind) {
            keys.insert(DependencyKey::CallableName(symbol_name(symbol).to_owned()));
        }
    }
    keys
}

fn relationships_for_file(
    path: &RepoRelativePath,
    file: &ParsedFile,
    catalog: &SymbolCatalog<'_>,
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
        contribution.edges.push(RelationshipEdge {
            kind: EdgeKind::Contains,
            from: parent,
            to: address(index),
            provenance: Provenance::TreeSitter,
            precision: Precision::Syntax,
            location: None,
        });
    }
    for relation in &file.named_relations {
        for target_kind in &relation.target_kinds {
            contribution
                .dependencies
                .insert(DependencyKey::Exact(relation.target.clone(), *target_kind));
            let Some(target) = catalog.unique_exact(&relation.target, *target_kind) else {
                continue;
            };
            contribution.edges.push(RelationshipEdge {
                kind: relation.kind,
                from: address(relation.from),
                to: target,
                provenance: Provenance::TreeSitter,
                precision: Precision::Heuristic,
                location: None,
            });
            break;
        }
    }
    for call in &file.calls {
        contribution
            .dependencies
            .insert(DependencyKey::CallableName(call.name.clone()));
        let (candidates, truncated) =
            catalog.call_candidates(&call.name, call.qualifier.as_deref());
        contribution.truncated_call_sites += u64::from(truncated);
        let caller = address(call.caller);
        let is_test = file
            .symbols
            .get(call.caller)
            .is_some_and(|symbol| symbol.key.kind == SymbolKind::Test);
        for target in candidates {
            contribution.edges.push(RelationshipEdge {
                kind: EdgeKind::Calls,
                from: caller.clone(),
                to: target.clone(),
                provenance: Provenance::TreeSitter,
                precision: Precision::Heuristic,
                location: Some(call.location.clone()),
            });
            if is_test {
                contribution.edges.push(RelationshipEdge {
                    kind: EdgeKind::Tests,
                    from: caller.clone(),
                    to: target,
                    provenance: Provenance::Heuristic,
                    precision: Precision::Heuristic,
                    location: Some(call.location.clone()),
                });
            }
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
        elapsed: started.elapsed(),
    };
    info!(
        files = metrics.parsed_files,
        syntax_error_files = metrics.syntax_error_files,
        truncated_call_sites = metrics.truncated_call_sites,
        symbols = metrics.symbols,
        edges = metrics.edges,
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
            2
        );
        assert!(
            calls
                .iter()
                .all(|edge| edge.precision == Precision::Heuristic)
        );
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
