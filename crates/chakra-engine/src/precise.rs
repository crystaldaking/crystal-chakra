//! Language-neutral contract for optional, query-time precise enrichment.
//!
//! The engine owns neither an LSP client nor provider-specific protocol
//! types. An adapter receives an immutable syntax workspace, synchronizes its
//! provider, and may return bounded precise relations for that exact
//! revision. A mismatch is treated as catching up by the query layer.

use std::path::PathBuf;
use std::sync::Arc;

use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::provenance::Provenance;
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;

use crate::WorkspaceSnapshot;

/// One exact source document captured in a published syntax revision.
#[derive(Debug, Clone)]
pub struct ProviderDocument {
    pub path: RepoRelativePath,
    pub source: Arc<str>,
}

/// Immutable input used to synchronize a live provider without giving it
/// access to mutable engine state.
#[derive(Debug, Clone)]
pub struct ProviderWorkspace {
    pub repository_root: PathBuf,
    pub revision: Revision,
    pub documents: Vec<ProviderDocument>,
}

impl ProviderWorkspace {
    pub(crate) fn from_snapshot(snapshot: &WorkspaceSnapshot) -> Self {
        let documents = snapshot
            .graph()
            .snapshot_documents()
            .into_iter()
            .map(|(path, source)| ProviderDocument { path, source })
            .collect();
        Self {
            repository_root: snapshot.identity().root.clone(),
            revision: snapshot.revision(),
            documents,
        }
    }
}

/// Syntax declaration selected by the caller before precise enrichment.
#[derive(Debug, Clone)]
pub struct ProviderSymbol {
    pub name: String,
    pub declaration: SourceRange,
}

/// The two bounded call-hierarchy directions needed by v0.1 queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallHierarchyDirections {
    pub incoming: bool,
    pub outgoing: bool,
}

/// Adapter-neutral request. No LSP positions, URIs, or protocol enums cross
/// this boundary.
#[derive(Debug, Clone)]
pub struct PreciseQueryRequest {
    pub workspace: ProviderWorkspace,
    pub symbol: ProviderSymbol,
    pub directions: CallHierarchyDirections,
    pub limit: usize,
}

/// One provider-confirmed relationship endpoint and optional call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreciseRelation {
    pub name: String,
    pub declaration: SourceRange,
    pub call_site: Option<SourceRange>,
    pub provenance: Provenance,
}

/// A bounded result explicitly tied to the syntax revision it enriches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreciseQueryResult {
    pub revision: Revision,
    pub state: ProviderState,
    pub incoming: Vec<PreciseRelation>,
    pub outgoing: Vec<PreciseRelation>,
    pub truncated: bool,
}

impl PreciseQueryResult {
    /// Honest syntax fallback when the provider cannot prove currency.
    pub fn unavailable(revision: Revision, state: ProviderState) -> Self {
        Self {
            revision,
            state,
            incoming: Vec::new(),
            outgoing: Vec::new(),
            truncated: false,
        }
    }
}

/// Optional precise-provider adapter installed for one active workspace.
pub trait PreciseProvider: std::fmt::Debug + Send + Sync {
    /// State relative to a specific published syntax revision.
    fn state_for(&self, revision: Revision) -> ProviderState;

    /// Bounded operator-facing reason for the current degraded/catching-up
    /// state, when the adapter has one.
    fn last_error(&self) -> Option<String> {
        None
    }

    /// Lazily enrich one selected symbol. Implementations must bound waiting
    /// and return `CatchingUp`/`Degraded` rather than stale precise facts.
    fn enrich(&self, request: PreciseQueryRequest) -> PreciseQueryResult;
}
