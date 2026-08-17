//! Deterministic Rust syntax indexing with reusable per-file facts.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{EdgeKind, EntityId, SymbolKind};
use chakra_engine::{CallSiteInput, ConsistencyError, GraphError, SymbolGraph};
use thiserror::Error;
use tracing::{info, info_span};

use crate::discovery::{DiscoveryError, discover_rust_files, resolve_repository_root};
use crate::parser::{ParsedFile, RustParser, SymbolDraft};

const MAX_SOURCE_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REPOSITORY_SOURCE_BYTES: usize = 256 * 1024 * 1024;

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
    pub syntax_error_files: u64,
    pub truncated_call_sites: u64,
}

/// Result of reconciling exact worktree contents with the reusable index.
#[derive(Debug)]
pub struct ReconcileReport {
    /// Present only when source content changed and a new graph is ready.
    pub graph: Option<SymbolGraph>,
    pub metrics: ReconcileMetrics,
    pub next_index: Option<RustSyntaxIndex>,
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
    #[error("Rust syntax index update failed: {0}")]
    Update(String),
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error("constructed syntax graph is inconsistent: {0}")]
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
#[derive(Debug, Clone, Default)]
pub struct RustSyntaxIndex {
    files: BTreeMap<RepoRelativePath, Arc<ParsedFile>>,
    relationships: BTreeMap<RepoRelativePath, Arc<RelationshipContribution>>,
}

impl RustSyntaxIndex {
    pub fn from_sources(
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
    ) -> Result<(Self, SymbolGraph), RustIndexError> {
        let mut parser = RustParser::new().map_err(parse_error)?;
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

    /// Reconciles exact discovered source contents. Unchanged text is not
    /// reparsed. Changed files and relationship owners are prepared in a
    /// private copy and become the reusable state only after graph validation.
    pub fn reconcile_sources(
        &self,
        sources: BTreeMap<RepoRelativePath, Arc<str>>,
    ) -> Result<ReconcileReport, RustIndexError> {
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
        let mut parser = RustParser::new().map_err(parse_error)?;
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
        0
    }

    fn materialize_graph(&self) -> Result<SymbolGraph, RustIndexError> {
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
        for (path, file) in &self.files {
            for call_site in &file.calls {
                graph.add_call_site(CallSiteInput {
                    caller: address_id(
                        &ids,
                        &SymbolAddress {
                            path: path.clone(),
                            index: call_site.caller,
                        },
                    )?,
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
        graph.set_truncated_call_sites(self.truncated_call_sites());
        Ok(graph)
    }
}

fn address_id(
    ids: &BTreeMap<SymbolAddress, EntityId>,
    address: &SymbolAddress,
) -> Result<EntityId, RustIndexError> {
    ids.get(address).copied().ok_or_else(|| {
        RustIndexError::Update(format!(
            "relationship references missing symbol {}#{}",
            address.path, address.index
        ))
    })
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
            contribution.edges.push(RelationshipEdge {
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
            });
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
            contribution.edges.push(RelationshipEdge {
                kind: EdgeKind::Contains,
                from: target,
                to: address(implementation.symbol),
                provenance: Provenance::TreeSitter,
                precision: Precision::Heuristic,
                location: None,
            });
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
            contribution.edges.push(RelationshipEdge {
                kind: EdgeKind::Implements,
                from: target,
                to: trait_address.clone(),
                provenance: Provenance::TreeSitter,
                precision: Precision::Heuristic,
                location: None,
            });
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
                contribution.edges.push(RelationshipEdge {
                    kind: EdgeKind::Implements,
                    from: address(method_index),
                    to: trait_method,
                    provenance: Provenance::TreeSitter,
                    precision: Precision::Heuristic,
                    location: None,
                });
            }
        }
    }

    contribution
}

fn read_sources(
    repository_root: &Path,
) -> Result<BTreeMap<RepoRelativePath, Arc<str>>, RustIndexError> {
    let files = discover_rust_files(repository_root)?;
    let mut sources = BTreeMap::new();
    let mut total_bytes = 0_usize;
    for path in files {
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
        sources.insert(path, Arc::<str>::from(source));
    }
    Ok(sources)
}

/// Builds a complete Rust syntax index from the actual materialized Git
/// worktree. The caller owns atomic publication into `WorkspaceEngine`.
pub fn index_repository(root: &Path) -> Result<IndexReport, RustIndexError> {
    let started = Instant::now();
    let repository_root = resolve_repository_root(root)?;
    let span = info_span!("rust_repository_index", root = %repository_root.display());
    let _entered = span.enter();
    let sources = read_sources(&repository_root)?;
    let discovered_files = sources.len() as u64;
    let (syntax_index, graph) = RustSyntaxIndex::from_sources(sources)?;
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
pub fn scan_repository_sources(
    repository_root: &Path,
) -> Result<BTreeMap<RepoRelativePath, Arc<str>>, RustIndexError> {
    read_sources(repository_root)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs::{self, File};
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

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
