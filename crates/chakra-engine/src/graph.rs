//! In-memory symbol graph (ADR-0002).
//!
//! Representation: persistent ordered symbol/call arenas plus persistent file
//! and adjacency indexes. Immutable revisions structurally share unchanged
//! payloads; a workspace graph is a shallow view over disjoint language
//! partitions. See `docs/adr/0002-in-memory-graph-representation.md`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::diagnostic::SyntaxDiagnostic;
use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::source::{SourceClassification, SourceMetadata, SourceMetadataCoverage};
use chakra_domain::symbol::{
    CallForm, CallResolution, CallSite, CallTargetKind, Edge, EdgeKind, EntityId, Language,
    MAX_RECEIVER_HINT_CHARS, ReceiverTypeSource, Symbol, SymbolKey, SymbolKind,
};
use rpds::{HashTrieMapSync, RedBlackTreeMapSync};
use thiserror::Error;

/// Revision-local entity ids are partitioned per language through an
/// explicit slot registry (ADR-0033): a 4-bit slot tag in bits 60..64
/// (`slot << 60`) plus a 60-bit per-language counter. The v0.1 graph is
/// in-memory only — no id is ever persisted — so the slot layout may change
/// between releases as long as it is consistent within one process.
/// Explicit slot assignment: Rust = 0, Php = 1, TypeScript = 2, Python = 3,
/// JavaScript = 4, Java = 5, C# = 6, Shell = 7, C++ = 8; 7 slots remain. Adding a language means
/// assigning it the next free slot
/// in `language_entity_slot` and appending it to `ENTITY_SLOT_LANGUAGES` —
/// nothing else in the id machinery changes.
const ENTITY_ID_SLOT_COUNT: usize = 16;
const ENTITY_ID_SLOT_SHIFT: u64 = 60;
const ENTITY_ID_COUNTER_LIMIT: u64 = 1 << ENTITY_ID_SLOT_SHIFT;

/// Registered languages in entity-slot order, iterated wherever per-language
/// graph state must be visited deterministically.
const ENTITY_SLOT_LANGUAGES: &[Language] = &[
    Language::Rust,
    Language::Php,
    Language::TypeScript,
    Language::Python,
    Language::JavaScript,
    Language::Java,
    Language::CSharp,
    Language::Shell,
    Language::Cpp,
];

/// The entity-id slot a language owns; see the slot registry above.
fn language_entity_slot(language: Language) -> usize {
    match language {
        Language::Rust => 0,
        Language::Php => 1,
        Language::TypeScript => 2,
        Language::Python => 3,
        Language::JavaScript => 4,
        Language::Java => 5,
        Language::CSharp => 6,
        Language::Shell => 7,
        Language::Cpp => 8,
    }
}

const CANCELLATION_POLL_ITEMS: usize = 256;

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
    #[error("source file is not indexed: {0}")]
    UnknownFile(RepoRelativePath),
    #[error("call-site name must be non-empty")]
    EmptyCallSiteName,
    #[error("call-site receiver hint exceeds the {limit}-character budget")]
    ReceiverHintTooLong { limit: usize },
    #[error("call-site receiver type and its evidence source must be present together")]
    ReceiverTypeEvidenceMismatch,
    #[error("call site for {caller:?} is in `{site_path}`, not caller file `{caller_path}`")]
    CallSiteLocationMismatch {
        caller: EntityId,
        site_path: RepoRelativePath,
        caller_path: RepoRelativePath,
    },
    #[error("cannot merge more than one independently resolved {language:?} graph")]
    OverlappingLanguageGraph { language: Language },
    #[error("cannot remove symbol {id:?} while revision-local relationships still reference it")]
    EntityStillReferenced { id: EntityId },
    #[error("owned graph edge is missing from a directional adjacency index")]
    MissingOwnedEdge,
    #[error("a composed workspace graph is immutable; update a language partition instead")]
    CompositeMutation,
    #[error("revision-local entity id range for {0:?} is exhausted")]
    EntityIdSpaceExhausted(Language),
    #[error("cannot preserve {id:?} while changing its symbol key")]
    PreservedEntityKeyChanged { id: EntityId },
    #[error("diagnostic range file `{diagnostic_path}` does not match indexed file `{file_path}`")]
    DiagnosticPathMismatch {
        file_path: RepoRelativePath,
        diagnostic_path: RepoRelativePath,
    },
    #[error("diagnostic total {total} is smaller than the {retained} retained diagnostics")]
    DiagnosticCountUnderflow { total: u64, retained: usize },
    #[error(
        "syntax diagnostic must use tree_sitter/syntax quality, got {provenance:?}/{precision:?}"
    )]
    InvalidDiagnosticQuality {
        provenance: Provenance,
        precision: Precision,
    },
    #[error("graph consistency audit failed: {0}")]
    Consistency(#[from] ConsistencyError),
}

/// Input for adding one syntax call expression to a private graph revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSiteInput {
    pub caller: EntityId,
    pub form: CallForm,
    pub target_kind: CallTargetKind,
    pub name: String,
    pub qualifier: Option<String>,
    pub receiver_type: Option<String>,
    pub receiver_type_source: Option<ReceiverTypeSource>,
    pub receiver_hint: Option<String>,
    pub location: SourceRange,
    pub provenance: Provenance,
    pub precision: Precision,
}

/// Allocation limits applied while a private language graph is materialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphBuildLimits {
    pub max_symbols: u64,
    pub max_edges: u64,
    pub max_call_sites: u64,
}

impl GraphBuildLimits {
    pub const UNLIMITED: Self = Self {
        max_symbols: u64::MAX,
        max_edges: u64::MAX,
        max_call_sites: u64::MAX,
    };
}

/// Exact retained/omitted work from bounded graph construction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GraphBuildReport {
    pub retained_symbols: u64,
    pub omitted_symbols: u64,
    pub retained_edges: u64,
    pub omitted_edges: u64,
    pub edges_omitted_by_symbol_budget: u64,
    pub edges_omitted_by_edge_budget: u64,
    pub edges_omitted_by_call_site_budget: u64,
    pub retained_call_sites: u64,
    pub omitted_call_sites: u64,
    pub call_sites_omitted_by_symbol_budget: u64,
    pub call_sites_omitted_by_edge_budget: u64,
    pub call_sites_omitted_by_call_site_budget: u64,
}

/// Deterministic facade that refuses allocations before a graph budget is
/// exceeded. Files are always retained because source admission is bounded
/// before parsing and file/text queries are the degraded baseline.
#[derive(Debug)]
pub struct BoundedGraphBuilder {
    graph: SymbolGraph,
    limits: GraphBuildLimits,
    report: GraphBuildReport,
}

impl BoundedGraphBuilder {
    pub fn new(limits: GraphBuildLimits) -> Self {
        Self {
            graph: SymbolGraph::new(),
            limits,
            report: GraphBuildReport::default(),
        }
    }

    pub fn add_file(
        &mut self,
        path: RepoRelativePath,
        source: impl Into<Arc<str>>,
    ) -> Result<(), GraphError> {
        self.graph.add_file(path, source)
    }

    pub fn add_file_with_metadata(
        &mut self,
        path: RepoRelativePath,
        source: impl Into<Arc<str>>,
        metadata: SourceMetadata,
    ) -> Result<(), GraphError> {
        self.graph.add_file_with_metadata(path, source, metadata)
    }

    pub fn add_file_with_metadata_and_diagnostics(
        &mut self,
        path: RepoRelativePath,
        source: impl Into<Arc<str>>,
        metadata: SourceMetadata,
        diagnostics: Vec<SyntaxDiagnostic>,
        diagnostic_count: u64,
    ) -> Result<(), GraphError> {
        self.graph.add_file_with_metadata_and_diagnostics(
            path,
            source,
            metadata,
            diagnostics,
            diagnostic_count,
        )
    }

    pub fn add_symbol(
        &mut self,
        key: SymbolKey,
        location: SourceRange,
        signature: Option<String>,
        provenance: Provenance,
        precision: Precision,
    ) -> Result<Option<EntityId>, GraphError> {
        if self.graph.symbol_count() >= self.limits.max_symbols {
            self.report.omitted_symbols = self.report.omitted_symbols.saturating_add(1);
            return Ok(None);
        }
        self.graph
            .add_symbol(key, location, signature, provenance, precision)
            .map(Some)
    }

    pub fn add_edge(
        &mut self,
        kind: EdgeKind,
        from: EntityId,
        to: EntityId,
        provenance: Provenance,
        precision: Precision,
        location: Option<SourceRange>,
    ) -> Result<bool, GraphError> {
        if self.graph.edge_count() >= self.limits.max_edges {
            self.omit_edges_for_edge_budget(1);
            return Ok(false);
        }
        self.graph
            .add_edge(kind, from, to, provenance, precision, location)?;
        Ok(true)
    }

    pub fn add_edge_owned_by(
        &mut self,
        owner: RepoRelativePath,
        edge: Edge,
    ) -> Result<bool, GraphError> {
        if self.graph.edge_count() >= self.limits.max_edges {
            self.omit_edges_for_edge_budget(1);
            return Ok(false);
        }
        self.graph.add_edge_owned_by(owner, edge)?;
        Ok(true)
    }

    pub fn add_call_site(&mut self, input: CallSiteInput) -> Result<bool, GraphError> {
        let required_edges = self.graph.call_site_edge_cost(&input)?;
        if self.graph.call_site_count() >= self.limits.max_call_sites {
            self.omit_call_sites_for_call_site_budget(1);
            self.omit_edges_for_call_site_budget(required_edges);
            return Ok(false);
        }
        if self.graph.edge_count().saturating_add(required_edges) > self.limits.max_edges {
            self.omit_call_sites_for_edge_budget(1);
            self.omit_edges_for_edge_budget(required_edges);
            return Ok(false);
        }
        self.graph.add_call_site(input)?;
        Ok(true)
    }

    pub fn omit_edges_for_symbol_budget(&mut self, count: u64) {
        self.report.omitted_edges = self.report.omitted_edges.saturating_add(count);
        self.report.edges_omitted_by_symbol_budget = self
            .report
            .edges_omitted_by_symbol_budget
            .saturating_add(count);
    }

    pub fn omit_edges_for_edge_budget(&mut self, count: u64) {
        self.report.omitted_edges = self.report.omitted_edges.saturating_add(count);
        self.report.edges_omitted_by_edge_budget = self
            .report
            .edges_omitted_by_edge_budget
            .saturating_add(count);
    }

    fn omit_edges_for_call_site_budget(&mut self, count: u64) {
        self.report.omitted_edges = self.report.omitted_edges.saturating_add(count);
        self.report.edges_omitted_by_call_site_budget = self
            .report
            .edges_omitted_by_call_site_budget
            .saturating_add(count);
    }

    pub fn omit_call_sites_for_symbol_budget(&mut self, count: u64) {
        self.report.omitted_call_sites = self.report.omitted_call_sites.saturating_add(count);
        self.report.call_sites_omitted_by_symbol_budget = self
            .report
            .call_sites_omitted_by_symbol_budget
            .saturating_add(count);
    }

    fn omit_call_sites_for_edge_budget(&mut self, count: u64) {
        self.report.omitted_call_sites = self.report.omitted_call_sites.saturating_add(count);
        self.report.call_sites_omitted_by_edge_budget = self
            .report
            .call_sites_omitted_by_edge_budget
            .saturating_add(count);
    }

    fn omit_call_sites_for_call_site_budget(&mut self, count: u64) {
        self.report.omitted_call_sites = self.report.omitted_call_sites.saturating_add(count);
        self.report.call_sites_omitted_by_call_site_budget = self
            .report
            .call_sites_omitted_by_call_site_budget
            .saturating_add(count);
    }

    pub fn omitted_symbols(&self) -> u64 {
        self.report.omitted_symbols
    }

    pub fn finish(mut self) -> (SymbolGraph, GraphBuildReport) {
        self.report.retained_symbols = self.graph.symbol_count();
        self.report.retained_edges = self.graph.edge_count();
        self.report.retained_call_sites = self.graph.call_site_count();
        (self.graph, self.report)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CallLookupKey {
    language: Language,
    target_kind: CallTargetKind,
    name: String,
    qualifier: Option<String>,
}

/// Work observed by one complete, independent graph consistency audit.
///
/// The adjacency count is deterministic proof of the audit's linear work:
/// every logical edge must appear once in each directional index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsistencyAudit {
    pub symbols_audited: u64,
    pub files_audited: u64,
    pub edges_audited: u64,
    pub adjacency_entries_examined: u64,
    pub elapsed: Duration,
}

#[derive(Debug, Clone)]
struct IndexedFile {
    symbols: Vec<EntityId>,
    /// Immutable source captured in the same published revision as the
    /// syntax facts. `Arc<str>` keeps private snapshot clones cheap.
    source: Option<Arc<str>>,
    provenance: Provenance,
    precision: Precision,
    metadata: SourceMetadata,
    diagnostics: Vec<SyntaxDiagnostic>,
    diagnostic_count: u64,
}

/// Deterministic file view used by repository-level queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphFileSummary {
    pub path: RepoRelativePath,
    pub symbol_count: u64,
    pub provenance: Provenance,
    pub precision: Precision,
    pub metadata: SourceMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDiagnosticSummary {
    pub files_with_diagnostics: u64,
    pub total_diagnostics: u64,
    pub diagnostics: Vec<SyntaxDiagnostic>,
    pub capture_omitted: u64,
    pub response_omitted: u64,
}

/// Symbols and typed relations of one workspace revision.
///
/// Mutated only through `add_*` while a snapshot is being built privately;
/// once published it is immutable behind an `Arc`.
#[derive(Debug, Clone, Default)]
pub struct SymbolGraph {
    /// Workspace composition is a shallow immutable list of disjoint
    /// language partitions. Owned language graphs keep this `None`.
    parts: Option<Arc<Vec<SymbolGraph>>>,
    /// Persistent ordered arena. Values are independently shared so updating
    /// a trie path never copies unchanged symbol payloads.
    symbols: RedBlackTreeMapSync<EntityId, Arc<Symbol>>,
    /// Exact simple and qualified name lookup used before bounded traversal.
    symbols_by_exact_name: HashTrieMapSync<String, Arc<Vec<EntityId>>>,
    /// Case-folded simple and qualified names used by case-insensitive search.
    symbols_by_folded_name: HashTrieMapSync<String, Arc<Vec<EntityId>>>,
    /// File → captured source plus entities declared in it.
    files: RedBlackTreeMapSync<RepoRelativePath, Arc<IndexedFile>>,
    outgoing: HashTrieMapSync<EntityId, Arc<Vec<Edge>>>,
    incoming: HashTrieMapSync<EntityId, Arc<Vec<Edge>>>,
    /// Non-call syntax relationships grouped by the file contribution that
    /// produced them. This is the delta boundary used by live reconciliation.
    relationship_edges_by_owner: RedBlackTreeMapSync<RepoRelativePath, Arc<Vec<Edge>>>,
    edge_count: u64,
    call_sites: RedBlackTreeMapSync<u64, Arc<CallSite>>,
    call_sites_by_caller: HashTrieMapSync<EntityId, Arc<Vec<u64>>>,
    call_sites_by_lookup: HashTrieMapSync<CallLookupKey, Arc<Vec<u64>>>,
    callables: HashTrieMapSync<CallLookupKey, Arc<Vec<EntityId>>>,
    ambiguous_call_sites: u64,
    unresolved_call_sites: u64,
    /// Legacy eager-resolution truncation count. Lazy call candidates are
    /// retained compactly and bounded only when a query expands them, so
    /// current language indexes keep this at zero.
    truncated_call_sites: u64,
    /// Per-slot next entity id, indexed by `language_entity_slot`.
    next_entity_ids: [u64; ENTITY_ID_SLOT_COUNT],
    next_call_site_id: u64,
    /// Per-slot live symbol counts, indexed by `language_entity_slot`.
    symbol_counts: [u64; ENTITY_ID_SLOT_COUNT],
    adjacency_entries_copied: u64,
}

/// Borrowed deterministic symbol view kept as a small compatibility facade
/// while the underlying arena uses persistent tree nodes.
#[derive(Debug, Clone, Copy)]
pub struct Symbols<'a> {
    graph: &'a SymbolGraph,
}

impl<'a> Symbols<'a> {
    pub fn iter(&self) -> Box<dyn DoubleEndedIterator<Item = &'a Symbol> + 'a> {
        self.graph.symbol_iterator()
    }

    pub fn first(&self) -> Option<&'a Symbol> {
        self.graph.symbol_iterator().next()
    }
}

impl<'a> IntoIterator for Symbols<'a> {
    type Item = &'a Symbol;
    type IntoIter = Box<dyn DoubleEndedIterator<Item = &'a Symbol> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.graph.symbol_iterator()
    }
}

impl SymbolGraph {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_owned(&self) -> Result<(), GraphError> {
        if self.parts.is_some() {
            Err(GraphError::CompositeMutation)
        } else {
            Ok(())
        }
    }

    fn languages(&self) -> Vec<Language> {
        if let Some(parts) = self.parts.as_ref() {
            let mut languages = Vec::new();
            for part in parts.iter() {
                for language in part.languages() {
                    if !languages.contains(&language) {
                        languages.push(language);
                    }
                }
            }
            return languages;
        }
        let mut languages = Vec::with_capacity(ENTITY_SLOT_LANGUAGES.len());
        for (slot, language) in ENTITY_SLOT_LANGUAGES.iter().enumerate() {
            if self.symbol_counts[slot] != 0 {
                languages.push(*language);
            }
        }
        languages
    }

    fn symbol_iterator<'a>(&'a self) -> Box<dyn DoubleEndedIterator<Item = &'a Symbol> + 'a> {
        if let Some(parts) = self.parts.as_ref() {
            Box::new(parts.iter().flat_map(|part| part.symbol_iterator()))
        } else {
            Box::new(self.symbols.iter().map(|(_, symbol)| symbol.as_ref()))
        }
    }

    /// Combines independently built, disjoint language graphs into one
    /// revision-local workspace view without copying or remapping unchanged
    /// facts. Language-scoped entity-id ranges keep partition ids disjoint.
    /// Overlapping languages are rejected because each input has already
    /// resolved its call sites against its own callable catalog.
    pub fn merge(graphs: impl IntoIterator<Item = SymbolGraph>) -> Result<Self, GraphError> {
        let mut parts = Vec::new();
        let mut languages = HashSet::new();
        for graph in graphs {
            for language in graph.languages() {
                if !languages.insert(language) {
                    return Err(GraphError::OverlappingLanguageGraph { language });
                }
            }
            if let Some(nested) = graph.parts.as_ref() {
                parts.extend(nested.iter().cloned());
            } else {
                parts.push(graph);
            }
        }
        Ok(Self {
            parts: Some(Arc::new(parts)),
            ..Self::default()
        })
    }

    /// Adds one discovered source file and the exact text parsed for this
    /// graph revision. Files with no extractable declarations still appear
    /// in `repo_map` and text search.
    pub fn add_file(
        &mut self,
        path: RepoRelativePath,
        source: impl Into<Arc<str>>,
    ) -> Result<(), GraphError> {
        let metadata = SourceMetadata::path_fallback(&path);
        self.add_file_with_metadata(path, source, metadata)
    }

    /// Adds one discovered source with explicit language-neutral role and
    /// package metadata.
    pub fn add_file_with_metadata(
        &mut self,
        path: RepoRelativePath,
        source: impl Into<Arc<str>>,
        metadata: SourceMetadata,
    ) -> Result<(), GraphError> {
        self.add_file_with_metadata_and_diagnostics(path, source, metadata, Vec::new(), 0)
    }

    /// Adds one discovered source plus bounded diagnostics from the exact
    /// Tree-sitter parse that produced this graph revision.
    pub fn add_file_with_metadata_and_diagnostics(
        &mut self,
        path: RepoRelativePath,
        source: impl Into<Arc<str>>,
        metadata: SourceMetadata,
        diagnostics: Vec<SyntaxDiagnostic>,
        diagnostic_count: u64,
    ) -> Result<(), GraphError> {
        validate_diagnostics(&path, &diagnostics, diagnostic_count)?;
        self.ensure_owned()?;
        match self.files.get(&path) {
            None => {
                self.files.insert_mut(
                    path,
                    Arc::new(IndexedFile {
                        symbols: Vec::new(),
                        source: Some(source.into()),
                        provenance: Provenance::Git,
                        precision: Precision::Precise,
                        metadata,
                        diagnostics,
                        diagnostic_count,
                    }),
                );
            }
            Some(existing) => {
                if existing.source.is_some() {
                    return Err(GraphError::DuplicateFile(path));
                }
                let mut file = existing.as_ref().clone();
                file.source = Some(source.into());
                file.provenance = Provenance::Git;
                file.precision = Precision::Precise;
                file.metadata = metadata;
                file.diagnostics = diagnostics;
                file.diagnostic_count = diagnostic_count;
                self.files.insert_mut(path, Arc::new(file));
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
        self.ensure_owned()?;
        if &key.path != location.file() {
            return Err(GraphError::KeyLocationMismatch {
                key_path: key.path.as_str().to_owned(),
                location_path: location.file().as_str().to_owned(),
            });
        }
        let slot = language_entity_slot(key.language);
        let counter = self.next_entity_ids[slot];
        if counter >= ENTITY_ID_COUNTER_LIMIT {
            return Err(GraphError::EntityIdSpaceExhausted(key.language));
        }
        // slot < 16 and counter < 2^60, so the tagged id always fits u64.
        let id = EntityId(((slot as u64) << ENTITY_ID_SLOT_SHIFT) + counter);
        self.next_entity_ids[slot] = counter + 1;
        self.symbol_counts[slot] = self.symbol_counts[slot].saturating_add(1);
        let symbol = Symbol {
            id,
            key,
            location,
            signature,
            provenance,
            precision,
        };
        let file_path = symbol.location.file().clone();
        let mut file = self
            .files
            .get(&file_path)
            .map(|file| file.as_ref().clone())
            .unwrap_or_else(|| IndexedFile {
                symbols: Vec::new(),
                source: None,
                provenance: symbol.provenance,
                precision: symbol.precision,
                metadata: SourceMetadata::path_fallback(&file_path),
                diagnostics: Vec::new(),
                diagnostic_count: 0,
            });
        file.symbols.push(id);
        self.files.insert_mut(file_path, Arc::new(file));
        for lookup in callable_lookup_keys(&symbol) {
            let mut ids = self
                .callables
                .get(&lookup)
                .map_or_else(Vec::new, |ids| ids.as_ref().clone());
            ids.push(id);
            self.callables.insert_mut(lookup, Arc::new(ids));
        }
        let simple_name = symbol.name().to_owned();
        for name in [simple_name.clone(), symbol.key.qualified_name.clone()] {
            let mut ids = self
                .symbols_by_exact_name
                .get(&name)
                .map_or_else(Vec::new, |ids| ids.as_ref().clone());
            ids.push(id);
            self.symbols_by_exact_name.insert_mut(name, Arc::new(ids));
            if simple_name == symbol.key.qualified_name {
                break;
            }
        }
        let folded_simple_name = simple_name.to_lowercase();
        let folded_qualified_name = symbol.key.qualified_name.to_lowercase();
        for name in [folded_simple_name.clone(), folded_qualified_name.clone()] {
            let mut ids = self
                .symbols_by_folded_name
                .get(&name)
                .map_or_else(Vec::new, |ids| ids.as_ref().clone());
            ids.push(id);
            self.symbols_by_folded_name.insert_mut(name, Arc::new(ids));
            if folded_simple_name == folded_qualified_name {
                break;
            }
        }
        self.symbols.insert_mut(id, Arc::new(symbol));
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
        self.ensure_owned()?;
        let owner = location
            .as_ref()
            .map(|range| range.file().clone())
            .or_else(|| self.symbol(from).map(|symbol| symbol.key.path.clone()))
            .ok_or(GraphError::UnknownEntity(from))?;
        self.add_edge_owned_by(
            owner,
            Edge {
                kind,
                from,
                to,
                provenance,
                precision,
                location,
            },
        )
    }

    /// Adds one syntax relationship owned by a specific file contribution.
    /// Ownership is private graph-maintenance metadata and never changes the
    /// public edge/provenance contract.
    pub fn add_edge_owned_by(
        &mut self,
        owner: RepoRelativePath,
        edge: Edge,
    ) -> Result<(), GraphError> {
        self.ensure_owned()?;
        self.add_edge_raw(edge.clone())?;
        let mut owned = self
            .relationship_edges_by_owner
            .get(&owner)
            .map_or_else(Vec::new, |edges| edges.as_ref().clone());
        owned.push(edge);
        self.relationship_edges_by_owner
            .insert_mut(owner, Arc::new(owned));
        Ok(())
    }

    fn add_edge_raw(&mut self, edge: Edge) -> Result<(), GraphError> {
        let from = edge.from;
        let to = edge.to;
        for id in [from, to] {
            if self.symbol(id).is_none() {
                return Err(GraphError::UnknownEntity(id));
            }
        }
        let outgoing_copied = self.outgoing.get(&from).map_or(0, |edges| edges.len()) as u64;
        let mut outgoing = self
            .outgoing
            .get(&from)
            .map_or_else(Vec::new, |edges| edges.as_ref().clone());
        outgoing.push(edge.clone());
        self.outgoing.insert_mut(from, Arc::new(outgoing));
        let incoming_copied = self.incoming.get(&to).map_or(0, |edges| edges.len()) as u64;
        let mut incoming = self
            .incoming
            .get(&to)
            .map_or_else(Vec::new, |edges| edges.as_ref().clone());
        incoming.push(edge);
        self.incoming.insert_mut(to, Arc::new(incoming));
        self.adjacency_entries_copied = self
            .adjacency_entries_copied
            .saturating_add(outgoing_copied)
            .saturating_add(incoming_copied);
        self.edge_count += 1;
        Ok(())
    }

    /// Removes the exact non-call relationships contributed by `owner`.
    pub fn remove_relationships_in_file(
        &mut self,
        owner: &RepoRelativePath,
    ) -> Result<u64, GraphError> {
        self.ensure_owned()?;
        let Some(edges) = self.relationship_edges_by_owner.get(owner).cloned() else {
            return Ok(0);
        };
        for edge in edges.iter() {
            self.remove_edge_raw(edge)?;
        }
        self.relationship_edges_by_owner.remove_mut(owner);
        Ok(edges.len() as u64)
    }

    fn remove_edge_raw(&mut self, edge: &Edge) -> Result<(), GraphError> {
        let outgoing_copied = remove_adjacency_edge(&mut self.outgoing, edge.from, edge)?;
        let incoming_copied = remove_adjacency_edge(&mut self.incoming, edge.to, edge)?;
        self.adjacency_entries_copied = self
            .adjacency_entries_copied
            .saturating_add(outgoing_copied)
            .saturating_add(incoming_copied);
        self.edge_count = self.edge_count.saturating_sub(1);
        Ok(())
    }

    /// Removes a file and its declarations after callers/relationships that
    /// reference them have been removed from the same private update.
    pub fn remove_file(&mut self, path: &RepoRelativePath) -> Result<bool, GraphError> {
        self.ensure_owned()?;
        let Some(file) = self.files.get(path).cloned() else {
            return Ok(false);
        };
        for id in &file.symbols {
            if !self.outgoing_edges(*id).is_empty()
                || !self.incoming_edges(*id).is_empty()
                || self.call_sites_by_caller.get(id).is_some()
            {
                return Err(GraphError::EntityStillReferenced { id: *id });
            }
        }
        for id in &file.symbols {
            let Some(symbol) = self.symbol(*id).cloned() else {
                return Err(GraphError::UnknownEntity(*id));
            };
            for lookup in callable_lookup_keys(&symbol) {
                let Some(existing) = self.callables.get(&lookup) else {
                    continue;
                };
                let mut ids = existing.as_ref().clone();
                ids.retain(|candidate| candidate != id);
                if ids.is_empty() {
                    self.callables.remove_mut(&lookup);
                } else {
                    self.callables.insert_mut(lookup, Arc::new(ids));
                }
            }
            let simple_name = symbol.name().to_owned();
            for name in [simple_name.clone(), symbol.key.qualified_name.clone()] {
                if let Some(existing) = self.symbols_by_exact_name.get(&name) {
                    let mut ids = existing.as_ref().clone();
                    ids.retain(|candidate| candidate != id);
                    if ids.is_empty() {
                        self.symbols_by_exact_name.remove_mut(&name);
                    } else {
                        self.symbols_by_exact_name
                            .insert_mut(name.clone(), Arc::new(ids));
                    }
                }
                if simple_name == symbol.key.qualified_name {
                    break;
                }
            }
            let folded_simple_name = simple_name.to_lowercase();
            let folded_qualified_name = symbol.key.qualified_name.to_lowercase();
            for name in [folded_simple_name.clone(), folded_qualified_name.clone()] {
                if let Some(existing) = self.symbols_by_folded_name.get(&name) {
                    let mut ids = existing.as_ref().clone();
                    ids.retain(|candidate| candidate != id);
                    if ids.is_empty() {
                        self.symbols_by_folded_name.remove_mut(&name);
                    } else {
                        self.symbols_by_folded_name
                            .insert_mut(name.clone(), Arc::new(ids));
                    }
                }
                if folded_simple_name == folded_qualified_name {
                    break;
                }
            }
            let slot = language_entity_slot(symbol.key.language);
            self.symbol_counts[slot] = self.symbol_counts[slot].saturating_sub(1);
            self.symbols.remove_mut(id);
        }
        self.files.remove_mut(path);
        Ok(true)
    }

    /// Replaces captured text while preserving the file's declaration ids.
    pub fn replace_file_source(
        &mut self,
        path: &RepoRelativePath,
        source: Arc<str>,
    ) -> Result<(), GraphError> {
        self.ensure_owned()?;
        let Some(existing) = self.files.get(path) else {
            return Err(GraphError::UnknownFile(path.clone()));
        };
        let mut file = existing.as_ref().clone();
        file.source = Some(source);
        file.provenance = Provenance::Git;
        file.precision = Precision::Precise;
        self.files.insert_mut(path.clone(), Arc::new(file));
        Ok(())
    }

    /// Replaces captured text and diagnostics while preserving declaration
    /// ids for an incrementally reparsed file with stable symbol keys.
    pub fn replace_file_source_and_diagnostics(
        &mut self,
        path: &RepoRelativePath,
        source: Arc<str>,
        diagnostics: Vec<SyntaxDiagnostic>,
        diagnostic_count: u64,
    ) -> Result<(), GraphError> {
        validate_diagnostics(path, &diagnostics, diagnostic_count)?;
        self.ensure_owned()?;
        let Some(existing) = self.files.get(path) else {
            return Err(GraphError::UnknownFile(path.clone()));
        };
        let mut file = existing.as_ref().clone();
        file.source = Some(source);
        file.provenance = Provenance::Git;
        file.precision = Precision::Precise;
        file.diagnostics = diagnostics;
        file.diagnostic_count = diagnostic_count;
        self.files.insert_mut(path.clone(), Arc::new(file));
        Ok(())
    }

    /// Replaces revision-local symbol details while retaining an id only when
    /// its language-aware key is unchanged.
    pub fn replace_symbol_payload(
        &mut self,
        id: EntityId,
        key: SymbolKey,
        location: SourceRange,
        signature: Option<String>,
        provenance: Provenance,
        precision: Precision,
    ) -> Result<bool, GraphError> {
        self.ensure_owned()?;
        if key.path != *location.file() {
            return Err(GraphError::KeyLocationMismatch {
                key_path: key.path.as_str().to_owned(),
                location_path: location.file().as_str().to_owned(),
            });
        }
        let Some(existing) = self.symbols.get(&id) else {
            return Err(GraphError::UnknownEntity(id));
        };
        if existing.key != key {
            return Err(GraphError::PreservedEntityKeyChanged { id });
        }
        let replacement = Symbol {
            id,
            key,
            location,
            signature,
            provenance,
            precision,
        };
        if existing.as_ref() == &replacement {
            return Ok(false);
        }
        self.symbols.insert_mut(id, Arc::new(replacement));
        Ok(true)
    }

    pub fn symbol(&self, id: EntityId) -> Option<&Symbol> {
        if let Some(parts) = self.parts.as_ref() {
            return parts.iter().find_map(|part| part.symbol(id));
        }
        self.symbols.get(&id).map(Arc::as_ref)
    }

    pub fn symbols(&self) -> Symbols<'_> {
        Symbols { graph: self }
    }

    pub fn symbol_count(&self) -> u64 {
        if let Some(parts) = self.parts.as_ref() {
            return parts.iter().map(SymbolGraph::symbol_count).sum();
        }
        self.symbols.size() as u64
    }

    pub fn edge_count(&self) -> u64 {
        if let Some(parts) = self.parts.as_ref() {
            return parts.iter().map(SymbolGraph::edge_count).sum();
        }
        self.edge_count
    }

    /// Cumulative adjacency `Edge` payload copies performed while replacing
    /// persistent per-entity vectors. A revision reports the delta from its
    /// base graph, not this lifetime total.
    pub fn adjacency_entries_copied(&self) -> u64 {
        if let Some(parts) = self.parts.as_ref() {
            return parts
                .iter()
                .map(SymbolGraph::adjacency_entries_copied)
                .sum();
        }
        self.adjacency_entries_copied
    }

    pub fn call_site_count(&self) -> u64 {
        if let Some(parts) = self.parts.as_ref() {
            return parts.iter().map(SymbolGraph::call_site_count).sum();
        }
        self.call_sites.size() as u64
    }

    pub fn ambiguous_call_site_count(&self) -> u64 {
        if let Some(parts) = self.parts.as_ref() {
            return parts
                .iter()
                .map(SymbolGraph::ambiguous_call_site_count)
                .sum();
        }
        self.ambiguous_call_sites
    }

    pub fn unresolved_call_site_count(&self) -> u64 {
        if let Some(parts) = self.parts.as_ref() {
            return parts
                .iter()
                .map(SymbolGraph::unresolved_call_site_count)
                .sum();
        }
        self.unresolved_call_sites
    }

    /// Adds one compact syntax call site and materializes graph edges only
    /// when its target resolves to exactly one declaration.
    pub fn add_call_site(&mut self, input: CallSiteInput) -> Result<CallResolution, GraphError> {
        self.ensure_owned()?;
        self.validate_call_site_input(&input)?;
        let language = self
            .symbol(input.caller)
            .ok_or(GraphError::UnknownEntity(input.caller))?
            .key
            .language;
        let resolution = self.resolve_call(
            language,
            input.form,
            input.target_kind,
            &input.name,
            input.qualifier.as_deref(),
        );
        if let CallResolution::Resolved { target } = resolution {
            let (calls_provenance, calls_precision) =
                call_relation_tier(input.provenance, input.precision);
            self.add_edge_raw(Edge {
                kind: EdgeKind::Calls,
                from: input.caller,
                to: target,
                provenance: calls_provenance,
                precision: calls_precision,
                location: Some(input.location.clone()),
            })?;
            if self
                .symbol(input.caller)
                .is_some_and(|symbol| symbol.key.kind == SymbolKind::Test)
                && !self.call_sites_from(input.caller).any(|call_site| {
                    matches!(
                        call_site.resolution,
                        CallResolution::Resolved { target: previous } if previous == target
                    )
                })
            {
                let (tests_provenance, tests_precision) =
                    test_relation_tier(input.provenance, input.precision);
                self.add_edge_raw(Edge {
                    kind: EdgeKind::Tests,
                    from: input.caller,
                    to: target,
                    provenance: tests_provenance,
                    precision: tests_precision,
                    location: Some(input.location.clone()),
                })?;
            }
        }
        self.insert_call_site(CallSite {
            caller: input.caller,
            form: input.form,
            target_kind: input.target_kind,
            name: input.name,
            qualifier: input.qualifier,
            receiver_type: input.receiver_type,
            receiver_type_source: input.receiver_type_source,
            receiver_hint: input.receiver_hint,
            location: input.location,
            resolution: resolution.clone(),
            provenance: input.provenance,
            precision: input.precision,
        })?;
        Ok(resolution)
    }

    fn call_site_edge_cost(&self, input: &CallSiteInput) -> Result<u64, GraphError> {
        self.validate_call_site_input(input)?;
        let caller = self
            .symbol(input.caller)
            .ok_or(GraphError::UnknownEntity(input.caller))?;
        let resolution = self.resolve_call(
            caller.key.language,
            input.form,
            input.target_kind,
            &input.name,
            input.qualifier.as_deref(),
        );
        Ok(match resolution {
            CallResolution::Resolved { .. } if caller.key.kind == SymbolKind::Test => 2,
            CallResolution::Resolved { .. } => 1,
            CallResolution::Ambiguous { .. } | CallResolution::Unresolved => 0,
        })
    }

    /// Records legacy eager call-candidate incompleteness.
    pub fn set_truncated_call_sites(
        &mut self,
        truncated_call_sites: u64,
    ) -> Result<(), GraphError> {
        self.ensure_owned()?;
        self.truncated_call_sites = truncated_call_sites;
        Ok(())
    }

    /// Legacy eager call sites cut while building this graph revision.
    pub fn truncated_call_sites(&self) -> u64 {
        if let Some(parts) = self.parts.as_ref() {
            return parts.iter().map(SymbolGraph::truncated_call_sites).sum();
        }
        self.truncated_call_sites
    }

    pub fn file_count(&self) -> u64 {
        if let Some(parts) = self.parts.as_ref() {
            return parts.iter().map(SymbolGraph::file_count).sum();
        }
        self.files.size() as u64
    }

    /// Files with the number of symbols declared in each, sorted by path.
    pub fn file_summaries(&self) -> Vec<GraphFileSummary> {
        if let Some(parts) = self.parts.as_ref() {
            let mut summaries: Vec<_> =
                parts.iter().flat_map(SymbolGraph::file_summaries).collect();
            summaries.sort_by(|a, b| a.path.cmp(&b.path));
            return summaries;
        }
        let mut summaries: Vec<_> = self
            .files
            .iter()
            .map(|(path, file)| GraphFileSummary {
                path: path.clone(),
                symbol_count: file.symbols.len() as u64,
                provenance: file.provenance,
                precision: file.precision,
                metadata: file.metadata.clone(),
            })
            .collect();
        summaries.sort_by(|a, b| a.path.cmp(&b.path));
        summaries
    }

    /// Streams file summaries in global path order without first
    /// materializing the complete workspace inventory. Composite graphs use
    /// a small k-way merge over their immutable ordered language partitions.
    pub fn file_summaries_iter(&self) -> Box<dyn Iterator<Item = GraphFileSummary> + '_> {
        if let Some(parts) = self.parts.as_ref() {
            let mut iterators: Vec<_> = parts
                .iter()
                .map(|part| part.file_summaries_iter().peekable())
                .collect();
            return Box::new(std::iter::from_fn(move || {
                let mut best: Option<(usize, RepoRelativePath)> = None;
                for (index, iterator) in iterators.iter_mut().enumerate() {
                    let Some(summary) = iterator.peek() else {
                        continue;
                    };
                    if best
                        .as_ref()
                        .is_none_or(|(_, path)| summary.path.cmp(path) == Ordering::Less)
                    {
                        best = Some((index, summary.path.clone()));
                    }
                }
                let (index, _) = best?;
                iterators[index].next()
            }));
        }
        Box::new(self.files.iter().map(|(path, file)| GraphFileSummary {
            path: path.clone(),
            symbol_count: file.symbols.len() as u64,
            provenance: file.provenance,
            precision: file.precision,
            metadata: file.metadata.clone(),
        }))
    }

    pub fn file_summaries_with_context(
        &self,
        operation: &OperationContext,
    ) -> Result<Vec<GraphFileSummary>, OperationAbort> {
        if let Some(parts) = self.parts.as_ref() {
            let mut summaries = Vec::new();
            for part in parts.iter() {
                operation.check()?;
                summaries.extend(part.file_summaries_with_context(operation)?);
            }
            summaries.sort_by(|a, b| a.path.cmp(&b.path));
            return Ok(summaries);
        }
        let mut summaries = Vec::with_capacity(self.files.size());
        for (index, (path, file)) in self.files.iter().enumerate() {
            if index % CANCELLATION_POLL_ITEMS == 0 {
                operation.check()?;
            }
            summaries.push(GraphFileSummary {
                path: path.clone(),
                symbol_count: file.symbols.len() as u64,
                provenance: file.provenance,
                precision: file.precision,
                metadata: file.metadata.clone(),
            });
        }
        summaries.sort_by(|a, b| a.path.cmp(&b.path));
        operation.check()?;
        Ok(summaries)
    }

    pub fn file_metadata(&self, path: &RepoRelativePath) -> Option<&SourceMetadata> {
        if let Some(parts) = self.parts.as_ref() {
            return parts.iter().find_map(|part| part.file_metadata(path));
        }
        self.files.get(path).map(|file| &file.metadata)
    }

    pub fn source_metadata_coverage(&self) -> SourceMetadataCoverage {
        if let Some(parts) = self.parts.as_ref() {
            return parts
                .iter()
                .fold(SourceMetadataCoverage::default(), |mut coverage, part| {
                    let part = part.source_metadata_coverage();
                    coverage.total_files = coverage.total_files.saturating_add(part.total_files);
                    coverage.cargo_metadata_files = coverage
                        .cargo_metadata_files
                        .saturating_add(part.cargo_metadata_files);
                    coverage.composer_metadata_files = coverage
                        .composer_metadata_files
                        .saturating_add(part.composer_metadata_files);
                    coverage.package_json_metadata_files = coverage
                        .package_json_metadata_files
                        .saturating_add(part.package_json_metadata_files);
                    coverage.pyproject_metadata_files = coverage
                        .pyproject_metadata_files
                        .saturating_add(part.pyproject_metadata_files);
                    coverage.maven_metadata_files = coverage
                        .maven_metadata_files
                        .saturating_add(part.maven_metadata_files);
                    coverage.gradle_metadata_files = coverage
                        .gradle_metadata_files
                        .saturating_add(part.gradle_metadata_files);
                    coverage.dotnet_project_metadata_files = coverage
                        .dotnet_project_metadata_files
                        .saturating_add(part.dotnet_project_metadata_files);
                    coverage.shell_project_metadata_files = coverage
                        .shell_project_metadata_files
                        .saturating_add(part.shell_project_metadata_files);
                    coverage.cpp_project_metadata_files = coverage
                        .cpp_project_metadata_files
                        .saturating_add(part.cpp_project_metadata_files);
                    coverage.path_fallback_files = coverage
                        .path_fallback_files
                        .saturating_add(part.path_fallback_files);
                    coverage
                });
        }
        let mut coverage = SourceMetadataCoverage {
            total_files: self.files.size() as u64,
            ..SourceMetadataCoverage::default()
        };
        for (_, file) in self.files.iter() {
            match file.metadata.classification {
                SourceClassification::CargoMetadata => coverage.cargo_metadata_files += 1,
                SourceClassification::ComposerMetadata => coverage.composer_metadata_files += 1,
                SourceClassification::PackageJsonMetadata => {
                    coverage.package_json_metadata_files += 1;
                }
                SourceClassification::PyprojectMetadata => {
                    coverage.pyproject_metadata_files += 1;
                }
                SourceClassification::MavenMetadata => {
                    coverage.maven_metadata_files += 1;
                }
                SourceClassification::GradleMetadata => {
                    coverage.gradle_metadata_files += 1;
                }
                SourceClassification::DotnetProjectMetadata => {
                    coverage.dotnet_project_metadata_files += 1;
                }
                SourceClassification::ShellProjectMetadata => {
                    coverage.shell_project_metadata_files += 1;
                }
                SourceClassification::CppProjectMetadata => {
                    coverage.cpp_project_metadata_files += 1;
                }
                SourceClassification::PathFallback => coverage.path_fallback_files += 1,
            }
        }
        coverage
    }

    /// Deterministic bounded diagnostic view for one immutable graph.
    pub fn syntax_diagnostics(&self, limit: usize) -> GraphDiagnosticSummary {
        if let Some(parts) = self.parts.as_ref() {
            let mut combined = GraphDiagnosticSummary {
                files_with_diagnostics: 0,
                total_diagnostics: 0,
                diagnostics: Vec::with_capacity(limit),
                capture_omitted: 0,
                response_omitted: 0,
            };
            let mut captured_diagnostics = 0_u64;
            for part in parts.iter() {
                let summary = part.syntax_diagnostics(limit);
                combined.files_with_diagnostics = combined
                    .files_with_diagnostics
                    .saturating_add(summary.files_with_diagnostics);
                combined.total_diagnostics = combined
                    .total_diagnostics
                    .saturating_add(summary.total_diagnostics);
                combined.capture_omitted = combined
                    .capture_omitted
                    .saturating_add(summary.capture_omitted);
                captured_diagnostics = captured_diagnostics
                    .saturating_add(summary.diagnostics.len() as u64)
                    .saturating_add(summary.response_omitted);
                for diagnostic in summary.diagnostics {
                    let position = combined
                        .diagnostics
                        .binary_search_by(|candidate| diagnostic_cmp(candidate, &diagnostic))
                        .unwrap_or_else(|position| position);
                    if combined.diagnostics.len() < limit {
                        combined.diagnostics.insert(position, diagnostic);
                    } else if position < limit {
                        combined.diagnostics.insert(position, diagnostic);
                        combined.diagnostics.pop();
                    }
                }
            }
            combined.response_omitted =
                captured_diagnostics.saturating_sub(combined.diagnostics.len() as u64);
            return combined;
        }
        let mut files_with_diagnostics = 0_u64;
        let mut total_diagnostics = 0_u64;
        let mut captured_diagnostics = 0_u64;
        let mut capture_omitted = 0_u64;
        let mut diagnostics = Vec::with_capacity(limit);
        for file in self.files.values() {
            if file.diagnostic_count > 0 {
                files_with_diagnostics += 1;
            }
            total_diagnostics = total_diagnostics.saturating_add(file.diagnostic_count);
            captured_diagnostics =
                captured_diagnostics.saturating_add(file.diagnostics.len() as u64);
            capture_omitted = capture_omitted.saturating_add(
                file.diagnostic_count
                    .saturating_sub(file.diagnostics.len() as u64),
            );
            for diagnostic in &file.diagnostics {
                let position = diagnostics
                    .binary_search_by(|candidate| diagnostic_cmp(candidate, diagnostic))
                    .unwrap_or_else(|position| position);
                if diagnostics.len() < limit {
                    diagnostics.insert(position, diagnostic.clone());
                } else if position < limit {
                    diagnostics.insert(position, diagnostic.clone());
                    diagnostics.pop();
                }
            }
        }
        let response_omitted = captured_diagnostics.saturating_sub(diagnostics.len() as u64);
        GraphDiagnosticSummary {
            files_with_diagnostics,
            total_diagnostics,
            diagnostics,
            capture_omitted,
            response_omitted,
        }
    }

    /// Exact number of syntax diagnostics attributed to one indexed file.
    pub fn file_diagnostic_count(&self, path: &RepoRelativePath) -> Option<u64> {
        if let Some(parts) = self.parts.as_ref() {
            return parts
                .iter()
                .find_map(|part| part.file_diagnostic_count(path));
        }
        self.files.get(path).map(|file| file.diagnostic_count)
    }

    /// Captured source for one file in this graph revision.
    pub fn file_source(&self, path: &RepoRelativePath) -> Option<&str> {
        if let Some(parts) = self.parts.as_ref() {
            return parts.iter().find_map(|part| part.file_source(path));
        }
        self.files.get(path)?.source.as_deref()
    }

    /// Diagnostic proof that an unchanged file contribution is the exact
    /// same immutable allocation in two published revisions.
    pub fn shares_file_payload_with(&self, other: &SymbolGraph, path: &RepoRelativePath) -> bool {
        let Some(left) = self.file_payload(path) else {
            return false;
        };
        let Some(right) = other.file_payload(path) else {
            return false;
        };
        Arc::ptr_eq(left, right)
    }

    /// Diagnostic proof that one revision-local symbol payload was reused
    /// rather than cloned while assembling another revision.
    pub fn shares_symbol_payload_with(&self, other: &SymbolGraph, id: EntityId) -> bool {
        let Some(left) = self.symbol_payload(id) else {
            return false;
        };
        let Some(right) = other.symbol_payload(id) else {
            return false;
        };
        Arc::ptr_eq(left, right)
    }

    fn file_payload(&self, path: &RepoRelativePath) -> Option<&Arc<IndexedFile>> {
        if let Some(parts) = self.parts.as_ref() {
            return parts.iter().find_map(|part| part.file_payload(path));
        }
        self.files.get(path)
    }

    fn symbol_payload(&self, id: EntityId) -> Option<&Arc<Symbol>> {
        if let Some(parts) = self.parts.as_ref() {
            return parts.iter().find_map(|part| part.symbol_payload(id));
        }
        self.symbols.get(&id)
    }

    /// Captured source files sorted by repository-relative path.
    pub fn source_files(&self) -> Vec<(&RepoRelativePath, &str)> {
        if let Some(parts) = self.parts.as_ref() {
            let mut files: Vec<_> = parts.iter().flat_map(SymbolGraph::source_files).collect();
            files.sort_by(|a, b| a.0.cmp(b.0));
            return files;
        }
        let mut files: Vec<_> = self
            .files
            .iter()
            .filter_map(|(path, file)| file.source.as_deref().map(|source| (path, source)))
            .collect();
        files.sort_by(|a, b| a.0.cmp(b.0));
        files
    }

    /// Streams captured sources without materializing the complete inventory.
    pub fn source_files_iter(&self) -> Box<dyn Iterator<Item = (&RepoRelativePath, &str)> + '_> {
        if let Some(parts) = self.parts.as_ref() {
            return Box::new(parts.iter().flat_map(SymbolGraph::source_files_iter));
        }
        Box::new(
            self.files
                .iter()
                .filter_map(|(path, file)| file.source.as_deref().map(|source| (path, source))),
        )
    }

    pub fn source_files_with_context(
        &self,
        operation: &OperationContext,
    ) -> Result<Vec<(&RepoRelativePath, &str)>, OperationAbort> {
        if let Some(parts) = self.parts.as_ref() {
            let mut files = Vec::new();
            for part in parts.iter() {
                operation.check()?;
                files.extend(part.source_files_with_context(operation)?);
            }
            files.sort_by(|a, b| a.0.cmp(b.0));
            return Ok(files);
        }
        let mut files = Vec::with_capacity(self.files.size());
        for (index, (path, file)) in self.files.iter().enumerate() {
            if index % CANCELLATION_POLL_ITEMS == 0 {
                operation.check()?;
            }
            if let Some(source) = file.source.as_deref() {
                files.push((path, source));
            }
        }
        files.sort_by(|a, b| a.0.cmp(b.0));
        operation.check()?;
        Ok(files)
    }

    pub(crate) fn snapshot_documents_with_context(
        &self,
        operation: &OperationContext,
    ) -> Result<Vec<(RepoRelativePath, Arc<str>)>, OperationAbort> {
        if let Some(parts) = self.parts.as_ref() {
            let mut files = Vec::new();
            for part in parts.iter() {
                operation.check()?;
                files.extend(part.snapshot_documents_with_context(operation)?);
            }
            files.sort_by(|a, b| a.0.cmp(&b.0));
            return Ok(files);
        }
        let mut files = Vec::with_capacity(self.files.size());
        for (index, (path, file)) in self.files.iter().enumerate() {
            if index % CANCELLATION_POLL_ITEMS == 0 {
                operation.check()?;
            }
            if let Some(source) = file.source.as_ref() {
                files.push((path.clone(), source.clone()));
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        operation.check()?;
        Ok(files)
    }

    /// One captured source allocation from this immutable graph revision.
    /// Provider adapters use this targeted lookup instead of materializing the
    /// complete document catalog for every precise query.
    pub(crate) fn snapshot_document(&self, path: &RepoRelativePath) -> Option<Arc<str>> {
        if let Some(parts) = self.parts.as_ref() {
            return parts.iter().find_map(|part| part.snapshot_document(path));
        }
        self.files.get(path)?.source.clone()
    }

    /// Symbols declared in one file, in deterministic arena order.
    pub fn symbols_in_file<'a>(
        &'a self,
        path: &'a RepoRelativePath,
    ) -> Box<dyn Iterator<Item = &'a Symbol> + 'a> {
        if let Some(parts) = self.parts.as_ref() {
            return Box::new(
                parts
                    .iter()
                    .flat_map(move |part| part.symbols_in_file(path)),
            );
        }
        Box::new(
            self.files
                .get(path)
                .into_iter()
                .flat_map(|file| file.symbols.iter())
                .filter_map(|id| self.symbol(*id)),
        )
    }

    /// Case-insensitive substring search over qualified names. Result
    /// construction stops at the caller's budget plus the first omitted
    /// match, so a broad query cannot allocate one view per graph symbol.
    pub fn search_names(&self, needle: &str, limit: usize) -> (Vec<EntityId>, bool) {
        self.search_names_where(needle, limit, |_, _| true)
    }

    /// Bounded name search with a file-metadata predicate applied before the
    /// result budget, so filtered-out symbols cannot crowd out matching ones.
    pub fn search_names_where(
        &self,
        needle: &str,
        limit: usize,
        mut predicate: impl FnMut(&Symbol, &SourceMetadata) -> bool,
    ) -> (Vec<EntityId>, bool) {
        let needle = needle.to_lowercase();
        let mut matches = Vec::with_capacity(limit.min(self.symbol_count() as usize));
        for symbol in self.symbols() {
            // The simple name is a suffix of the qualified name, so one
            // comparison covers both without a second lowercase allocation.
            let Some(metadata) = self.file_metadata(&symbol.key.path) else {
                continue;
            };
            if symbol.key.qualified_name.to_lowercase().contains(&needle)
                && predicate(symbol, metadata)
            {
                if matches.len() == limit {
                    return (matches, true);
                }
                matches.push(symbol.id);
            }
        }
        (matches, false)
    }

    pub fn search_names_with_context(
        &self,
        needle: &str,
        limit: usize,
        operation: &OperationContext,
    ) -> Result<(Vec<EntityId>, bool), OperationAbort> {
        self.search_names_where_with_context(needle, limit, operation, |_, _| true)
    }

    pub fn search_names_where_with_context(
        &self,
        needle: &str,
        limit: usize,
        operation: &OperationContext,
        mut predicate: impl FnMut(&Symbol, &SourceMetadata) -> bool,
    ) -> Result<(Vec<EntityId>, bool), OperationAbort> {
        let needle = needle.to_lowercase();
        let mut matches = Vec::with_capacity(limit.min(self.symbol_count() as usize));
        for (index, symbol) in self.symbols().into_iter().enumerate() {
            if index % CANCELLATION_POLL_ITEMS == 0 {
                operation.check()?;
            }
            // The simple name is a suffix of the qualified name, so one
            // comparison covers both without a second lowercase allocation.
            let Some(metadata) = self.file_metadata(&symbol.key.path) else {
                continue;
            };
            if symbol.key.qualified_name.to_lowercase().contains(&needle)
                && predicate(symbol, metadata)
            {
                if matches.len() == limit {
                    return Ok((matches, true));
                }
                matches.push(symbol.id);
            }
        }
        Ok((matches, false))
    }

    /// Exact resolution by simple or qualified name (SPEC §24).
    pub fn resolve_name(&self, name: &str) -> Vec<EntityId> {
        self.resolve_name_candidates(name)
    }

    pub(crate) fn exact_name_candidates_iter<'a>(
        &'a self,
        name: &'a str,
    ) -> Box<dyn Iterator<Item = EntityId> + 'a> {
        if let Some(parts) = self.parts.as_ref() {
            return Box::new(
                parts
                    .iter()
                    .flat_map(move |part| part.exact_name_candidates_iter(name)),
            );
        }
        Box::new(
            self.symbols_by_exact_name
                .get(name)
                .into_iter()
                .flat_map(|ids| ids.iter().copied()),
        )
    }

    pub(crate) fn folded_name_candidates_iter<'a>(
        &'a self,
        folded_name: &'a str,
    ) -> Box<dyn Iterator<Item = EntityId> + 'a> {
        if let Some(parts) = self.parts.as_ref() {
            return Box::new(
                parts
                    .iter()
                    .flat_map(move |part| part.folded_name_candidates_iter(folded_name)),
            );
        }
        Box::new(
            self.symbols_by_folded_name
                .get(folded_name)
                .into_iter()
                .flat_map(|ids| ids.iter().copied()),
        )
    }

    pub(crate) fn resolve_name_candidates(&self, name: &str) -> Vec<EntityId> {
        let mut candidates: Vec<_> = self.exact_name_candidates_iter(name).collect();
        candidates.sort_unstable();
        candidates
    }

    pub fn resolve_name_with_context(
        &self,
        name: &str,
        operation: &OperationContext,
    ) -> Result<Vec<EntityId>, OperationAbort> {
        operation.check()?;
        let matches = self.resolve_name_candidates(name);
        operation.check()?;
        Ok(matches)
    }

    pub fn outgoing_edges(&self, id: EntityId) -> &[Edge] {
        if let Some(parts) = self.parts.as_ref() {
            return parts
                .iter()
                .find_map(|part| part.symbol(id).is_some().then(|| part.outgoing_edges(id)))
                .unwrap_or(&[]);
        }
        self.outgoing.get(&id).map_or(&[], |edges| edges.as_slice())
    }

    pub fn incoming_edges(&self, id: EntityId) -> &[Edge] {
        if let Some(parts) = self.parts.as_ref() {
            return parts
                .iter()
                .find_map(|part| part.symbol(id).is_some().then(|| part.incoming_edges(id)))
                .unwrap_or(&[]);
        }
        self.incoming.get(&id).map_or(&[], |edges| edges.as_slice())
    }

    /// Syntax call sites owned by one caller, in deterministic source-index
    /// insertion order.
    pub fn call_sites_from(&self, caller: EntityId) -> Box<dyn Iterator<Item = &CallSite> + '_> {
        if let Some(parts) = self.parts.as_ref() {
            return parts
                .iter()
                .find(|part| part.symbol(caller).is_some())
                .map_or_else(
                    || Box::new(std::iter::empty()) as Box<dyn Iterator<Item = &CallSite>>,
                    |part| part.call_sites_from(caller),
                );
        }
        Box::new(
            self.call_sites_by_caller
                .get(&caller)
                .into_iter()
                .flat_map(|indexes| indexes.iter())
                .filter_map(|index| self.call_sites.get(index).map(Arc::as_ref)),
        )
    }

    /// Removes compact call facts (and their derived `CALLS`/`TESTS` edges)
    /// for callers declared in `path` without touching any other file.
    pub fn remove_call_sites_in_file(
        &mut self,
        path: &RepoRelativePath,
    ) -> Result<u64, GraphError> {
        self.ensure_owned()?;
        let Some(file) = self.files.get(path).cloned() else {
            return Ok(0);
        };
        let mut removed = 0_u64;
        let mut removed_test_relations = HashSet::new();
        for caller in &file.symbols {
            let Some(indexes) = self.call_sites_by_caller.get(caller).cloned() else {
                continue;
            };
            for index in indexes.iter() {
                let Some(call_site) = self.call_sites.get(index).cloned() else {
                    continue;
                };
                if let CallResolution::Resolved { target } = call_site.resolution {
                    let (calls_provenance, calls_precision) =
                        call_relation_tier(call_site.provenance, call_site.precision);
                    self.remove_edge_raw(&Edge {
                        kind: EdgeKind::Calls,
                        from: call_site.caller,
                        to: target,
                        provenance: calls_provenance,
                        precision: calls_precision,
                        location: Some(call_site.location.clone()),
                    })?;
                    if self
                        .symbol(call_site.caller)
                        .is_some_and(|symbol| symbol.key.kind == SymbolKind::Test)
                        && removed_test_relations.insert((call_site.caller, target))
                    {
                        let (tests_provenance, tests_precision) =
                            test_relation_tier(call_site.provenance, call_site.precision);
                        self.remove_edge_raw(&Edge {
                            kind: EdgeKind::Tests,
                            from: call_site.caller,
                            to: target,
                            provenance: tests_provenance,
                            precision: tests_precision,
                            location: Some(call_site.location.clone()),
                        })?;
                    }
                }
                match call_site.resolution {
                    CallResolution::Ambiguous { .. } => {
                        self.ambiguous_call_sites = self.ambiguous_call_sites.saturating_sub(1);
                        if let Some(key) = call_site_lookup_key(
                            self.symbol(call_site.caller)
                                .map(|symbol| symbol.key.language),
                            call_site.form,
                            call_site.target_kind,
                            &call_site.name,
                            call_site.qualifier.as_deref(),
                        ) {
                            remove_index(&mut self.call_sites_by_lookup, &key, *index);
                        }
                    }
                    CallResolution::Unresolved => {
                        self.unresolved_call_sites = self.unresolved_call_sites.saturating_sub(1);
                    }
                    CallResolution::Resolved { .. } => {}
                }
                self.call_sites.remove_mut(index);
                removed = removed.saturating_add(1);
            }
            self.call_sites_by_caller.remove_mut(caller);
        }
        Ok(removed)
    }

    /// Syntax call site that materialized a `CALLS`/`TESTS` edge, when the
    /// relation originated from the lazy syntax call-site arena.
    pub fn call_site_for_edge(&self, edge: &Edge) -> Option<&CallSite> {
        if !matches!(edge.kind, EdgeKind::Calls | EdgeKind::Tests) {
            return None;
        }
        self.call_sites_from(edge.from).find(|call_site| {
            matches!(
                call_site.resolution,
                CallResolution::Resolved { target } if target == edge.to
            ) && edge.location.as_ref() == Some(&call_site.location)
        })
    }

    /// Bounded candidate declarations for one ambiguous call site.
    pub fn call_candidates<'a>(
        &'a self,
        call_site: &CallSite,
        limit: usize,
    ) -> (Vec<&'a Symbol>, bool) {
        if let Some(parts) = self.parts.as_ref() {
            return parts
                .iter()
                .find(|part| part.symbol(call_site.caller).is_some())
                .map_or((Vec::new(), false), |part| {
                    part.call_candidates(call_site, limit)
                });
        }
        if !matches!(call_site.resolution, CallResolution::Ambiguous { .. }) {
            return (Vec::new(), false);
        }
        let Some(key) = call_site_lookup_key(
            self.symbol(call_site.caller)
                .map(|symbol| symbol.key.language),
            call_site.form,
            call_site.target_kind,
            &call_site.name,
            call_site.qualifier.as_deref(),
        ) else {
            return (Vec::new(), false);
        };
        let Some(ids) = self.callables.get(&key) else {
            return (Vec::new(), false);
        };
        let truncated = ids.len() > limit;
        let candidates = ids
            .iter()
            .take(limit)
            .filter_map(|id| self.symbol(*id))
            .collect();
        (candidates, truncated)
    }

    /// Bounded ambiguous call sites for which `target` is one candidate.
    pub fn call_sites_for_target(&self, target: EntityId, limit: usize) -> (Vec<&CallSite>, bool) {
        if let Some(parts) = self.parts.as_ref() {
            return parts
                .iter()
                .find(|part| part.symbol(target).is_some())
                .map_or((Vec::new(), false), |part| {
                    part.call_sites_for_target(target, limit)
                });
        }
        let Some(symbol) = self.symbol(target) else {
            return (Vec::new(), false);
        };
        let mut indexes = Vec::with_capacity(limit.saturating_add(1));
        let mut seen = HashSet::with_capacity(limit.saturating_add(1));
        'keys: for key in callable_lookup_keys(symbol) {
            let Some(call_sites) = self.call_sites_by_lookup.get(&key) else {
                continue;
            };
            for index in call_sites.iter() {
                if seen.insert(*index) {
                    indexes.push(*index);
                    if indexes.len() > limit {
                        break 'keys;
                    }
                }
            }
        }
        indexes.sort_unstable();
        let truncated = indexes.len() > limit;
        indexes.truncate(limit);
        let call_sites = indexes
            .into_iter()
            .filter_map(|index| self.call_sites.get(&index).map(Arc::as_ref))
            .collect();
        (call_sites, truncated)
    }

    fn validate_call_site_input(&self, input: &CallSiteInput) -> Result<(), GraphError> {
        if input.name.trim().is_empty() {
            return Err(GraphError::EmptyCallSiteName);
        }
        if input
            .receiver_hint
            .as_ref()
            .is_some_and(|hint| hint.chars().count() > MAX_RECEIVER_HINT_CHARS)
        {
            return Err(GraphError::ReceiverHintTooLong {
                limit: MAX_RECEIVER_HINT_CHARS,
            });
        }
        if input.receiver_type.is_some() != input.receiver_type_source.is_some() {
            return Err(GraphError::ReceiverTypeEvidenceMismatch);
        }
        let caller = self
            .symbol(input.caller)
            .ok_or(GraphError::UnknownEntity(input.caller))?;
        if caller.location.file() != input.location.file() {
            return Err(GraphError::CallSiteLocationMismatch {
                caller: input.caller,
                site_path: input.location.file().clone(),
                caller_path: caller.location.file().clone(),
            });
        }
        Ok(())
    }

    fn insert_call_site(&mut self, call_site: CallSite) -> Result<(), GraphError> {
        let input = CallSiteInput {
            caller: call_site.caller,
            form: call_site.form,
            target_kind: call_site.target_kind,
            name: call_site.name.clone(),
            qualifier: call_site.qualifier.clone(),
            receiver_type: call_site.receiver_type.clone(),
            receiver_type_source: call_site.receiver_type_source,
            receiver_hint: call_site.receiver_hint.clone(),
            location: call_site.location.clone(),
            provenance: call_site.provenance,
            precision: call_site.precision,
        };
        self.validate_call_site_input(&input)?;
        if let CallResolution::Resolved { target } = call_site.resolution {
            self.symbol(target)
                .ok_or(GraphError::UnknownEntity(target))?;
        }
        let index = self.next_call_site_id;
        self.next_call_site_id = self.next_call_site_id.saturating_add(1);
        let mut caller_sites = self
            .call_sites_by_caller
            .get(&call_site.caller)
            .map_or_else(Vec::new, |indexes| indexes.as_ref().clone());
        caller_sites.push(index);
        self.call_sites_by_caller
            .insert_mut(call_site.caller, Arc::new(caller_sites));
        match call_site.resolution {
            CallResolution::Ambiguous { .. } => {
                self.ambiguous_call_sites += 1;
                if let Some(key) = call_site_lookup_key(
                    self.symbol(call_site.caller)
                        .map(|symbol| symbol.key.language),
                    call_site.form,
                    call_site.target_kind,
                    &call_site.name,
                    call_site.qualifier.as_deref(),
                ) {
                    let mut lookup_sites = self
                        .call_sites_by_lookup
                        .get(&key)
                        .map_or_else(Vec::new, |indexes| indexes.as_ref().clone());
                    lookup_sites.push(index);
                    self.call_sites_by_lookup
                        .insert_mut(key, Arc::new(lookup_sites));
                }
            }
            CallResolution::Unresolved => self.unresolved_call_sites += 1,
            CallResolution::Resolved { .. } => {}
        }
        self.call_sites.insert_mut(index, Arc::new(call_site));
        Ok(())
    }

    fn resolve_call(
        &self,
        language: Language,
        form: CallForm,
        target_kind: CallTargetKind,
        name: &str,
        qualifier: Option<&str>,
    ) -> CallResolution {
        let Some(key) = call_site_lookup_key(Some(language), form, target_kind, name, qualifier)
        else {
            return CallResolution::Unresolved;
        };
        match self
            .callables
            .get(&key)
            .map(|ids| ids.as_slice())
            .unwrap_or(&[])
        {
            [] => CallResolution::Unresolved,
            [target] => CallResolution::Resolved { target: *target },
            candidates => CallResolution::Ambiguous {
                candidates: candidates.len() as u64,
            },
        }
    }

    /// Runs an independent, expected-linear consistency audit.
    ///
    /// Every derived structure is recomputed from the arena and compared, so
    /// a hybrid snapshot is caught. Edge mirrors are compared as exact
    /// multisets: identical parallel edges therefore cannot hide duplicate or
    /// missing adjacency entries.
    pub fn audit_consistency(&self) -> Result<ConsistencyAudit, ConsistencyError> {
        let started = Instant::now();
        if let Some(parts) = self.parts.as_ref() {
            let mut combined = ConsistencyAudit {
                symbols_audited: 0,
                files_audited: 0,
                edges_audited: 0,
                adjacency_entries_examined: 0,
                elapsed: Duration::ZERO,
            };
            for part in parts.iter() {
                let audit = part.audit_consistency()?;
                combined.symbols_audited = combined
                    .symbols_audited
                    .saturating_add(audit.symbols_audited);
                combined.files_audited = combined.files_audited.saturating_add(audit.files_audited);
                combined.edges_audited = combined.edges_audited.saturating_add(audit.edges_audited);
                combined.adjacency_entries_examined = combined
                    .adjacency_entries_examined
                    .saturating_add(audit.adjacency_entries_examined);
            }
            combined.elapsed = started.elapsed();
            return Ok(combined);
        }

        // Persistent arena keys and the revision-local ids in their payloads
        // must agree. IDs are deliberately sparse after incremental edits.
        for (id, symbol) in self.symbols.iter() {
            if symbol.id != *id {
                return Err(ConsistencyError::IdKeyMismatch {
                    id: symbol.id,
                    key: *id,
                });
            }
        }

        // The file index covers exactly the arena symbols.
        let mut expected_by_file: HashMap<&RepoRelativePath, Vec<EntityId>> =
            self.files.keys().map(|path| (path, Vec::new())).collect();
        for (_, symbol) in self.symbols.iter() {
            expected_by_file
                .entry(symbol.location.file())
                .or_default()
                .push(symbol.id);
        }
        let file_index_matches = expected_by_file.len() == self.files.size()
            && expected_by_file.iter().all(|(path, expected)| {
                self.files
                    .get(*path)
                    .is_some_and(|actual| actual.symbols == *expected)
            });
        if !file_index_matches {
            return Err(ConsistencyError::FileIndexMismatch);
        }
        for (path, file) in &self.files {
            if file.diagnostic_count < file.diagnostics.len() as u64 {
                return Err(ConsistencyError::DiagnosticCountUnderflow {
                    path: path.clone(),
                    total: file.diagnostic_count,
                    retained: file.diagnostics.len(),
                });
            }
            if let Some(diagnostic) = file
                .diagnostics
                .iter()
                .find(|diagnostic| diagnostic.range.file() != path)
            {
                return Err(ConsistencyError::DiagnosticPathMismatch {
                    file_path: path.clone(),
                    diagnostic_path: diagnostic.range.file().clone(),
                });
            }
            if let Some(diagnostic) = file.diagnostics.iter().find(|diagnostic| {
                diagnostic.provenance != Provenance::TreeSitter
                    || diagnostic.precision != Precision::Syntax
            }) {
                return Err(ConsistencyError::InvalidDiagnosticQuality {
                    path: path.clone(),
                    provenance: diagnostic.provenance,
                    precision: diagnostic.precision,
                });
            }
        }

        let mut expected_by_name: HashMap<String, Vec<EntityId>> = HashMap::new();
        for (_, symbol) in self.symbols.iter() {
            let simple_name = symbol.name().to_owned();
            expected_by_name
                .entry(simple_name.clone())
                .or_default()
                .push(symbol.id);
            if symbol.key.qualified_name != simple_name {
                expected_by_name
                    .entry(symbol.key.qualified_name.clone())
                    .or_default()
                    .push(symbol.id);
            }
        }
        let actual_by_name: HashMap<_, _> = self
            .symbols_by_exact_name
            .iter()
            .map(|(name, ids)| (name.clone(), ids.as_ref().clone()))
            .collect();
        if expected_by_name != actual_by_name {
            return Err(ConsistencyError::ExactNameIndexMismatch);
        }
        let mut expected_by_folded_name: HashMap<String, Vec<EntityId>> = HashMap::new();
        for (_, symbol) in self.symbols.iter() {
            let simple_name = symbol.name().to_lowercase();
            let qualified_name = symbol.key.qualified_name.to_lowercase();
            expected_by_folded_name
                .entry(simple_name.clone())
                .or_default()
                .push(symbol.id);
            if qualified_name != simple_name {
                expected_by_folded_name
                    .entry(qualified_name)
                    .or_default()
                    .push(symbol.id);
            }
        }
        let actual_by_folded_name: HashMap<_, _> = self
            .symbols_by_folded_name
            .iter()
            .map(|(name, ids)| (name.clone(), ids.as_ref().clone()))
            .collect();
        if expected_by_folded_name != actual_by_folded_name {
            return Err(ConsistencyError::FoldedNameIndexMismatch);
        }

        // Count each outgoing edge once. The mirror count is consumed by the
        // incoming index below; the call-site count is consumed by resolved
        // call sites. Sharing this table keeps the complete audit linear even
        // when one caller owns a high-degree adjacency list.
        let mut outgoing_total = 0_u64;
        let mut edge_counts: HashMap<&Edge, (u64, u64)> = HashMap::new();
        for (key, edges) in self.outgoing.iter() {
            for edge in edges.iter() {
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
                let counts = edge_counts.entry(edge).or_default();
                counts.0 += 1;
                counts.1 += 1;
            }
        }

        // The compact call-site index is derived from the same symbol arena
        // and must resolve against the exact callable catalog of this
        // revision. Ambiguous sites are indexed by lookup key without
        // materializing one edge per candidate.
        let mut expected_callables: HashMap<CallLookupKey, Vec<EntityId>> = HashMap::new();
        for (_, symbol) in self.symbols.iter() {
            for key in callable_lookup_keys(symbol) {
                expected_callables.entry(key).or_default().push(symbol.id);
            }
        }
        let actual_callables: HashMap<_, _> = self
            .callables
            .iter()
            .map(|(key, ids)| (key.clone(), ids.as_ref().clone()))
            .collect();
        if expected_callables != actual_callables {
            return Err(ConsistencyError::CallableIndexMismatch);
        }
        let mut expected_by_caller: HashMap<EntityId, Vec<u64>> = HashMap::new();
        let mut expected_by_lookup: HashMap<CallLookupKey, Vec<u64>> = HashMap::new();
        let mut expected_test_relations = HashSet::new();
        let mut ambiguous = 0_u64;
        let mut unresolved = 0_u64;
        for (index, call_site) in self.call_sites.iter() {
            let caller = self
                .symbol(call_site.caller)
                .ok_or(ConsistencyError::UnknownEntity(call_site.caller))?;
            if caller.location.file() != call_site.location.file() {
                return Err(ConsistencyError::CallSiteLocationMismatch {
                    index: *index as usize,
                });
            }
            let expected_resolution = self.resolve_call(
                caller.key.language,
                call_site.form,
                call_site.target_kind,
                &call_site.name,
                call_site.qualifier.as_deref(),
            );
            if expected_resolution != call_site.resolution {
                return Err(ConsistencyError::CallSiteResolutionMismatch {
                    index: *index as usize,
                });
            }
            expected_by_caller
                .entry(call_site.caller)
                .or_default()
                .push(*index);
            match call_site.resolution {
                CallResolution::Resolved { target } => {
                    self.symbol(target)
                        .ok_or(ConsistencyError::UnknownEntity(target))?;
                    let (calls_provenance, calls_precision) =
                        call_relation_tier(call_site.provenance, call_site.precision);
                    let expected_call = Edge {
                        kind: EdgeKind::Calls,
                        from: call_site.caller,
                        to: target,
                        provenance: calls_provenance,
                        precision: calls_precision,
                        location: Some(call_site.location.clone()),
                    };
                    let Some((_, available_calls)) = edge_counts.get_mut(&expected_call) else {
                        return Err(ConsistencyError::ResolvedCallEdgeMissing {
                            index: *index as usize,
                        });
                    };
                    if *available_calls == 0 {
                        return Err(ConsistencyError::ResolvedCallEdgeMissing {
                            index: *index as usize,
                        });
                    }
                    *available_calls -= 1;
                    if caller.key.kind == SymbolKind::Test
                        && expected_test_relations.insert((call_site.caller, target))
                    {
                        let (tests_provenance, tests_precision) =
                            test_relation_tier(call_site.provenance, call_site.precision);
                        let expected_test = Edge {
                            kind: EdgeKind::Tests,
                            from: call_site.caller,
                            to: target,
                            provenance: tests_provenance,
                            precision: tests_precision,
                            location: Some(call_site.location.clone()),
                        };
                        let Some((_, available_tests)) = edge_counts.get_mut(&expected_test) else {
                            return Err(ConsistencyError::ResolvedTestEdgeMissing {
                                index: *index as usize,
                            });
                        };
                        if *available_tests == 0 {
                            return Err(ConsistencyError::ResolvedTestEdgeMissing {
                                index: *index as usize,
                            });
                        }
                        *available_tests -= 1;
                    }
                }
                CallResolution::Ambiguous { .. } => {
                    ambiguous += 1;
                    let key = call_site_lookup_key(
                        Some(caller.key.language),
                        call_site.form,
                        call_site.target_kind,
                        &call_site.name,
                        call_site.qualifier.as_deref(),
                    )
                    .ok_or(ConsistencyError::CallSiteResolutionMismatch {
                        index: *index as usize,
                    })?;
                    expected_by_lookup.entry(key).or_default().push(*index);
                }
                CallResolution::Unresolved => unresolved += 1,
            }
        }
        let actual_by_caller: HashMap<_, _> = self
            .call_sites_by_caller
            .iter()
            .map(|(caller, indexes)| (*caller, indexes.as_ref().clone()))
            .collect();
        let actual_by_lookup: HashMap<_, _> = self
            .call_sites_by_lookup
            .iter()
            .map(|(key, indexes)| (key.clone(), indexes.as_ref().clone()))
            .collect();
        if expected_by_caller != actual_by_caller || expected_by_lookup != actual_by_lookup {
            return Err(ConsistencyError::CallSiteIndexMismatch);
        }
        if ambiguous != self.ambiguous_call_sites || unresolved != self.unresolved_call_sites {
            return Err(ConsistencyError::CallSiteCountMismatch {
                ambiguous,
                recorded_ambiguous: self.ambiguous_call_sites,
                unresolved,
                recorded_unresolved: self.unresolved_call_sites,
            });
        }

        let mut owned_edge_counts: HashMap<&Edge, u64> = HashMap::new();
        for (_, edges) in self.relationship_edges_by_owner.iter() {
            for edge in edges.iter() {
                *owned_edge_counts.entry(edge).or_default() += 1;
            }
        }
        if edge_counts.iter().any(|(edge, (_, unclaimed))| {
            owned_edge_counts.get(edge).copied().unwrap_or(0) != *unclaimed
        }) || owned_edge_counts
            .iter()
            .any(|(edge, count)| edge_counts.get(edge).map(|counts| counts.1) != Some(*count))
        {
            return Err(ConsistencyError::RelationshipOwnershipMismatch);
        }

        // Edges are stored under the correct key, endpoints exist, and both
        // adjacency indexes mirror the exact same multiset. Counting outgoing
        // edges and consuming those counts from incoming is expected O(E),
        // including for high-degree nodes and identical parallel edges.
        let mut incoming_total = 0_u64;
        for (key, edges) in self.incoming.iter() {
            for edge in edges.iter() {
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
                let Some((unmatched_mirrors, _)) = edge_counts.get_mut(edge) else {
                    return Err(ConsistencyError::EdgeIncomingMirrorMissing {
                        from: edge.from,
                        to: edge.to,
                    });
                };
                if *unmatched_mirrors == 0 {
                    return Err(ConsistencyError::EdgeIncomingMirrorMissing {
                        from: edge.from,
                        to: edge.to,
                    });
                }
                *unmatched_mirrors -= 1;
            }
        }
        if let Some((edge, _)) = edge_counts
            .into_iter()
            .find(|(_, (unmatched_mirrors, _))| *unmatched_mirrors != 0)
        {
            return Err(ConsistencyError::EdgeMirrorMissing {
                from: edge.from,
                to: edge.to,
            });
        }
        if outgoing_total != self.edge_count || incoming_total != self.edge_count {
            return Err(ConsistencyError::EdgeCountMismatch {
                recorded: self.edge_count,
                outgoing: outgoing_total,
                incoming: incoming_total,
            });
        }
        Ok(ConsistencyAudit {
            symbols_audited: self.symbol_count(),
            files_audited: self.file_count(),
            edges_audited: outgoing_total,
            adjacency_entries_examined: outgoing_total.saturating_add(incoming_total),
            elapsed: started.elapsed(),
        })
    }

    /// Compatibility wrapper for callers that only need pass/fail status.
    pub fn validate_consistency(&self) -> Result<(), ConsistencyError> {
        self.audit_consistency().map(|_| ())
    }
}

fn call_target_kind(kind: SymbolKind) -> Option<CallTargetKind> {
    match kind {
        SymbolKind::Function => Some(CallTargetKind::Function),
        SymbolKind::Method => Some(CallTargetKind::Method),
        SymbolKind::Test => Some(CallTargetKind::Test),
        _ => None,
    }
}

fn remove_adjacency_edge(
    adjacency: &mut HashTrieMapSync<EntityId, Arc<Vec<Edge>>>,
    key: EntityId,
    edge: &Edge,
) -> Result<u64, GraphError> {
    let Some(existing) = adjacency.get(&key) else {
        return Err(GraphError::MissingOwnedEdge);
    };
    let copied = existing.len() as u64;
    let mut edges = existing.as_ref().clone();
    let Some(index) = edges.iter().position(|candidate| candidate == edge) else {
        return Err(GraphError::MissingOwnedEdge);
    };
    edges.remove(index);
    if edges.is_empty() {
        adjacency.remove_mut(&key);
    } else {
        adjacency.insert_mut(key, Arc::new(edges));
    }
    Ok(copied)
}

fn remove_index(
    index: &mut HashTrieMapSync<CallLookupKey, Arc<Vec<u64>>>,
    key: &CallLookupKey,
    value: u64,
) {
    let Some(existing) = index.get(key) else {
        return;
    };
    let mut values = existing.as_ref().clone();
    values.retain(|candidate| *candidate != value);
    if values.is_empty() {
        index.remove_mut(key);
    } else {
        index.insert_mut(key.clone(), Arc::new(values));
    }
}

fn callable_lookup_keys(symbol: &Symbol) -> Vec<CallLookupKey> {
    let Some(target_kind) = call_target_kind(symbol.key.kind) else {
        return Vec::new();
    };
    let name = symbol.name().to_owned();
    let mut qualifiers = vec![None];
    if let Some(container) = symbol.key.container.as_ref() {
        qualifiers.push(Some(container.clone()));
    }
    if let Some((container, _)) = symbol.key.qualified_name.rsplit_once("::") {
        let qualified = Some(container.to_owned());
        if !qualifiers.contains(&qualified) {
            qualifiers.push(qualified);
        }
        let simple = Some(
            container
                .rsplit("::")
                .next()
                .unwrap_or(container)
                .to_owned(),
        );
        if !qualifiers.contains(&simple) {
            qualifiers.push(simple);
        }
    }
    qualifiers
        .into_iter()
        .map(|qualifier| CallLookupKey {
            language: symbol.key.language,
            target_kind,
            name: name.clone(),
            qualifier,
        })
        .collect()
}

fn call_site_lookup_key(
    language: Option<Language>,
    form: CallForm,
    target_kind: CallTargetKind,
    name: &str,
    qualifier: Option<&str>,
) -> Option<CallLookupKey> {
    let language = language?;
    if qualifier.is_none()
        && matches!(
            form,
            CallForm::Member | CallForm::NullsafeMember | CallForm::Scoped
        )
    {
        return None;
    }
    Some(CallLookupKey {
        language,
        target_kind,
        name: name.to_owned(),
        qualifier: qualifier.map(str::to_owned),
    })
}

fn validate_diagnostics(
    path: &RepoRelativePath,
    diagnostics: &[SyntaxDiagnostic],
    diagnostic_count: u64,
) -> Result<(), GraphError> {
    if diagnostic_count < diagnostics.len() as u64 {
        return Err(GraphError::DiagnosticCountUnderflow {
            total: diagnostic_count,
            retained: diagnostics.len(),
        });
    }
    if let Some(diagnostic) = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.range.file() != path)
    {
        return Err(GraphError::DiagnosticPathMismatch {
            file_path: path.clone(),
            diagnostic_path: diagnostic.range.file().clone(),
        });
    }
    if let Some(diagnostic) = diagnostics.iter().find(|diagnostic| {
        diagnostic.provenance != Provenance::TreeSitter || diagnostic.precision != Precision::Syntax
    }) {
        return Err(GraphError::InvalidDiagnosticQuality {
            provenance: diagnostic.provenance,
            precision: diagnostic.precision,
        });
    }
    Ok(())
}

fn diagnostic_cmp(left: &SyntaxDiagnostic, right: &SyntaxDiagnostic) -> Ordering {
    left.range
        .file()
        .cmp(right.range.file())
        .then(left.range.start().cmp(&right.range.start()))
        .then(left.range.end().cmp(&right.range.end()))
        .then(left.language.cmp(&right.language))
        .then(left.kind.cmp(&right.kind))
        .then(left.precision.cmp(&right.precision))
        .then(provenance_rank(left.provenance).cmp(&provenance_rank(right.provenance)))
        .then(left.cause.cmp(&right.cause))
        .then(left.node_kind.cmp(&right.node_kind))
}

fn provenance_rank(provenance: Provenance) -> u8 {
    match provenance {
        Provenance::RustAnalyzer => 0,
        Provenance::Vtsls => 0,
        Provenance::Pyright => 0,
        Provenance::Jdtls => 0,
        Provenance::CsharpLs => 0,
        Provenance::BashLanguageServer => 0,
        Provenance::Clangd => 0,
        Provenance::ChakraResolver => 0,
        Provenance::TreeSitter => 1,
        Provenance::Git => 2,
        Provenance::TextSearch => 3,
        Provenance::Heuristic => 4,
    }
}

/// `(provenance, precision)` of the `CALLS` relation materialized from one
/// resolved syntax call site.
///
/// Resolved syntax calls stay heuristic by default (ADR-010, ADR-015) while
/// keeping the call site's provenance. A language indexer promotes a single
/// call site to the precise tier only through an explicit strict-tier
/// evidence rule (ADR-0030); the relation then carries the call site's own
/// provenance so the promotion stays attributable. Precision is never
/// upgraded silently (PROV-01).
fn call_relation_tier(provenance: Provenance, precision: Precision) -> (Provenance, Precision) {
    if precision == Precision::Precise {
        (provenance, Precision::Precise)
    } else {
        (provenance, Precision::Heuristic)
    }
}

/// `(provenance, precision)` of the deduplicated `TESTS` relation
/// materialized from one resolved syntax call site. Unlike `CALLS`, the
/// default tier records the relation itself as heuristic; a strict-tier
/// call site (ADR-0030) promotes it exactly like [`call_relation_tier`].
fn test_relation_tier(provenance: Provenance, precision: Precision) -> (Provenance, Precision) {
    if precision == Precision::Precise {
        (provenance, Precision::Precise)
    } else {
        (Provenance::Heuristic, Precision::Heuristic)
    }
}

/// A broken internal graph invariant found by [`SymbolGraph::audit_consistency`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConsistencyError {
    #[error("edge endpoint {0:?} does not exist in the arena")]
    UnknownEntity(EntityId),
    #[error("symbol payload id {id:?} is stored under persistent-map key {key:?}")]
    IdKeyMismatch { id: EntityId, key: EntityId },
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
    #[error("exact-name index does not match the symbol arena")]
    ExactNameIndexMismatch,
    #[error("case-folded name index does not match the symbol arena")]
    FoldedNameIndexMismatch,
    #[error("callable lookup index does not match the symbol arena")]
    CallableIndexMismatch,
    #[error("call site at index {index} is not in its caller's file")]
    CallSiteLocationMismatch { index: usize },
    #[error("call site at index {index} has a stale or incorrect resolution")]
    CallSiteResolutionMismatch { index: usize },
    #[error("resolved call site at index {index} has no corresponding CALLS edge")]
    ResolvedCallEdgeMissing { index: usize },
    #[error("resolved test call site at index {index} has no corresponding TESTS edge")]
    ResolvedTestEdgeMissing { index: usize },
    #[error("call-site lookup indexes do not match the call-site arena")]
    CallSiteIndexMismatch,
    #[error(
        "call-site counts do not match: ambiguous {recorded_ambiguous} recorded/{ambiguous} actual, unresolved {recorded_unresolved} recorded/{unresolved} actual"
    )]
    CallSiteCountMismatch {
        ambiguous: u64,
        recorded_ambiguous: u64,
        unresolved: u64,
        recorded_unresolved: u64,
    },
    #[error("file-owned relationship contributions do not match non-call graph edges")]
    RelationshipOwnershipMismatch,
    #[error(
        "file {path} records {total} diagnostics but retains {retained}, which exceeds that total"
    )]
    DiagnosticCountUnderflow {
        path: RepoRelativePath,
        total: u64,
        retained: usize,
    },
    #[error("diagnostic range file `{diagnostic_path}` does not match indexed file `{file_path}`")]
    DiagnosticPathMismatch {
        file_path: RepoRelativePath,
        diagnostic_path: RepoRelativePath,
    },
    #[error(
        "file {path} has a syntax diagnostic with invalid quality {provenance:?}/{precision:?}"
    )]
    InvalidDiagnosticQuality {
        path: RepoRelativePath,
        provenance: Provenance,
        precision: Precision,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use chakra_domain::diagnostic::{SyntaxDiagnosticCause, SyntaxDiagnosticKind};
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

    fn diagnostic(path: RepoRelativePath) -> Result<SyntaxDiagnostic, Box<dyn std::error::Error>> {
        Ok(SyntaxDiagnostic {
            language: Language::Rust,
            range: range(path)?,
            kind: SyntaxDiagnosticKind::Error,
            provenance: Provenance::TreeSitter,
            precision: Precision::Syntax,
            cause: SyntaxDiagnosticCause::ParseRecovery,
            node_kind: "ERROR".to_owned(),
        })
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

    fn add_callable(
        graph: &mut SymbolGraph,
        qualified_name: &str,
        container: Option<&str>,
        kind: SymbolKind,
        path: &str,
    ) -> Result<EntityId, Box<dyn std::error::Error>> {
        let path = file(path)?;
        Ok(graph.add_symbol(
            SymbolKey {
                language: Language::Rust,
                qualified_name: qualified_name.to_owned(),
                container: container.map(str::to_owned),
                kind,
                path: path.clone(),
            },
            range(path)?,
            None,
            Provenance::TreeSitter,
            Precision::Syntax,
        )?)
    }

    fn call_site_input(
        caller: EntityId,
        form: CallForm,
        target_kind: CallTargetKind,
        name: &str,
        qualifier: Option<&str>,
        receiver_hint: Option<&str>,
        path: &str,
    ) -> Result<CallSiteInput, Box<dyn std::error::Error>> {
        Ok(CallSiteInput {
            caller,
            form,
            target_kind,
            name: name.to_owned(),
            qualifier: qualifier.map(str::to_owned),
            receiver_type: None,
            receiver_type_source: None,
            receiver_hint: receiver_hint.map(str::to_owned),
            location: range(file(path)?)?,
            provenance: Provenance::TreeSitter,
            precision: Precision::Syntax,
        })
    }

    fn high_degree_graph(edge_count: usize) -> Result<SymbolGraph, Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let target = add_fn(&mut graph, "target", "src/high_degree.rs")?;
        for index in 0..edge_count {
            let caller = add_fn(&mut graph, &format!("caller_{index}"), "src/high_degree.rs")?;
            graph.add_edge(
                EdgeKind::Calls,
                caller,
                target,
                Provenance::TreeSitter,
                Precision::Syntax,
                None,
            )?;
        }
        Ok(graph)
    }

    fn test_push_edge(
        adjacency: &mut HashTrieMapSync<EntityId, Arc<Vec<Edge>>>,
        key: EntityId,
        edge: Edge,
    ) {
        let mut edges = adjacency
            .get(&key)
            .map_or_else(Vec::new, |edges| edges.as_ref().clone());
        edges.push(edge);
        adjacency.insert_mut(key, Arc::new(edges));
    }

    fn test_pop_edge(
        adjacency: &mut HashTrieMapSync<EntityId, Arc<Vec<Edge>>>,
        key: EntityId,
    ) -> Option<Edge> {
        let mut edges = adjacency.get(&key)?.as_ref().clone();
        let edge = edges.pop()?;
        adjacency.insert_mut(key, Arc::new(edges));
        Some(edge)
    }

    fn measure_high_degree_audit(
        edge_count: usize,
    ) -> Result<ConsistencyAudit, Box<dyn std::error::Error>> {
        let graph = high_degree_graph(edge_count)?;
        Ok(graph.audit_consistency()?)
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
        let audit = graph.audit_consistency()?;
        assert_eq!(graph.symbol_count(), 2);
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(graph.file_count(), 2);
        assert_eq!(audit.symbols_audited, 2);
        assert_eq!(audit.files_audited, 2);
        assert_eq!(audit.edges_audited, 1);
        assert_eq!(audit.adjacency_entries_examined, 2);
        Ok(())
    }

    #[test]
    fn consistency_validation_scaling_is_directly_measurable()
    -> Result<(), Box<dyn std::error::Error>> {
        let smaller_edges = 2_000;
        let larger_edges = 4_000;
        let smaller = measure_high_degree_audit(smaller_edges)?;
        let larger = measure_high_degree_audit(larger_edges)?;
        assert_eq!(smaller.edges_audited, smaller_edges as u64);
        assert_eq!(smaller.adjacency_entries_examined, 4_000);
        assert_eq!(larger.edges_audited, larger_edges as u64);
        assert_eq!(larger.adjacency_entries_examined, 8_000);
        eprintln!(
            "graph_consistency_high_degree: smaller_edges={smaller_edges}, smaller={:?}, larger_edges={larger_edges}, larger={:?}",
            smaller.elapsed, larger.elapsed
        );
        Ok(())
    }

    #[test]
    fn call_sites_separate_domains_and_materialize_only_unique_targets()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let caller = add_fn(&mut graph, "caller", "src/caller.rs")?;
        let test_caller = add_callable(
            &mut graph,
            "save_buffer_works",
            None,
            SymbolKind::Test,
            "tests/save_buffer.rs",
        )?;
        let _same_name_test = add_callable(
            &mut graph,
            "save_buffer",
            None,
            SymbolKind::Test,
            "tests/same_name.rs",
        )?;
        let free_function = add_fn(&mut graph, "save_buffer", "src/free.rs")?;
        let project = add_callable(
            &mut graph,
            "Project::save_buffer",
            Some("Project"),
            SymbolKind::Method,
            "src/project.rs",
        )?;
        let buffer_store = add_callable(
            &mut graph,
            "BufferStore::save_buffer",
            Some("BufferStore"),
            SymbolKind::Method,
            "src/buffer_store.rs",
        )?;
        add_fn(&mut graph, "a::helper", "src/a.rs")?;
        add_fn(&mut graph, "b::helper", "src/b.rs")?;

        assert_eq!(
            graph.add_call_site(call_site_input(
                caller,
                CallForm::Function,
                CallTargetKind::Function,
                "save_buffer",
                None,
                None,
                "src/caller.rs",
            )?)?,
            CallResolution::Resolved {
                target: free_function
            }
        );
        assert_eq!(
            graph.add_call_site(call_site_input(
                caller,
                CallForm::Member,
                CallTargetKind::Method,
                "save_buffer",
                None,
                Some("store"),
                "src/caller.rs",
            )?)?,
            CallResolution::Unresolved
        );
        assert_eq!(
            graph.add_call_site(call_site_input(
                caller,
                CallForm::Member,
                CallTargetKind::Method,
                "save_buffer",
                Some("Project"),
                Some("self"),
                "src/caller.rs",
            )?)?,
            CallResolution::Resolved { target: project }
        );
        assert_eq!(
            graph.add_call_site(call_site_input(
                caller,
                CallForm::Function,
                CallTargetKind::Function,
                "helper",
                None,
                None,
                "src/caller.rs",
            )?)?,
            CallResolution::Ambiguous { candidates: 2 }
        );
        assert_eq!(
            graph.add_call_site(call_site_input(
                test_caller,
                CallForm::Function,
                CallTargetKind::Function,
                "save_buffer",
                None,
                None,
                "tests/save_buffer.rs",
            )?)?,
            CallResolution::Resolved {
                target: free_function
            }
        );

        assert_eq!(graph.call_site_count(), 5);
        assert_eq!(graph.ambiguous_call_site_count(), 1);
        assert_eq!(graph.unresolved_call_site_count(), 1);
        let call_targets: Vec<_> = graph
            .outgoing_edges(caller)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .map(|edge| edge.to)
            .collect();
        assert_eq!(call_targets, [free_function, project]);
        assert!(graph.call_sites_for_target(buffer_store, 10).0.is_empty());
        let ambiguous = graph
            .call_sites_from(caller)
            .find(|call_site| call_site.name == "helper")
            .ok_or("ambiguous call site missing")?;
        assert_eq!(graph.call_candidates(ambiguous, 1).0.len(), 1);
        assert!(graph.call_candidates(ambiguous, 1).1);
        assert_eq!(
            graph
                .outgoing_edges(test_caller)
                .iter()
                .filter(|edge| edge.kind == EdgeKind::Calls)
                .count(),
            1
        );
        assert_eq!(
            graph
                .outgoing_edges(test_caller)
                .iter()
                .filter(|edge| edge.kind == EdgeKind::Tests)
                .count(),
            1
        );
        graph.validate_consistency()?;
        Ok(())
    }

    #[test]
    fn call_site_budget_reports_the_resolved_edges_it_prevents()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = BoundedGraphBuilder::new(GraphBuildLimits {
            max_symbols: 2,
            max_edges: 2,
            max_call_sites: 1,
        });
        let path = file("src/caller.rs")?;
        let caller = builder
            .add_symbol(
                key("caller", path.clone()),
                range(path.clone())?,
                None,
                Provenance::TreeSitter,
                Precision::Syntax,
            )?
            .ok_or("caller must fit")?;
        builder
            .add_symbol(
                key("target", path),
                range(file("src/caller.rs")?)?,
                None,
                Provenance::TreeSitter,
                Precision::Syntax,
            )?
            .ok_or("target must fit")?;
        let input = call_site_input(
            caller,
            CallForm::Function,
            CallTargetKind::Function,
            "target",
            None,
            None,
            "src/caller.rs",
        )?;

        assert!(builder.add_call_site(input.clone())?);
        assert!(!builder.add_call_site(input)?);
        let (graph, report) = builder.finish();

        assert_eq!(graph.call_site_count(), 1);
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(report.omitted_call_sites, 1);
        assert_eq!(report.omitted_edges, 1);
        assert_eq!(report.edges_omitted_by_call_site_budget, 1);
        graph.validate_consistency()?;
        Ok(())
    }

    #[test]
    fn repeated_calls_from_one_test_create_one_test_relation()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let test_caller = add_callable(
            &mut graph,
            "service_runs_twice",
            None,
            SymbolKind::Test,
            "tests/service.rs",
        )?;
        let target = add_fn(&mut graph, "run", "src/service.rs")?;

        let first = call_site_input(
            test_caller,
            CallForm::Function,
            CallTargetKind::Function,
            "run",
            None,
            None,
            "tests/service.rs",
        )?;
        let first_location = first.location.clone();
        graph.add_call_site(first)?;

        let mut second = call_site_input(
            test_caller,
            CallForm::Function,
            CallTargetKind::Function,
            "run",
            None,
            None,
            "tests/service.rs",
        )?;
        second.location = SourceRange::new(
            file("tests/service.rs")?,
            TextPosition::new(2, 1)?,
            TextPosition::new(2, 4)?,
        )?;
        graph.add_call_site(second)?;

        let calls: Vec<_> = graph
            .outgoing_edges(test_caller)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls && edge.to == target)
            .collect();
        assert_eq!(calls.len(), 2);
        let tests: Vec<_> = graph
            .outgoing_edges(test_caller)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Tests && edge.to == target)
            .collect();
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].location.as_ref(), Some(&first_location));
        assert_eq!(graph.call_sites_from(test_caller).count(), 2);
        assert_eq!(
            graph
                .call_site_for_edge(tests[0])
                .map(|site| &site.location),
            Some(&first_location)
        );
        graph.validate_consistency()?;
        Ok(())
    }

    #[test]
    fn audit_rejects_a_test_relation_with_the_wrong_representative_range()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let test_caller = add_callable(
            &mut graph,
            "service_runs",
            None,
            SymbolKind::Test,
            "tests/service.rs",
        )?;
        let target = add_fn(&mut graph, "run", "src/service.rs")?;
        graph.add_call_site(call_site_input(
            test_caller,
            CallForm::Function,
            CallTargetKind::Function,
            "run",
            None,
            None,
            "tests/service.rs",
        )?)?;

        let wrong_location = SourceRange::new(
            file("tests/service.rs")?,
            TextPosition::new(3, 1)?,
            TextPosition::new(3, 4)?,
        )?;
        if let Some(edges) = graph.outgoing.get_mut(&test_caller) {
            for edge in Arc::make_mut(edges)
                .iter_mut()
                .filter(|edge| edge.kind == EdgeKind::Tests && edge.to == target)
            {
                edge.location = Some(wrong_location.clone());
            }
        }
        if let Some(edges) = graph.incoming.get_mut(&target) {
            for edge in Arc::make_mut(edges)
                .iter_mut()
                .filter(|edge| edge.kind == EdgeKind::Tests && edge.from == test_caller)
            {
                edge.location = Some(wrong_location.clone());
            }
        }

        assert!(matches!(
            graph.validate_consistency(),
            Err(ConsistencyError::ResolvedTestEdgeMissing { .. })
        ));
        Ok(())
    }

    #[test]
    fn call_site_receiver_hints_are_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let caller = add_fn(&mut graph, "caller", "src/caller.rs")?;
        let oversized = "r".repeat(MAX_RECEIVER_HINT_CHARS + 1);
        let result = graph.add_call_site(call_site_input(
            caller,
            CallForm::Member,
            CallTargetKind::Method,
            "target",
            None,
            Some(&oversized),
            "src/caller.rs",
        )?);
        let error = match result {
            Ok(_) => return Err("oversized receiver hint was accepted".into()),
            Err(error) => error,
        };
        assert_eq!(
            error,
            GraphError::ReceiverHintTooLong {
                limit: MAX_RECEIVER_HINT_CHARS
            }
        );
        Ok(())
    }

    #[test]
    fn receiver_type_requires_a_typed_evidence_source() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let caller = add_fn(&mut graph, "caller", "src/caller.rs")?;
        let mut input = call_site_input(
            caller,
            CallForm::Member,
            CallTargetKind::Method,
            "target",
            Some("Service"),
            Some("service"),
            "src/caller.rs",
        )?;
        input.receiver_type = Some("Service".to_owned());
        assert_eq!(
            graph.add_call_site(input),
            Err(GraphError::ReceiverTypeEvidenceMismatch)
        );

        let mut input = call_site_input(
            caller,
            CallForm::Member,
            CallTargetKind::Method,
            "target",
            Some("Service"),
            Some("service"),
            "src/caller.rs",
        )?;
        input.receiver_type_source = Some(ReceiverTypeSource::Parameter);
        assert_eq!(
            graph.add_call_site(input),
            Err(GraphError::ReceiverTypeEvidenceMismatch)
        );
        Ok(())
    }

    #[test]
    fn merge_rejects_overlapping_language_resolution_domains()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut first = SymbolGraph::new();
        add_fn(&mut first, "first", "src/first.rs")?;
        let mut second = SymbolGraph::new();
        add_fn(&mut second, "second", "src/second.rs")?;

        let error = match SymbolGraph::merge([first, second]) {
            Ok(_) => return Err("overlapping language graphs were merged".into()),
            Err(error) => error,
        };
        assert_eq!(
            error,
            GraphError::OverlappingLanguageGraph {
                language: Language::Rust
            }
        );
        Ok(())
    }

    #[test]
    fn merge_shares_disjoint_language_payloads_without_remapping()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut rust = SymbolGraph::new();
        let rust_id = add_fn(&mut rust, "rust_fn", "src/lib.rs")?;
        let mut php = SymbolGraph::new();
        let php_path = file("src/App.php")?;
        let php_id = php.add_symbol(
            SymbolKey {
                language: Language::Php,
                qualified_name: "App::phpFn".to_owned(),
                container: Some("App".to_owned()),
                kind: SymbolKind::Method,
                path: php_path.clone(),
            },
            range(php_path)?,
            None,
            Provenance::TreeSitter,
            Precision::Syntax,
        )?;
        assert_eq!(php_id.0 >> ENTITY_ID_SLOT_SHIFT, 1);

        let combined = SymbolGraph::merge([rust.clone(), php.clone()])?;
        assert_eq!(combined.symbol_count(), 2);
        assert_eq!(combined.symbol(rust_id).map(Symbol::name), Some("rust_fn"));
        assert_eq!(combined.symbol(php_id).map(Symbol::name), Some("phpFn"));
        assert!(combined.shares_symbol_payload_with(&rust, rust_id));
        assert!(combined.shares_symbol_payload_with(&php, php_id));
        combined.validate_consistency()?;
        Ok(())
    }

    #[test]
    fn audit_rejects_a_call_resolution_staled_during_private_construction()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let caller = add_fn(&mut graph, "caller", "src/caller.rs")?;
        add_fn(&mut graph, "target", "src/first.rs")?;
        graph.add_call_site(call_site_input(
            caller,
            CallForm::Function,
            CallTargetKind::Function,
            "target",
            None,
            None,
            "src/caller.rs",
        )?)?;

        // Language adapters add every declaration before call sites. Simulate
        // a broken private builder that violates that ordering; publication's
        // consistency audit must reject its formerly unique resolution.
        add_fn(&mut graph, "other::target", "src/second.rs")?;
        assert_eq!(
            graph.validate_consistency(),
            Err(ConsistencyError::CallSiteResolutionMismatch { index: 0 })
        );
        Ok(())
    }

    #[test]
    fn audit_tracks_resolved_call_edge_multiplicity() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let caller = add_fn(&mut graph, "caller", "src/caller.rs")?;
        let target = add_fn(&mut graph, "target", "src/target.rs")?;
        for _ in 0..2 {
            graph.add_call_site(call_site_input(
                caller,
                CallForm::Function,
                CallTargetKind::Function,
                "target",
                None,
                None,
                "src/caller.rs",
            )?)?;
        }

        // Keep the two adjacency indexes mutually consistent while removing
        // one of the two edges required by the two identical call sites.
        assert!(test_pop_edge(&mut graph.outgoing, caller).is_some());
        assert!(test_pop_edge(&mut graph.incoming, target).is_some());
        graph.edge_count -= 1;

        assert_eq!(
            graph.audit_consistency(),
            Err(ConsistencyError::ResolvedCallEdgeMissing { index: 1 })
        );
        Ok(())
    }

    #[test]
    fn source_files_without_symbols_are_indexed_and_duplicates_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let path = file("src/comments.rs")?;
        graph.add_file(path.clone(), "//! Documentation only.\n")?;
        assert_eq!(graph.file_count(), 1);
        let summaries = graph.file_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].path, path);
        assert_eq!(summaries[0].symbol_count, 0);
        assert_eq!(summaries[0].provenance, Provenance::Git);
        assert_eq!(summaries[0].precision, Precision::Precise);
        assert_eq!(graph.file_source(&path), Some("//! Documentation only.\n"));
        assert_eq!(graph.file_diagnostic_count(&path), Some(0));
        assert_eq!(graph.file_diagnostic_count(&file("src/missing.rs")?), None);
        assert!(matches!(
            graph.add_file(path.clone(), "changed"),
            Err(GraphError::DuplicateFile(duplicate)) if duplicate == path
        ));
        graph.validate_consistency()?;
        Ok(())
    }

    #[test]
    fn rejects_inconsistent_diagnostic_metadata() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let indexed_path = file("src/a.rs")?;
        let wrong_path = file("src/b.rs")?;
        assert!(matches!(
            graph.add_file_with_metadata_and_diagnostics(
                indexed_path.clone(),
                "broken",
                SourceMetadata::path_fallback(&indexed_path),
                vec![diagnostic(wrong_path)?],
                1,
            ),
            Err(GraphError::DiagnosticPathMismatch { .. })
        ));
        assert!(matches!(
            graph.add_file_with_metadata_and_diagnostics(
                indexed_path.clone(),
                "broken",
                SourceMetadata::path_fallback(&indexed_path),
                vec![diagnostic(indexed_path.clone())?],
                0,
            ),
            Err(GraphError::DiagnosticCountUnderflow {
                total: 0,
                retained: 1
            })
        ));
        let mut invalid_quality = diagnostic(indexed_path.clone())?;
        invalid_quality.precision = Precision::Precise;
        assert!(matches!(
            graph.add_file_with_metadata_and_diagnostics(
                indexed_path.clone(),
                "broken",
                SourceMetadata::path_fallback(&indexed_path),
                vec![invalid_quality],
                1,
            ),
            Err(GraphError::InvalidDiagnosticQuality {
                provenance: Provenance::TreeSitter,
                precision: Precision::Precise
            })
        ));
        Ok(())
    }

    #[test]
    fn diagnostic_summary_is_ordered_and_bounded_independent_of_insertion_order()
    -> Result<(), Box<dyn std::error::Error>> {
        fn graph_with_order(paths: [&str; 2]) -> Result<SymbolGraph, Box<dyn std::error::Error>> {
            let mut graph = SymbolGraph::new();
            for raw_path in paths {
                let path = RepoRelativePath::new(raw_path)?;
                graph.add_file_with_metadata_and_diagnostics(
                    path.clone(),
                    "broken",
                    SourceMetadata::path_fallback(&path),
                    vec![diagnostic(path)?],
                    1,
                )?;
            }
            Ok(graph)
        }

        let forward = graph_with_order(["src/a.rs", "src/b.rs"])?.syntax_diagnostics(1);
        let reverse = graph_with_order(["src/b.rs", "src/a.rs"])?.syntax_diagnostics(1);
        assert_eq!(forward, reverse);
        assert_eq!(forward.total_diagnostics, 2);
        assert_eq!(forward.response_omitted, 1);
        assert_eq!(forward.diagnostics[0].range.file().as_str(), "src/a.rs");
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
        test_push_edge(
            &mut graph.incoming,
            a,
            Edge {
                kind: EdgeKind::Calls,
                from: b,
                to: a,
                provenance: Provenance::TreeSitter,
                precision: Precision::Syntax,
                location: None,
            },
        );
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
        test_push_edge(
            &mut graph.incoming,
            a,
            Edge {
                kind: EdgeKind::Calls,
                from: a,
                to: b,
                provenance: Provenance::TreeSitter,
                precision: Precision::Syntax,
                location: None,
            },
        );
        assert!(matches!(
            graph.validate_consistency(),
            Err(ConsistencyError::EdgeWrongIncomingKey { key, to }) if key == a && to == b
        ));
        Ok(())
    }

    #[test]
    fn audit_catches_outgoing_edge_under_wrong_key() -> Result<(), Box<dyn std::error::Error>> {
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
        let edge = test_pop_edge(&mut graph.outgoing, a)
            .ok_or_else(|| std::io::Error::other("test edge must exist"))?;
        test_push_edge(&mut graph.outgoing, b, edge);
        assert!(matches!(
            graph.audit_consistency(),
            Err(ConsistencyError::EdgeWrongOutgoingKey { key, from }) if key == b && from == a
        ));
        Ok(())
    }

    #[test]
    fn audit_catches_unknown_edge_endpoint() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let a = add_fn(&mut graph, "a", "src/a.rs")?;
        let ghost = EntityId(999);
        test_push_edge(
            &mut graph.outgoing,
            a,
            Edge {
                kind: EdgeKind::Calls,
                from: a,
                to: ghost,
                provenance: Provenance::TreeSitter,
                precision: Precision::Syntax,
                location: None,
            },
        );
        assert!(matches!(
            graph.audit_consistency(),
            Err(ConsistencyError::UnknownEntity(id)) if id == ghost
        ));
        Ok(())
    }

    #[test]
    fn audit_catches_missing_identical_parallel_edge_mirror()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        let a = add_fn(&mut graph, "a", "src/a.rs")?;
        let b = add_fn(&mut graph, "b", "src/b.rs")?;
        for _ in 0..2 {
            graph.add_edge(
                EdgeKind::Calls,
                a,
                b,
                Provenance::TreeSitter,
                Precision::Syntax,
                None,
            )?;
        }
        let removed = test_pop_edge(&mut graph.incoming, b);
        assert!(removed.is_some());
        assert!(matches!(
            graph.audit_consistency(),
            Err(ConsistencyError::EdgeMirrorMissing { from, to }) if from == a && to == b
        ));
        Ok(())
    }

    #[test]
    fn audit_catches_extra_identical_parallel_edge_mirror() -> Result<(), Box<dyn std::error::Error>>
    {
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
        let duplicate = graph
            .incoming
            .get(&b)
            .and_then(|edges| edges.first())
            .cloned()
            .ok_or_else(|| std::io::Error::other("test edge must exist"))?;
        test_push_edge(&mut graph.incoming, b, duplicate);
        assert!(matches!(
            graph.audit_consistency(),
            Err(ConsistencyError::EdgeIncomingMirrorMissing { from, to }) if from == a && to == b
        ));
        Ok(())
    }

    #[test]
    fn audit_catches_file_index_order_drift() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        add_fn(&mut graph, "a", "src/a.rs")?;
        add_fn(&mut graph, "b", "src/a.rs")?;
        let path = file("src/a.rs")?;
        let indexed = graph
            .files
            .get(&path)
            .ok_or_else(|| std::io::Error::other("test file must exist"))?;
        let mut indexed = indexed.as_ref().clone();
        indexed.symbols.reverse();
        graph.files.insert_mut(path, Arc::new(indexed));
        assert_eq!(
            graph.audit_consistency(),
            Err(ConsistencyError::FileIndexMismatch)
        );
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

    #[test]
    fn audit_catches_exact_name_index_drift() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        add_fn(&mut graph, "module::target", "src/target.rs")?;

        graph.symbols_by_exact_name = Default::default();

        assert_eq!(
            graph.validate_consistency(),
            Err(ConsistencyError::ExactNameIndexMismatch)
        );
        Ok(())
    }

    #[test]
    fn audit_catches_folded_name_index_drift() -> Result<(), Box<dyn std::error::Error>> {
        let mut graph = SymbolGraph::new();
        add_fn(&mut graph, "module::Target", "src/target.rs")?;

        graph.symbols_by_folded_name = Default::default();

        assert_eq!(
            graph.validate_consistency(),
            Err(ConsistencyError::FoldedNameIndexMismatch)
        );
        Ok(())
    }
}
