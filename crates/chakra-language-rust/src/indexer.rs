//! Repository-wide deterministic syntax index construction.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chakra_domain::location::RepoRelativePath;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{EdgeKind, EntityId, SymbolKind};
use chakra_engine::{ConsistencyError, GraphError, SymbolGraph};
use thiserror::Error;
use tracing::{info, info_span};

use crate::discovery::{DiscoveryError, discover_rust_files, resolve_repository_root};
use crate::parser::{ParsedFile, RustParser};

const MAX_CANDIDATES_PER_CALL_SITE: usize = 64;

/// Measurements captured during a deterministic initial syntax index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexMetrics {
    pub discovered_files: u64,
    pub parsed_files: u64,
    pub syntax_error_files: u64,
    /// Call sites whose same-name candidate set exceeded the safety bound.
    pub truncated_call_sites: u64,
    pub symbols: u64,
    pub edges: u64,
    pub elapsed: Duration,
}

/// Complete private index result, ready for atomic publication by the
/// workspace engine owner.
#[derive(Debug)]
pub struct IndexReport {
    pub repository_root: PathBuf,
    pub graph: SymbolGraph,
    pub metrics: IndexMetrics,
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
    #[error("failed to parse Rust source: {0}")]
    Parse(String),
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error("constructed syntax graph is inconsistent: {0}")]
    Consistency(#[from] ConsistencyError),
}

fn symbol_lookup(graph: &SymbolGraph) -> HashMap<(String, SymbolKind), Vec<EntityId>> {
    let mut lookup: HashMap<(String, SymbolKind), Vec<EntityId>> = HashMap::new();
    for symbol in graph.symbols() {
        lookup
            .entry((symbol.key.qualified_name.clone(), symbol.key.kind))
            .or_default()
            .push(symbol.id);
    }
    lookup
}

fn unique(matches: Option<&Vec<EntityId>>) -> Option<EntityId> {
    let matches = matches?;
    if matches.len() == 1 {
        matches.first().copied()
    } else {
        None
    }
}

fn qualified(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}::{name}")
    }
}

fn callable_lookup(graph: &SymbolGraph) -> HashMap<String, Vec<EntityId>> {
    let mut lookup: HashMap<String, Vec<EntityId>> = HashMap::new();
    for symbol in graph.symbols().iter().filter(|symbol| {
        matches!(
            symbol.key.kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
        )
    }) {
        lookup
            .entry(symbol.name().to_owned())
            .or_default()
            .push(symbol.id);
    }
    lookup
}

fn candidates_by_name(
    graph: &SymbolGraph,
    lookup: &HashMap<String, Vec<EntityId>>,
    name: &str,
    qualifier: Option<&str>,
) -> (Vec<EntityId>, bool) {
    let mut candidates = lookup.get(name).cloned().unwrap_or_default();
    if let Some(qualifier) = qualifier {
        let qualified: Vec<EntityId> = candidates
            .iter()
            .copied()
            .filter(|id| {
                graph.symbol(*id).is_some_and(|symbol| {
                    symbol.key.container.as_deref() == Some(qualifier)
                        || symbol.key.qualified_name.rsplit_once("::").is_some_and(
                            |(container, _)| {
                                container == qualifier
                                    || container.rsplit("::").next() == Some(qualifier)
                            },
                        )
                })
            })
            .collect();
        if !qualified.is_empty() {
            candidates = qualified;
        }
    }
    candidates.sort_unstable();
    let truncated = candidates.len() > MAX_CANDIDATES_PER_CALL_SITE;
    candidates.truncate(MAX_CANDIDATES_PER_CALL_SITE);
    (candidates, truncated)
}

fn add_containment_edges(
    graph: &mut SymbolGraph,
    parsed: &[ParsedFile],
    ids: &[Vec<EntityId>],
) -> Result<(), GraphError> {
    let lookup = symbol_lookup(graph);
    for (file_index, file) in parsed.iter().enumerate() {
        let physical_module = if file.module_path.is_empty() {
            None
        } else {
            unique(lookup.get(&(file.module_path.join("::"), SymbolKind::Module)))
        };
        for (symbol_index, draft) in file.symbols.iter().enumerate() {
            let (parent, provenance, precision) = if let Some(parent) = draft.parent {
                (
                    ids[file_index].get(parent).copied(),
                    Provenance::TreeSitter,
                    Precision::Syntax,
                )
            } else {
                // Mapping a physical module file to the `mod` declaration in
                // its parent file is deterministic layout inference, not a
                // type-system fact.
                (physical_module, Provenance::Heuristic, Precision::Heuristic)
            };
            let Some(parent) = parent else {
                continue;
            };
            let child = ids[file_index][symbol_index];
            if parent != child {
                graph.add_edge(
                    EdgeKind::Contains,
                    parent,
                    child,
                    provenance,
                    precision,
                    None,
                )?;
            }
        }
    }
    Ok(())
}

fn add_implementation_edges(
    graph: &mut SymbolGraph,
    parsed: &[ParsedFile],
    ids: &[Vec<EntityId>],
) -> Result<(), GraphError> {
    let lookup = symbol_lookup(graph);
    for (file_index, file) in parsed.iter().enumerate() {
        for implementation in &file.implementations {
            let impl_id = ids[file_index][implementation.symbol];
            let module = file.module_path.join("::");
            let target = [SymbolKind::Struct, SymbolKind::Enum]
                .into_iter()
                .find_map(|kind| {
                    unique(lookup.get(&(qualified(&module, &implementation.target_type), kind)))
                });
            if let Some(target) = target {
                graph.add_edge(
                    EdgeKind::Contains,
                    target,
                    impl_id,
                    Provenance::TreeSitter,
                    Precision::Syntax,
                    None,
                )?;
            }

            let Some(trait_name) = implementation.trait_name.as_ref() else {
                continue;
            };
            let local_trait =
                unique(lookup.get(&(qualified(&module, trait_name), SymbolKind::Trait)));
            let unique_global_trait = graph
                .symbols()
                .iter()
                .filter(|symbol| {
                    symbol.key.kind == SymbolKind::Trait && symbol.name() == trait_name
                })
                .map(|symbol| symbol.id)
                .collect::<Vec<_>>();
            let Some(trait_id) = local_trait
                .or_else(|| (unique_global_trait.len() == 1).then(|| unique_global_trait[0]))
            else {
                continue;
            };
            if let Some(target) = target {
                graph.add_edge(
                    EdgeKind::Implements,
                    target,
                    trait_id,
                    Provenance::TreeSitter,
                    Precision::Syntax,
                    None,
                )?;
            }

            for (method_index, method) in file.symbols.iter().enumerate() {
                if method.parent != Some(implementation.symbol)
                    || method.key.kind != SymbolKind::Method
                {
                    continue;
                }
                let method_name = method
                    .key
                    .qualified_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(&method.key.qualified_name);
                let trait_method = graph.symbol(trait_id).and_then(|trait_symbol| {
                    unique(lookup.get(&(
                        qualified(&trait_symbol.key.qualified_name, method_name),
                        SymbolKind::Method,
                    )))
                });
                if let Some(trait_method) = trait_method {
                    graph.add_edge(
                        EdgeKind::Implements,
                        ids[file_index][method_index],
                        trait_method,
                        Provenance::TreeSitter,
                        Precision::Syntax,
                        None,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn add_call_candidate_edges(
    graph: &mut SymbolGraph,
    parsed: &[ParsedFile],
    ids: &[Vec<EntityId>],
) -> Result<u64, GraphError> {
    let mut truncated_call_sites = 0_u64;
    let lookup = callable_lookup(graph);
    for (file_index, file) in parsed.iter().enumerate() {
        for call in &file.calls {
            let caller = ids[file_index][call.caller];
            let is_test = graph
                .symbol(caller)
                .is_some_and(|symbol| symbol.key.kind == SymbolKind::Test);
            let (candidates, truncated) =
                candidates_by_name(graph, &lookup, &call.name, call.qualifier.as_deref());
            truncated_call_sites += u64::from(truncated);
            for target in candidates {
                // Tree-sitter proves the call expression and spelling. The
                // link to a same-named declaration remains heuristic until a
                // precise provider confirms it.
                graph.add_edge(
                    EdgeKind::Calls,
                    caller,
                    target,
                    Provenance::TreeSitter,
                    Precision::Heuristic,
                    Some(call.location.clone()),
                )?;
                if is_test {
                    graph.add_edge(
                        EdgeKind::Tests,
                        caller,
                        target,
                        Provenance::Heuristic,
                        Precision::Heuristic,
                        Some(call.location.clone()),
                    )?;
                }
            }
        }
    }
    Ok(truncated_call_sites)
}

/// Builds a complete Rust syntax index from the actual materialized Git
/// worktree. The caller owns atomic publication into `WorkspaceEngine`.
pub fn index_repository(root: &Path) -> Result<IndexReport, RustIndexError> {
    let started = Instant::now();
    let repository_root = resolve_repository_root(root)?;
    let span = info_span!(
        "rust_repository_index",
        root = %repository_root.display()
    );
    let _entered = span.enter();
    let files = discover_rust_files(&repository_root)?;
    let mut parser = RustParser::new().map_err(|error| RustIndexError::Parse(error.to_string()))?;
    let mut parsed = Vec::with_capacity(files.len());

    for path in files {
        let file_span = info_span!("rust_file_parse", path = %path);
        let _file_entered = file_span.enter();
        let source = fs::read_to_string(repository_root.join(path.as_str())).map_err(|source| {
            RustIndexError::Read {
                path: path.clone(),
                source,
            }
        })?;
        parsed.push(
            parser
                .parse(path, source)
                .map_err(|error| RustIndexError::Parse(error.to_string()))?,
        );
    }

    let mut graph = SymbolGraph::new();
    let mut ids = Vec::with_capacity(parsed.len());
    for file in &parsed {
        graph.add_file(file.path.clone(), file.source.clone())?;
        let mut file_ids = Vec::with_capacity(file.symbols.len());
        for symbol in &file.symbols {
            file_ids.push(graph.add_symbol(
                symbol.key.clone(),
                symbol.location.clone(),
                symbol.signature.clone(),
                Provenance::TreeSitter,
                Precision::Syntax,
            )?);
        }
        ids.push(file_ids);
    }

    add_containment_edges(&mut graph, &parsed, &ids)?;
    add_implementation_edges(&mut graph, &parsed, &ids)?;
    let truncated_call_sites = add_call_candidate_edges(&mut graph, &parsed, &ids)?;
    graph.validate_consistency()?;

    let metrics = IndexMetrics {
        discovered_files: parsed.len() as u64,
        parsed_files: parsed.len() as u64,
        syntax_error_files: parsed.iter().filter(|file| file.has_errors).count() as u64,
        truncated_call_sites,
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
        "Rust syntax index completed"
    );
    Ok(IndexReport {
        repository_root,
        graph,
        metrics,
    })
}
