//! Adapter-neutral contract for current workspace changes.
//!
//! Git process and output details stay in an outward adapter. The engine
//! supplies one immutable syntax snapshot and receives revision-scoped,
//! provenance-aware file changes that the query layer can join to the graph.

use std::path::PathBuf;
use std::sync::Arc;

use chakra_domain::composition::{
    CommitSnapshotLayer, OverlayFileChange, SourceLayer, WorkspaceGraphLayers, WorktreeOverlayLayer,
};
use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{ChangeKind, DiffScope, ResolvedDiffScope};
use chakra_domain::revision::Revision;
use std::collections::BTreeMap;
use thiserror::Error;

use crate::SymbolGraph;
use crate::WorkspaceSnapshot;

const MAX_PUBLISHED_OVERLAY_FILES: usize = 500;

/// One source document captured in the same atomically published revision
/// used by the query.
#[derive(Debug, Clone)]
pub struct DiffDocument {
    pub path: RepoRelativePath,
    pub source: Arc<str>,
}

/// Immutable input to a workspace-diff adapter.
#[derive(Debug, Clone)]
pub struct DiffWorkspace {
    pub repository_root: PathBuf,
    pub revision: Revision,
    pub scope: DiffScope,
    pub documents: Vec<DiffDocument>,
}

impl DiffWorkspace {
    pub fn from_graph_with_context(
        repository_root: PathBuf,
        revision: Revision,
        scope: DiffScope,
        graph: &SymbolGraph,
        operation: &OperationContext,
    ) -> Result<Self, OperationAbort> {
        let documents = graph
            .snapshot_documents_with_context(operation)?
            .into_iter()
            .map(|(path, source)| DiffDocument { path, source })
            .collect();
        Ok(Self {
            repository_root,
            revision,
            scope,
            documents,
        })
    }

    pub(crate) fn from_snapshot_with_context(
        snapshot: &WorkspaceSnapshot,
        scope: DiffScope,
        operation: &OperationContext,
    ) -> Result<Self, OperationAbort> {
        let documents = snapshot
            .graph()
            .snapshot_documents_with_context(operation)?
            .into_iter()
            .map(|(path, source)| DiffDocument { path, source })
            .collect();
        Ok(Self {
            repository_root: snapshot.identity().root.clone(),
            revision: snapshot.revision(),
            scope,
            documents,
        })
    }
}

/// Converts an adapter-verified worktree diff into the layer metadata that is
/// atomically published beside the effective graph.
pub fn workspace_graph_layers(
    source_files: u64,
    source_bytes: u64,
    diff: &WorkspaceDiff,
) -> WorkspaceGraphLayers {
    let published_files = diff.files.len().min(MAX_PUBLISHED_OVERLAY_FILES);
    let locally_omitted = diff.files.len().saturating_sub(published_files) as u64;
    let files_omitted = match diff.truncation {
        Some(truncation) => truncation
            .omitted
            .map(|omitted| locally_omitted.saturating_add(omitted as u64)),
        None if locally_omitted > 0 => Some(locally_omitted),
        None => None,
    };
    WorkspaceGraphLayers {
        commit_snapshot: CommitSnapshotLayer {
            commit: diff.scope.base_commit.clone(),
            source_files,
            source_bytes,
        },
        worktree_overlay: WorktreeOverlayLayer {
            files: diff
                .files
                .iter()
                .take(published_files)
                .map(|change| OverlayFileChange {
                    path: change.path.clone(),
                    previous_path: change.previous_path.clone(),
                    change: change.change,
                })
                .collect(),
            files_truncated: locally_omitted > 0 || diff.truncation.is_some(),
            files_omitted,
        },
    }
}

/// Exact current-file ownership derived from immutable commit and effective
/// source contents. Unlike the bounded public overlay inventory, this map
/// cannot mislabel a fact when the changed-file list is truncated.
pub fn source_layers_for_graph(
    commit_graph: &SymbolGraph,
    effective_graph: &SymbolGraph,
) -> BTreeMap<RepoRelativePath, SourceLayer> {
    effective_graph
        .source_files_iter()
        .map(|(path, source)| {
            let layer = if commit_graph.file_source(path) == Some(source) {
                SourceLayer::CommitSnapshot
            } else {
                SourceLayer::WorktreeOverlay
            };
            (path.clone(), layer)
        })
        .collect()
}

/// One current file change supplied by the workspace adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFileChange {
    pub path: RepoRelativePath,
    pub previous_path: Option<RepoRelativePath>,
    pub change: ChangeKind,
    pub provenance: Provenance,
    pub precision: Precision,
}

/// Bounded change inventory for one syntax revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDiff {
    pub revision: Revision,
    pub scope: ResolvedDiffScope,
    /// Deterministic adapter order. The query layer examines only a bounded
    /// prefix before applying its own stable path ordering and result limits.
    pub files: Vec<WorkspaceFileChange>,
    pub truncation: Option<DiffInventoryTruncation>,
}

/// Adapter-reported bound applied before query-local section budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffInventoryTruncation {
    pub limit: usize,
    pub omitted: Option<usize>,
}

/// Failure to derive current changes without inventing partial results.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("workspace diff failed: {message}")]
pub struct WorkspaceDiffError {
    message: String,
}

impl WorkspaceDiffError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Optional adapter installed for the one active v0.1 worktree. Implementors
/// must return `files` in deterministic order for an unchanged worktree and
/// apply their own finite inventory bound before returning.
pub trait WorkspaceDiffProvider: std::fmt::Debug + Send + Sync {
    fn diff(&self, workspace: DiffWorkspace) -> Result<WorkspaceDiff, WorkspaceDiffError> {
        self.diff_with_context(workspace, &OperationContext::unbounded())
    }

    fn diff_with_context(
        &self,
        workspace: DiffWorkspace,
        operation: &OperationContext,
    ) -> Result<WorkspaceDiff, WorkspaceDiffError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chakra_domain::query::DiffScope;

    #[test]
    fn published_overlay_inventory_is_bounded_and_reports_omissions()
    -> Result<(), Box<dyn std::error::Error>> {
        let files = (0..=MAX_PUBLISHED_OVERLAY_FILES)
            .map(|index| {
                Ok(WorkspaceFileChange {
                    path: RepoRelativePath::new(format!("src/file_{index:04}.rs"))?,
                    previous_path: None,
                    change: ChangeKind::Modified,
                    provenance: Provenance::Git,
                    precision: Precision::Precise,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let layers = workspace_graph_layers(
            501,
            1_024,
            &WorkspaceDiff {
                revision: Revision::INITIAL,
                scope: ResolvedDiffScope {
                    requested: DiffScope::Worktree,
                    base_commit: Some("a".repeat(40)),
                },
                files,
                truncation: Some(DiffInventoryTruncation {
                    limit: MAX_PUBLISHED_OVERLAY_FILES + 1,
                    omitted: Some(7),
                }),
            },
        );

        assert_eq!(
            layers.worktree_overlay.files.len(),
            MAX_PUBLISHED_OVERLAY_FILES
        );
        assert_eq!(layers.worktree_overlay.files_omitted, Some(8));
        assert!(layers.worktree_overlay.files_truncated);
        Ok(())
    }

    #[test]
    fn published_overlay_preserves_unknown_adapter_truncation() {
        let layers = workspace_graph_layers(
            0,
            0,
            &WorkspaceDiff {
                revision: Revision::INITIAL,
                scope: ResolvedDiffScope {
                    requested: DiffScope::Worktree,
                    base_commit: None,
                },
                files: Vec::new(),
                truncation: Some(DiffInventoryTruncation {
                    limit: 0,
                    omitted: None,
                }),
            },
        );

        assert!(layers.worktree_overlay.files_truncated);
        assert_eq!(layers.worktree_overlay.files_omitted, None);
    }
}
