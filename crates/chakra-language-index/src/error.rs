//! Shared indexing failure surface. Language crates alias this type as
//! their historical `XIndexError`; messages stay language-neutral because
//! the language is always visible from the caller context.

use std::io;

use chakra_domain::location::RepoRelativePath;
use chakra_engine::{ConsistencyError, GraphError};
use chakra_git::DiscoveryError;

/// Failure to discover, read, parse, or validate a language syntax index.
#[derive(Debug, thiserror::Error)]
pub enum LanguageIndexError {
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    #[error("failed to read source {path}: {source}")]
    Read {
        path: RepoRelativePath,
        #[source]
        source: io::Error,
    },
    #[error("source {path} exceeds the {limit}-byte indexing budget")]
    SourceTooLarge {
        path: RepoRelativePath,
        limit: usize,
    },
    #[error("indexed sources exceed the {limit}-byte repository budget")]
    RepositoryTooLarge { limit: usize },
    #[error("failed to parse source: {0}")]
    Parse(String),
    #[error("failed to start a bounded parser worker: {0}")]
    WorkerSpawn(#[source] io::Error),
    #[error("a bounded parser worker panicked")]
    WorkerPanicked,
    #[error("syntax index update failed: {0}")]
    Update(String),
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error("constructed syntax graph is inconsistent: {0}")]
    Consistency(#[from] ConsistencyError),
    #[error("syntax indexing was cancelled")]
    Cancelled,
}
