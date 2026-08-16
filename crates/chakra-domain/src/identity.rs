//! Repository and workspace identity (SPEC §11).
//!
//! Repository identity is supplied by the outward Git adapter. The domain
//! layer deliberately does not inspect Git administration or remote URLs.
//! A path-derived constructor remains available for isolated/static engines
//! that are not attached to a Git repository.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Repository identity, scoped to the local machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepositoryId(String);

impl RepositoryId {
    /// Creates an identity from an adapter-owned stable repository key.
    pub fn from_stable_key(key: impl Into<String>) -> Result<Self, IdentityError> {
        let key = key.into();
        if key.is_empty() {
            return Err(IdentityError::EmptyRepositoryKey);
        }
        Ok(Self(key))
    }

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
    #[error("repository identity key must not be empty")]
    EmptyRepositoryKey,
}

impl WorkspaceIdentity {
    /// Identity of one materialized worktree for an adapter-established
    /// repository identity.
    pub fn for_repository(root: &Path, repository: RepositoryId) -> Result<Self, IdentityError> {
        let canonical = canonicalize_root(root)?;
        let workspace = WorkspaceId(format!(
            "{}:worktree:{}",
            repository.as_str(),
            canonical.display()
        ));
        Ok(Self {
            repository,
            workspace,
            root: canonical,
        })
    }

    /// Identity of the primary (and only, in v0.1) materialized worktree
    /// rooted at `root` when no Git adapter is available.
    ///
    /// Production Git workspaces should use the Git adapter's repository
    /// identity and [`WorkspaceIdentity::for_repository`].
    pub fn for_primary_worktree(root: &Path) -> Result<Self, IdentityError> {
        let canonical = canonicalize_root(root)?;
        let repository =
            RepositoryId::from_stable_key(format!("standalone-path:{}", canonical.display()))?;
        Self::for_repository(&canonical, repository)
    }
}

fn canonicalize_root(root: &Path) -> Result<PathBuf, IdentityError> {
    std::fs::canonicalize(root).map_err(|source| IdentityError::Canonicalize {
        path: root.to_path_buf(),
        source,
    })
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

    #[test]
    fn adapter_repository_identity_is_preserved() -> Result<(), IdentityError> {
        let root = std::env::current_dir().map_err(|source| IdentityError::Canonicalize {
            path: PathBuf::from("."),
            source,
        })?;
        let repository = RepositoryId::from_stable_key("git-roots:abc123")?;
        let identity = WorkspaceIdentity::for_repository(&root, repository.clone())?;
        assert_eq!(identity.repository, repository);
        assert!(identity.workspace.as_str().contains(":worktree:"));
        Ok(())
    }

    #[test]
    fn empty_adapter_repository_key_is_rejected() {
        assert!(matches!(
            RepositoryId::from_stable_key(""),
            Err(IdentityError::EmptyRepositoryKey)
        ));
    }
}
