//! Rust-specific view of the shared Git-aware source discovery adapter.

use std::path::{Path, PathBuf};

use chakra_domain::location::RepoRelativePath;
use chakra_domain::symbol::Language;

pub use chakra_git::DiscoveryError;

pub fn resolve_repository_root(candidate: &Path) -> Result<PathBuf, DiscoveryError> {
    chakra_git::resolve_repository_root(candidate)
}

pub fn discover_rust_files(root: &Path) -> Result<Vec<RepoRelativePath>, DiscoveryError> {
    chakra_git::discover_language_files(root, Language::Rust)
}
