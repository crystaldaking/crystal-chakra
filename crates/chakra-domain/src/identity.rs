//! Repository and workspace identity (SPEC §11).
//!
//! For v0.1, `RepositoryId` is derived from the canonical repository root
//! path. This is a known limitation: moving the repository changes its
//! identity. The type boundary exists so a later Git-object-based identity
//! (remotes, alternates, worktrees) touches exactly one constructor.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Repository identity, scoped to the local machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepositoryId(String);

impl RepositoryId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepositoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identity of one materialized workspace (v0.1: the single active worktree).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identity of the single registered workspace: repository id plus the
/// canonical root of the materialized worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceIdentity {
    pub repository: RepositoryId,
    pub workspace: WorkspaceId,
    /// Canonical absolute path of the worktree root.
    pub root: PathBuf,
}

/// Failure to establish workspace identity.
#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("failed to canonicalize repository root {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl WorkspaceIdentity {
    /// Identity of the primary (and only, in v0.1) materialized worktree
    /// rooted at `root`.
    pub fn for_primary_worktree(root: &Path) -> Result<Self, IdentityError> {
        let canonical =
            std::fs::canonicalize(root).map_err(|source| IdentityError::Canonicalize {
                path: root.to_path_buf(),
                source,
            })?;
        // Known limitation (SPEC §11): identity currently is the canonical
        // path. A Git-object-based identity replaces this when the Git
        // subsystem lands; only this constructor changes.
        let repository = RepositoryId(format!("path:{}", canonical.display()));
        let workspace = WorkspaceId(format!("{}:primary", repository.as_str()));
        Ok(Self {
            repository,
            workspace,
            root: canonical,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_deterministic_for_same_root() -> Result<(), IdentityError> {
        let root = std::env::current_dir().map_err(|source| IdentityError::Canonicalize {
            path: PathBuf::from("."),
            source,
        })?;
        let a = WorkspaceIdentity::for_primary_worktree(&root)?;
        let b = WorkspaceIdentity::for_primary_worktree(&root)?;
        assert_eq!(a, b);
        Ok(())
    }

    #[test]
    fn missing_root_is_an_error() {
        let missing = Path::new("/definitely/not/a/real/chakra-path-9f3c2");
        assert!(WorkspaceIdentity::for_primary_worktree(missing).is_err());
    }

    #[test]
    fn dot_and_dotdot_canonicalize_to_same_identity() -> Result<(), IdentityError> {
        let root = std::env::current_dir().map_err(|source| IdentityError::Canonicalize {
            path: PathBuf::from("."),
            source,
        })?;
        let direct = WorkspaceIdentity::for_primary_worktree(&root)?;
        let dotted = WorkspaceIdentity::for_primary_worktree(&root.join("."))?;
        assert_eq!(direct, dotted);
        Ok(())
    }
}
