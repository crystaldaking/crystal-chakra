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
    /// Lowercase simple name → entities, for exact-name resolution.
    by_name: HashMap<String, Vec<EntityId>>,
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
        if key.path != location.file {
            return Err(GraphError::KeyLocationMismatch {
                key_path: key.path.as_str().to_owned(),
                location_path: location.file.as_str().to_owned(),
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
        self.by_name
            .entry(symbol.name().to_lowercase())
            .or_default()
            .push(id);
        self.by_file
            .entry(symbol.location.file.clone())
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

    /// Recomputes counts and checks edge endpoints against the arena.
    ///
    /// Publication makes hybrids impossible by construction; this exists so
    /// the atomic-revision regression test can assert that property on every
    /// observed snapshot.
    pub fn validate_consistency(&self) -> Result<(), GraphError> {
        for edges in self.outgoing.values().chain(self.incoming.values()) {
            for edge in edges {
                for id in [edge.from, edge.to] {
                    if self.symbol(id).is_none() {
                        return Err(GraphError::UnknownEntity(id));
                    }
                }
            }
        }
        if self.symbols.len() as u64 != self.symbol_count() {
            return Err(GraphError::KeyLocationMismatch {
                key_path: "internal".to_owned(),
                location_path: "count mismatch".to_owned(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chakra_domain::location::TextPosition;
    use chakra_domain::symbol::{Language, SymbolKind};

    fn file(path: &str) -> Result<RepoRelativePath, Box<dyn std::error::Error>> {
        Ok(RepoRelativePath::new(path)?)
    }

    fn range(path: RepoRelativePath) -> SourceRange {
        let position = TextPosition { line: 1, column: 1 };
        SourceRange {
            file: path,
            start: position,
            end: position,
        }
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
        Ok(graph.add_symbol(
            key(name, file.clone()),
            range(file),
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
        let result = graph.add_symbol(
            key("f", key_path),
            range(other),
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
