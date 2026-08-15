//! Query/application contracts (SPEC §23–§29).
//!
//! These types are the MCP-independent application interface: adapters map
//! transports onto [`QueryService`], and every response is wrapped in a
//! [`QueryEnvelope`]. v0.1 exposes exactly the seven queries listed in
//! `docs/roadmap/v0.1.md` §3; unimplemented ones fail with a typed
//! [`QueryError::Unsupported`] rather than a stub result.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::envelope::QueryEnvelope;
use crate::identity::WorkspaceIdentity;
use crate::location::{RepoRelativePath, SourceRange};
use crate::provenance::{Precision, Provenance};
use crate::revision::Revision;
use crate::state::{FreshnessRequirement, ProviderState};
use crate::symbol::{EdgeKind, EntityId, SymbolKind};

/// Default result budget when a request does not specify one (SPEC §29).
pub const DEFAULT_QUERY_LIMIT: u32 = 20;
/// Hard upper bound for any requested limit; responses are never unbounded.
pub const MAX_QUERY_LIMIT: u32 = 500;

/// Reference to a symbol for entity-based queries (SPEC §24).
///
/// Preferred flow: `symbol_search` → pick a candidate → address it by id.
/// `ById` carries the revision the id was taken from (the envelope revision
/// of the response that returned it); an [`EntityId`] is an arena index and
/// is meaningless once a newer revision is published, so resolution fails
/// with [`QueryError::StaleSymbolRef`] on a mismatch instead of silently
/// returning the wrong symbol. `ByName` resolves only when it is
/// unambiguous; ambiguity is returned as [`QueryError::AmbiguousSymbol`],
/// never guessed away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolRef {
    ById { id: EntityId, revision: Revision },
    ByName(String),
}

/// Flattened symbol view used inside query responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SymbolView {
    pub id: EntityId,
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub location: SourceRange,
    pub signature: Option<String>,
    pub provenance: Provenance,
    pub precision: Precision,
}

impl From<&crate::symbol::Symbol> for SymbolView {
    fn from(symbol: &crate::symbol::Symbol) -> Self {
        Self {
            id: symbol.id,
            name: symbol.name().to_owned(),
            qualified_name: symbol.key.qualified_name.clone(),
            kind: symbol.key.kind,
            location: symbol.location.clone(),
            signature: symbol.signature.clone(),
            provenance: symbol.provenance,
            precision: symbol.precision,
        }
    }
}

/// A typed relation to another symbol, as seen in query responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RelatedSymbol {
    pub symbol: SymbolView,
    pub edge_kind: EdgeKind,
    pub provenance: Provenance,
    pub precision: Precision,
    /// Range of the relation itself when known (e.g. call site).
    pub location: Option<SourceRange>,
}

// --- status ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StatusRequest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexCounts {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderInfo {
    /// Provider name, e.g. `rust-analyzer`.
    pub name: String,
    pub state: ProviderState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StatusData {
    pub workspace: WorkspaceIdentity,
    pub counts: IndexCounts,
    pub providers: Vec<ProviderInfo>,
}

// --- repo_map ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepoMapRequest {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileSummary {
    pub path: RepoRelativePath,
    pub symbol_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepoMapData {
    pub files: Vec<FileSummary>,
}

// --- search (text) ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TextMatch {
    pub file: RepoRelativePath,
    pub range: SourceRange,
    pub line: String,
    pub provenance: Provenance,
    pub precision: Precision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchData {
    pub matches: Vec<TextMatch>,
}

// --- symbol_search ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SymbolSearchRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SymbolSearchData {
    pub query: String,
    pub candidates: Vec<SymbolView>,
}

// --- context ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextRequest {
    pub symbol: Option<SymbolRef>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

/// Bounded, provenance-tagged context around one symbol (SPEC §25).
///
/// Source snippets are intentionally absent until the indexer owns file
/// contents; the fields here are what the syntax graph can honestly answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextData {
    pub symbol: SymbolView,
    pub callers: Vec<RelatedSymbol>,
    pub callees: Vec<RelatedSymbol>,
    pub implementations: Vec<RelatedSymbol>,
    pub tests: Vec<RelatedSymbol>,
    pub related_files: Vec<RepoRelativePath>,
}

// --- callers ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallersRequest {
    pub symbol: Option<SymbolRef>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallersData {
    pub target: SymbolView,
    pub callers: Vec<RelatedSymbol>,
}

// --- diff_context ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiffContextRequest {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChangedFile {
    pub path: RepoRelativePath,
    pub change: ChangeKind,
    pub provenance: Provenance,
    pub precision: Precision,
}

/// Bounded structured result of a diff walk (SPEC §26). Facts must be
/// distinguishable from heuristics through their precision fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiffContextData {
    pub changed_files: Vec<ChangedFile>,
    pub changed_symbols: Vec<SymbolView>,
    pub related_callers: Vec<RelatedSymbol>,
    pub related_tests: Vec<RelatedSymbol>,
}

// --- errors and the service contract ---

/// Typed query failures. Adapters map these onto transport errors; core
/// code never sees transport error types.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QueryError {
    #[error("query `{0}` is not implemented yet")]
    Unsupported(&'static str),
    #[error("symbol reference is required")]
    MissingSymbolRef,
    #[error(
        "symbol reference was taken from revision {reference_revision}, but the published revision is {current_revision}; re-resolve via symbol_search"
    )]
    StaleSymbolRef {
        reference_revision: Revision,
        current_revision: Revision,
    },
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
    #[error(
        "ambiguous symbol reference `{query}`: {candidates} candidates; resolve via symbol_search and EntityId"
    )]
    AmbiguousSymbol { query: String, candidates: usize },
    #[error("invalid request: {0}")]
    Invalid(String),
}

/// The MCP-independent application interface (SPEC §23).
///
/// Implementations must guarantee that a call observes exactly one
/// published revision (SPEC §5) and that large results respect budgets
/// (SPEC §29). Methods are synchronous: v0.1 queries run against an
/// in-memory snapshot and are cheap; async concerns live in the adapters.
pub trait QueryService: Send + Sync {
    fn status(&self, request: StatusRequest) -> Result<QueryEnvelope<StatusData>, QueryError>;
    fn repo_map(&self, request: RepoMapRequest) -> Result<QueryEnvelope<RepoMapData>, QueryError>;
    fn search(&self, request: SearchRequest) -> Result<QueryEnvelope<SearchData>, QueryError>;
    fn symbol_search(
        &self,
        request: SymbolSearchRequest,
    ) -> Result<QueryEnvelope<SymbolSearchData>, QueryError>;
    fn context(&self, request: ContextRequest) -> Result<QueryEnvelope<ContextData>, QueryError>;
    fn callers(&self, request: CallersRequest) -> Result<QueryEnvelope<CallersData>, QueryError>;
    fn diff_context(
        &self,
        request: DiffContextRequest,
    ) -> Result<QueryEnvelope<DiffContextData>, QueryError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_are_bounded() {
        let default = DEFAULT_QUERY_LIMIT;
        let max = MAX_QUERY_LIMIT;
        assert!(default > 0 && default <= max);
    }

    #[test]
    fn symbol_ref_serializes_as_snake_case_tagged() -> Result<(), Box<dyn std::error::Error>> {
        let by_name = serde_json::to_value(SymbolRef::ByName("refund".to_owned()))?;
        assert_eq!(by_name, serde_json::json!({ "by_name": "refund" }));
        let by_id = serde_json::to_value(SymbolRef::ById {
            id: EntityId(7),
            revision: Revision(42),
        })?;
        assert_eq!(
            by_id,
            serde_json::json!({ "by_id": { "id": 7, "revision": 42 } })
        );
        Ok(())
    }

    #[test]
    fn request_defaults_require_fresh() {
        let request = SymbolSearchRequest::default();
        assert_eq!(request.freshness, FreshnessRequirement::RequireFresh);
    }

    #[test]
    fn unsupported_is_a_typed_error() {
        let error = QueryError::Unsupported("search");
        assert!(error.to_string().contains("search"));
    }
}
