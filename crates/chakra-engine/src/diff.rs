//! Adapter-neutral contract for current workspace changes.
//!
//! Git process and output details stay in an outward adapter. The engine
//! supplies one immutable syntax snapshot and receives revision-scoped,
//! provenance-aware file changes that the query layer can join to the graph.

use std::path::PathBuf;
use std::sync::Arc;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{ChangeKind, DiffScope, ResolvedDiffScope};
use chakra_domain::revision::Revision;
use thiserror::Error;

use crate::WorkspaceSnapshot;

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
