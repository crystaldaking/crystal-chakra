//! In-memory symbol graph (ADR-0002).
//!
//! Representation: an arena of symbols (`Vec<Symbol>`, indexed by
//! [`EntityId`]) plus derived indexes for name lookup, file membership, and
//! adjacency. Chosen over a generic graph library because v0.1 traversals
//! are one hop deep and the whole structure is cloned privately per update;
//! see `docs/adr/0002-in-memory-graph-representation.md`.

use std::collections::HashMap;

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
}

/// Symbols and typed relations of one workspace revision.
///
/// Mutated only through `add_*` while a snapshot is being built privately;
/// once published it is immutable behind an `Arc`.
#[derive(Debug, Clone, Default)]
pub struct SymbolGraph {
    symbols: Vec<Symbol>,
    /// File → entities declared in it.
    by_file: HashMap<RepoRelativePath, Vec<EntityId>>,
    outgoing: HashMap<EntityId, Vec<Edge>>,
    incoming: HashMap<EntityId, Vec<Edge>>,
    edge_count: u64,
}

impl SymbolGraph {
    pub fn new() -> Self {
        Self::default()
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
        self.by_file
            .entry(symbol.location.file().clone())
            .or_default()
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

    pub fn file_count(&self) -> u64 {
        self.by_file.len() as u64
    }

    /// Files with the number of symbols declared in each, sorted by path.
    pub fn file_summaries(&self) -> Vec<(RepoRelativePath, u64)> {
        let mut summaries: Vec<(RepoRelativePath, u64)> = self
            .by_file
            .iter()
            .map(|(path, ids)| (path.clone(), ids.len() as u64))
            .collect();
        summaries.sort_by(|a, b| a.0.cmp(&b.0));
        summaries
    }

    /// Case-insensitive substring search over simple and qualified names.
    pub fn search_names(&self, needle: &str) -> Vec<EntityId> {
        let needle = needle.to_lowercase();
        self.symbols
            .iter()
            .filter(|symbol| {
                symbol.name().to_lowercase().contains(&needle)
                    || symbol.key.qualified_name.to_lowercase().contains(&needle)
            })
            .map(|symbol| symbol.id)
            .collect()
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
        let mut expected_by_file: HashMap<&RepoRelativePath, Vec<EntityId>> = HashMap::new();
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
            .by_file
            .iter()
            .map(|(path, ids)| (path, ids.clone()))
            .collect();
        for ids in actual_by_file.values_mut() {
            ids.sort_unstable();
        }
        if actual_by_file != expected_by_file {
            return Err(ConsistencyError::FileIndexMismatch);
        }

        // Edges: stored under the correct key, endpoints exist, mirrored in
        // the incoming index, and the recorded count matches reality.
        let mut actual_edge_count = 0_u64;
        for (key, edges) in &self.outgoing {
            for edge in edges {
                actual_edge_count += 1;
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
        if actual_edge_count != self.edge_count {
            return Err(ConsistencyError::EdgeCountMismatch {
                recorded: self.edge_count,
                actual: actual_edge_count,
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
    #[error("edge from {from:?} to {to:?} is missing from the incoming index")]
    EdgeMirrorMissing { from: EntityId, to: EntityId },
    #[error("recorded edge count {recorded} does not match actual {actual}")]
    EdgeCountMismatch { recorded: u64, actual: u64 },
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
        add_fn(&mut graph, "unrelated", "src/lib.rs")?;
        assert_eq!(graph.search_names("refund").len(), 1);
        assert_eq!(graph.search_names("REFUND").len(), 1);
        assert_eq!(graph.search_names("paymentservice::ref").len(), 1);
        assert_eq!(graph.search_names("missing").len(), 0);
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
}
