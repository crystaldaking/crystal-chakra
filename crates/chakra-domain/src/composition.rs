//! Typed provenance for the three layers of an effective workspace graph.
//!
//! The syntax graph is composed from immutable commit facts plus a
//! materialized worktree overlay. Precise enrichment remains a separate,
//! revision-bound layer and is never treated as commit truth.

use serde::{Deserialize, Serialize};

use crate::identity::WorkspaceId;
use crate::location::RepoRelativePath;
use crate::query::ChangeKind;
use crate::revision::Revision;
use crate::state::ProviderState;

/// Source layer that owns the current contents of a returned file fact.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceLayer {
    /// Immutable syntax/inventory facts derived from the base commit.
    #[default]
    CommitSnapshot,
    /// Current materialized contents differ from the base commit.
    WorktreeOverlay,
}

/// One deterministic file-owned worktree contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OverlayFileChange {
    /// Current path, or the former path when the file is deleted.
    pub path: RepoRelativePath,
    /// Former path for a Git-detected rename.
    pub previous_path: Option<RepoRelativePath>,
    pub change: ChangeKind,
}

/// Materialization-independent commit layer retained by one workspace
/// revision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CommitSnapshotLayer {
    /// Exact immutable commit object. `None` denotes an unborn repository.
    pub commit: Option<String>,
    pub source_files: u64,
    pub source_bytes: u64,
}

/// Materialized delta relative to [`CommitSnapshotLayer::commit`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorktreeOverlayLayer {
    /// Deterministic Git order. Publication applies a finite inventory bound.
    pub files: Vec<OverlayFileChange>,
    /// Whether the public inventory is incomplete.
    pub files_truncated: bool,
    /// Exact omitted count when known; `None` is valid for both a complete
    /// inventory and a truncated adapter result whose omitted count is
    /// unknowable, so consumers must inspect [`Self::files_truncated`].
    pub files_omitted: Option<u64>,
}

/// Atomically published syntax-layer composition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceGraphLayers {
    pub commit_snapshot: CommitSnapshotLayer,
    pub worktree_overlay: WorktreeOverlayLayer,
}

/// Query-relative precise-enrichment layer. A provider may be ready only for
/// the exact syntax revision reported here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceEnrichmentLayer {
    pub workspace_id: WorkspaceId,
    pub provider_state: ProviderState,
    /// Present only when precise enrichment is ready for this exact revision.
    pub revision: Option<Revision>,
}

/// Complete provenance envelope for one effective workspace graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EffectiveWorkspaceLayers {
    pub commit_snapshot: CommitSnapshotLayer,
    pub worktree_overlay: WorktreeOverlayLayer,
    pub workspace_enrichment: WorkspaceEnrichmentLayer,
}

impl EffectiveWorkspaceLayers {
    pub fn from_graph_layers(
        graph: &WorkspaceGraphLayers,
        workspace_id: WorkspaceId,
        revision: Revision,
        provider_state: ProviderState,
    ) -> Self {
        Self {
            commit_snapshot: graph.commit_snapshot.clone(),
            worktree_overlay: graph.worktree_overlay.clone(),
            workspace_enrichment: WorkspaceEnrichmentLayer {
                workspace_id,
                provider_state,
                revision: (provider_state == ProviderState::Ready).then_some(revision),
            },
        }
    }
}
