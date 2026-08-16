//! In-memory symbol graph (ADR-0002).
//!
//! Representation: an arena of symbols (`Vec<Symbol>`, indexed by
//! [`EntityId`]) plus derived indexes for name lookup, file membership, and
//! adjacency. Chosen over a generic graph library because v0.1 traversals
//! are one hop deep and the whole structure is cloned privately per update;
//! see `docs/adr/0002-in-memory-graph-representation.md`.

use std::collections::HashMap;
use std::sync::Arc;

use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{Edge, EdgeKind, EntityId, Symbol, SymbolKey};
use thiserror::Error;

/// Why a graph mutation was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GraphError {
    #[error("symbol key path `{key_path}` does not match location file `{location_path}`")]
    KeyLocationMismatch {
        key_path: String,
        location_path: String,
    },
    #[error("unknown symbol id: {0:?}")]
    UnknownEntity(EntityId),
    #[error("source file is already indexed: {0}")]
    DuplicateFile(RepoRelativePath),
    #[error("merged graph is inconsistent: {0}")]
    Consistency(#[from] ConsistencyError),
}

#[derive(Debug, Clone)]
struct IndexedFile {
    symbols: Vec<EntityId>,
    /// Immutable source captured in the same published revision as the
    /// syntax facts. `Arc<str>` keeps private snapshot clones cheap.
    source: Option<Arc<str>>,
    provenance: Provenance,
    precision: Precision,
}

/// Symbols and typed relations of one workspace revision.
///
/// Mutated only through `add_*` while a snapshot is being built privately;
/// once published it is immutable behind an `Arc`.
#[derive(Debug, Clone, Default)]
pub struct SymbolGraph {
    symbols: Vec<Symbol>,
    /// File → captured source plus entities declared in it.
    files: HashMap<RepoRelativePath, IndexedFile>,
    outgoing: HashMap<EntityId, Vec<Edge>>,
    incoming: HashMap<EntityId, Vec<Edge>>,
    edge_count: u64,
    /// Number of syntax call sites whose candidate targets were cut while
    /// building this exact graph revision. Query envelopes use this to avoid
    /// presenting an incomplete call slice as complete.
    truncated_call_sites: u64,
}

impl SymbolGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Combines independently built language graphs into one revision-local
    /// workspace graph, remapping arena ids while preserving every fact's
    /// language, provenance, precision, and source range.
    pub fn merge(graphs: impl IntoIterator<Item = SymbolGraph>) -> Result<Self, GraphError> {
        let mut merged = Self::new();
        for graph in graphs {
            merged.append(graph)?;
        }
        merged
            .validate_consistency()
            .map_err(GraphError::Consistency)?;
        Ok(merged)
    }

    fn append(&mut self, graph: SymbolGraph) -> Result<(), GraphError> {
        let mut ids = HashMap::with_capacity(graph.symbols.len());
        for (path, file) in &graph.files {
            if let Some(source) = &file.source {
                self.add_file(path.clone(), source.clone())?;
            }
        }
        for symbol in &graph.symbols {
            let id = self.add_symbol(
                symbol.key.clone(),
                symbol.location.clone(),
                symbol.signature.clone(),
                symbol.provenance,
                symbol.precision,
            )?;
            ids.insert(symbol.id, id);
        }
        for edges in graph.outgoing.values() {
            for edge in edges {
                let from = ids
                    .get(&edge.from)
                    .copied()
                    .ok_or(GraphError::UnknownEntity(edge.from))?;
                let to = ids
                    .get(&edge.to)
                    .copied()
                    .ok_or(GraphError::UnknownEntity(edge.to))?;
                self.add_edge(
                    edge.kind,
                    from,
                    to,
                    edge.provenance,
                    edge.precision,
                    edge.location.clone(),
                )?;
            }
        }
        self.truncated_call_sites = self
            .truncated_call_sites
            .saturating_add(graph.truncated_call_sites);
        Ok(())
    }

    /// Adds one discovered source file and the exact text parsed for this
    /// graph revision. Files with no extractable declarations still appear
    /// in `repo_map` and text search.
    pub fn add_file(
        &mut self,
        path: RepoRelativePath,
        source: impl Into<Arc<str>>,
    ) -> Result<(), GraphError> {
        match self.files.entry(path.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(IndexedFile {
                    symbols: Vec::new(),
                    source: Some(source.into()),
                    provenance: Provenance::Git,
                    precision: Precision::Precise,
                });
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().source.is_some() {
                    return Err(GraphError::DuplicateFile(path));
                }
                let file = entry.get_mut();
                file.source = Some(source.into());
                file.provenance = Provenance::Git;
                file.precision = Precision::Precise;
            }
        }
        Ok(())
    }

    /// Adds a symbol; the key path must match the location file.
    pub fn add_symbol(
        &mut self,
        key: SymbolKey,
        location: SourceRange,
        signature: Option<String>,
        provenance: Provenance,
        precision: Precision,
    ) -> Result<EntityId, GraphError> {
        if &key.path != location.file() {
            return Err(GraphError::KeyLocationMismatch {
                key_path: key.path.as_str().to_owned(),
                location_path: location.file().as_str().to_owned(),
            });
        }
        let id = EntityId(self.symbols.len() as u64);
        let symbol = Symbol {
            id,
            key,
            location,
            signature,
            provenance,
            precision,
        };
        self.files
            .entry(symbol.location.file().clone())
            .or_insert_with(|| IndexedFile {
                symbols: Vec::new(),
                source: None,
                provenance: symbol.provenance,
                precision: symbol.precision,
            })
            .symbols
            .push(id);
        self.symbols.push(symbol);
        Ok(id)
    }

    /// Adds a typed edge; both endpoints must already exist.
    pub fn add_edge(
        &mut self,
        kind: EdgeKind,
        from: EntityId,
        to: EntityId,
        provenance: Provenance,
        precision: Precision,
        location: Option<SourceRange>,
    ) -> Result<(), GraphError> {
        for id in [from, to] {
            if self.symbol(id).is_none() {
                return Err(GraphError::UnknownEntity(id));
            }
        }
        let edge = Edge {
            kind,
            from,
            to,
            provenance,
            precision,
            location,
        };
        self.outgoing.entry(from).or_default().push(edge.clone());
        self.incoming.entry(to).or_default().push(edge);
        self.edge_count += 1;
        Ok(())
    }

    pub fn symbol(&self, id: EntityId) -> Option<&Symbol> {
        self.symbols.get(id.0 as usize)
    }

    pub fn symbols(&self) -> &[Symbol] {
        &self.symbols
    }

    pub fn symbol_count(&self) -> u64 {
        self.symbols.len() as u64
    }

    pub fn edge_count(&self) -> u64 {
        self.edge_count
    }

    /// Records call-candidate incompleteness discovered by a language index.
    pub fn set_truncated_call_sites(&mut self, truncated_call_sites: u64) {
        self.truncated_call_sites = truncated_call_sites;
    }

    /// Call sites whose syntax candidate set was cut in this graph revision.
    pub fn truncated_call_sites(&self) -> u64 {
        self.truncated_call_sites
    }

    pub fn file_count(&self) -> u64 {
        self.files.len() as u64
    }

    /// Files with the number of symbols declared in each, sorted by path.
    pub fn file_summaries(&self) -> Vec<(RepoRelativePath, u64, Provenance, Precision)> {
        let mut summaries: Vec<_> = self
            .files
            .iter()
            .map(|(path, file)| {
                (
                    path.clone(),
                    file.symbols.len() as u64,
                    file.provenance,
                    file.precision,
                )
            })
            .collect();
        summaries.sort_by(|a, b| a.0.cmp(&b.0));
        summaries
    }

    /// Captured source for one file in this graph revision.
    pub fn file_source(&self, path: &RepoRelativePath) -> Option<&str> {
        self.files.get(path)?.source.as_deref()
    }

    /// Captured source files sorted by repository-relative path.
    pub fn source_files(&self) -> Vec<(&RepoRelativePath, &str)> {
        let mut files: Vec<_> = self
            .files
            .iter()
            .filter_map(|(path, file)| file.source.as_deref().map(|source| (path, source)))
            .collect();
        files.sort_by(|a, b| a.0.cmp(b.0));
        files
    }

    /// Cheap owned views of captured source for outward adapters. Cloning the
    /// `Arc<str>` never copies file contents.
    pub(crate) fn snapshot_documents(&self) -> Vec<(RepoRelativePath, Arc<str>)> {
        let mut files: Vec<_> = self
            .files
            .iter()
            .filter_map(|(path, file)| {
                file.source
                    .as_ref()
                    .map(|source| (path.clone(), source.clone()))
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    /// Symbols declared in one file, in deterministic arena order.
    pub fn symbols_in_file<'a>(
        &'a self,
        path: &RepoRelativePath,
    ) -> impl Iterator<Item = &'a Symbol> {
        self.files
            .get(path)
            .into_iter()
            .flat_map(|file| file.symbols.iter())
            .filter_map(|id| self.symbol(*id))
    }

    /// Case-insensitive substring search over qualified names. Result
    /// construction stops at the caller's budget plus the first omitted
    /// match, so a broad query cannot allocate one view per graph symbol.
    pub fn search_names(&self, needle: &str, limit: usize) -> (Vec<EntityId>, bool) {
        let needle = needle.to_lowercase();
        let mut matches = Vec::with_capacity(limit.min(self.symbols.len()));
        for symbol in &self.symbols {
            // The simple name is a suffix of the qualified name, so one
            // comparison covers both without a second lowercase allocation.
            if symbol.key.qualified_name.to_lowercase().contains(&needle) {
                if matches.len() == limit {
                    return (matches, true);
                }
                matches.push(symbol.id);
            }
        }
        (matches, false)
    }

    /// Exact resolution by simple or qualified name (SPEC §24).
    pub fn resolve_name(&self, name: &str) -> Vec<EntityId> {
        self.symbols
            .iter()
            .filter(|symbol| symbol.name() == name || symbol.key.qualified_name == name)
            .map(|symbol| symbol.id)
            .collect()
    }

    pub fn outgoing_edges(&self, id: EntityId) -> &[Edge] {
        self.outgoing.get(&id).map_or(&[], Vec::as_slice)
    }

    pub fn incoming_edges(&self, id: EntityId) -> &[Edge] {
        self.incoming.get(&id).map_or(&[], Vec::as_slice)
    }

    /// Independent consistency audit used by the atomic-revision regression
    /// tests: every derived structure is recomputed from the arena and
    /// compared, so a hybrid snapshot would be caught here.
    pub fn validate_consistency(&self) -> Result<(), ConsistencyError> {
        // Arena ids match arena positions.
        for (index, symbol) in self.symbols.iter().enumerate() {
            if symbol.id.0 as usize != index {
                return Err(ConsistencyError::IdPositionMismatch {
                    id: symbol.id,
                    index,
                });
            }
        }

        // The file index covers exactly the arena symbols.
        let mut expected_by_file: HashMap<&RepoRelativePath, Vec<EntityId>> =
            self.files.keys().map(|path| (path, Vec::new())).collect();
        for symbol in &self.symbols {
            expected_by_file
                .entry(symbol.location.file())
                .or_default()
                .push(symbol.id);
        }
        for ids in expected_by_file.values_mut() {
            ids.sort_unstable();
        }
        let mut actual_by_file: HashMap<&RepoRelativePath, Vec<EntityId>> = self
            .files
            .iter()
            .map(|(path, file)| (path, file.symbols.clone()))
            .collect();
        for ids in actual_by_file.values_mut() {
            ids.sort_unstable();
        }
        if actual_by_file != expected_by_file {
            return Err(ConsistencyError::FileIndexMismatch);
        }

        // Edges: stored under the correct key, endpoints exist, and both
        // adjacency indexes mirror each other exactly. Two independent
        // passes: outgoing → incoming, then incoming → outgoing, so a ghost
        // entry in either direction is caught (it would otherwise be served
        // by `callers`/`context` while passing the audit).
        let mut outgoing_total = 0_u64;
        for (key, edges) in &self.outgoing {
            for edge in edges {
                outgoing_total += 1;
                if edge.from != *key {
                    return Err(ConsistencyError::EdgeWrongOutgoingKey {
                        key: *key,
                        from: edge.from,
                    });
                }
                self.symbol(edge.from)
                    .ok_or(ConsistencyError::UnknownEntity(edge.from))?;
                self.symbol(edge.to)
                    .ok_or(ConsistencyError::UnknownEntity(edge.to))?;
                let mirrored = self
                    .incoming
                    .get(&edge.to)
                    .is_some_and(|incoming| incoming.contains(edge));
                if !mirrored {
                    return Err(ConsistencyError::EdgeMirrorMissing {
                        from: edge.from,
                        to: edge.to,
                    });
                }
            }
        }
        let mut incoming_total = 0_u64;
        for (key, edges) in &self.incoming {
            for edge in edges {
                incoming_total += 1;
                if edge.to != *key {
                    return Err(ConsistencyError::EdgeWrongIncomingKey {
                        key: *key,
                        to: edge.to,
                    });
                }
                self.symbol(edge.from)
                    .ok_or(ConsistencyError::UnknownEntity(edge.from))?;
                self.symbol(edge.to)
                    .ok_or(ConsistencyError::UnknownEntity(edge.to))?;
                let mirrored = self
                    .outgoing
                    .get(&edge.from)
                    .is_some_and(|outgoing| outgoing.contains(edge));
                if !mirrored {
                    return Err(ConsistencyError::EdgeIncomingMirrorMissing {
                        from: edge.from,
                        to: edge.to,
                    });
                }
            }
        }
        if outgoing_total != self.edge_count || incoming_total != self.edge_count {
            return Err(ConsistencyError::EdgeCountMismatch {
                recorded: self.edge_count,
                outgoing: outgoing_total,
                incoming: incoming_total,
            });
        }
        Ok(())
    }
}

/// A broken internal graph invariant found by [`SymbolGraph::validate_consistency`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConsistencyError {
    #[error("edge endpoint {0:?} does not exist in the arena")]
    UnknownEntity(EntityId),
    #[error("symbol id {id:?} sits at arena index {index}")]
    IdPositionMismatch { id: EntityId, index: usize },
    #[error("edge stored under outgoing key {key:?} but its from is {from:?}")]
    EdgeWrongOutgoingKey { key: EntityId, from: EntityId },
    #[error("edge stored under incoming key {key:?} but its to is {to:?}")]
    EdgeWrongIncomingKey { key: EntityId, to: EntityId },
    #[error("edge from {from:?} to {to:?} is missing from the incoming index")]
    EdgeMirrorMissing { from: EntityId, to: EntityId },
    #[error("edge from {from:?} to {to:?} is in the incoming index but has no outgoing twin")]
    EdgeIncomingMirrorMissing { from: EntityId, to: EntityId },
    #[error(
        "recorded edge count {recorded} does not match the indexes ({outgoing} outgoing, {incoming} incoming)"
    )]
    EdgeCountMismatch {
        recorded: u64,
        outgoing: u64,
        incoming: u64,
    },
    #[error("file index does not cover exactly the arena symbols")]
    FileIndexMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chakra_domain::location::TextPosition;
    use chakra_domain::symbol::{Language, SymbolKind};

    fn file(path: &str) -> Result<RepoRelativePath, Box<dyn std::error::Error>> {
        Ok(RepoRelativePath::new(path)?)
    }

    fn range(path: RepoRelativePath) -> Result<SourceRange, Box<dyn std::error::Error>> {
        let position = TextPosition::new(1, 1)?;
        Ok(SourceRange::new(path, position, position)?)
    }

    fn key(name: &str, path: RepoRelativePath) -> SymbolKey {
        SymbolKey {
            language: Language::Rust,
            qualified_name: name.to_owned(),
            container: None,
            kind: SymbolKind::Function,
            path,
        }
    }

    fn add_fn(
        graph: &mut SymbolGraph,
        name: &str,
        path: &str,
    ) -> Result<EntityId, Box<dyn std::error::Error>> {
        let file = file(path)?;
        let location = range(file.clone())?;
        Ok(graph.add_symbol(
            key(name, file),
            location,
            None,
            Provenance::TreeSitter,
            Precision::Syntax,
        )?)
    }

    #[test]
    fn rejects_key_location_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let key_path = file("src/a.rs")?;
        let other = file("src/b.rs")?;
        let location = range(other)?;
        let result = graph.add_symbol(
            key("f", key_path),
            location,
            None,
            Provenance::TreeSitter,
            Precision::Syntax,
        );
        assert!(matches!(
            result,
            Err(GraphError::KeyLocationMismatch { .. })
        ));
        Ok(())
    }

    #[test]
    fn rejects_edges_to_unknown_entities() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let a = add_fn(&mut graph, "a", "src/a.rs")?;
        let ghost = EntityId(999);
        assert!(matches!(
            graph.add_edge(
                EdgeKind::Calls,
                a,
                ghost,
                Provenance::TreeSitter,
                Precision::Syntax,
                None
            ),
            Err(GraphError::UnknownEntity(id)) if id == ghost
        ));
        Ok(())
    }

    #[test]
    fn name_search_is_case_insensitive_substring() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        add_fn(
            &mut graph,
            "service::PaymentService::refund",
            "src/service.rs",
        )?;
        add_fn(&mut graph, "refund_helper", "src/helper.rs")?;
        add_fn(&mut graph, "unrelated", "src/lib.rs")?;
        assert_eq!(graph.search_names("refund", 10).0.len(), 2);
        assert_eq!(graph.search_names("REFUND", 10).0.len(), 2);
        assert_eq!(graph.search_names("paymentservice::ref", 10).0.len(), 1);
        assert_eq!(graph.search_names("missing", 10), (vec![], false));
        let (limited, truncated) = graph.search_names("refund", 1);
        assert_eq!(limited.len(), 1);
        assert!(truncated);
        Ok(())
    }

    #[test]
    fn resolve_name_matches_simple_and_qualified() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        add_fn(&mut graph, "a::refund", "src/a.rs")?;
        add_fn(&mut graph, "b::refund", "src/b.rs")?;
        add_fn(&mut graph, "unique", "src/lib.rs")?;
        assert_eq!(graph.resolve_name("refund").len(), 2);
        assert_eq!(graph.resolve_name("a::refund").len(), 1);
        assert_eq!(graph.resolve_name("unique").len(), 1);
        assert_eq!(graph.resolve_name("nope").len(), 0);
        Ok(())
    }

    #[test]
    fn consistency_validation_passes_on_coherent_graph() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let a = add_fn(&mut graph, "a", "src/a.rs")?;
        let b = add_fn(&mut graph, "b", "src/b.rs")?;
        graph.add_edge(
            EdgeKind::Calls,
            a,
            b,
            Provenance::TreeSitter,
            Precision::Syntax,
            None,
        )?;
        graph.validate_consistency()?;
        assert_eq!(graph.symbol_count(), 2);
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(graph.file_count(), 2);
        Ok(())
    }

    #[test]
    fn source_files_without_symbols_are_indexed_and_duplicates_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let path = file("src/comments.rs")?;
        graph.add_file(path.clone(), "//! Documentation only.\n")?;
        assert_eq!(graph.file_count(), 1);
        assert_eq!(
            graph.file_summaries(),
            vec![(path.clone(), 0, Provenance::Git, Precision::Precise)]
        );
        assert_eq!(graph.file_source(&path), Some("//! Documentation only.\n"));
        assert!(matches!(
            graph.add_file(path.clone(), "changed"),
            Err(GraphError::DuplicateFile(duplicate)) if duplicate == path
        ));
        graph.validate_consistency()?;
        Ok(())
    }

    // Corruption tests reach into private fields on purpose: they simulate a
    // botched incremental update that the public `add_*` API would never
    // produce, and prove the audit catches it before readers are served.

    #[test]
    fn audit_catches_ghost_incoming_edge() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let a = add_fn(&mut graph, "a", "src/a.rs")?;
        let b = add_fn(&mut graph, "b", "src/b.rs")?;
        graph.add_edge(
            EdgeKind::Calls,
            a,
            b,
            Provenance::TreeSitter,
            Precision::Syntax,
            None,
        )?;
        // An incoming edge no outgoing entry mirrors: `incoming_edges(a)`
        // (and therefore `callers`) would serve it, so the audit must not.
        graph.incoming.entry(a).or_default().push(Edge {
            kind: EdgeKind::Calls,
            from: b,
            to: a,
            provenance: Provenance::TreeSitter,
            precision: Precision::Syntax,
            location: None,
        });
        assert!(matches!(
            graph.validate_consistency(),
            Err(ConsistencyError::EdgeIncomingMirrorMissing { from, to }) if from == b && to == a
        ));
        Ok(())
    }

    #[test]
    fn audit_catches_incoming_edge_under_wrong_key() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let a = add_fn(&mut graph, "a", "src/a.rs")?;
        let b = add_fn(&mut graph, "b", "src/b.rs")?;
        graph.add_edge(
            EdgeKind::Calls,
            a,
            b,
            Provenance::TreeSitter,
            Precision::Syntax,
            None,
        )?;
        graph.incoming.entry(a).or_default().push(Edge {
            kind: EdgeKind::Calls,
            from: a,
            to: b,
            provenance: Provenance::TreeSitter,
            precision: Precision::Syntax,
            location: None,
        });
        assert!(matches!(
            graph.validate_consistency(),
            Err(ConsistencyError::EdgeWrongIncomingKey { key, to }) if key == a && to == b
        ));
        Ok(())
    }

    #[test]
    fn audit_catches_recorded_count_drift() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let a = add_fn(&mut graph, "a", "src/a.rs")?;
        let b = add_fn(&mut graph, "b", "src/b.rs")?;
        graph.add_edge(
            EdgeKind::Calls,
            a,
            b,
            Provenance::TreeSitter,
            Precision::Syntax,
            None,
        )?;
        graph.edge_count += 1;
        assert!(matches!(
            graph.validate_consistency(),
            Err(ConsistencyError::EdgeCountMismatch {
                recorded: 2,
                outgoing: 1,
                incoming: 1
            })
        ));
        Ok(())
    }
}
