//! Git-backed source discovery and current worktree change adapter.
//!
//! The adapter asks Git to compare a resolved commit baseline with the final
//! materialized worktree and adds untracked, non-ignored supported source
//! files. It never constructs or inspects an administrative Git path, and
//! repository-controlled paths are passed as data rather than through a shell.

mod diff;
mod discovery;
mod project_model;
mod source_metadata;

pub use diff::GitWorkspaceDiff;
pub use discovery::{
    DiscoveryError, WorkspaceInventory, discover_language_files, discover_source_files,
    discover_source_files_in_worktree, discover_source_files_in_worktree_with_context,
    discover_workspace_inventory_in_worktree_with_context, metadata_languages,
    resolve_git_administrative_paths, resolve_repository_identity, resolve_repository_root,
    resolve_repository_root_with_context, resolve_workspace_identity, source_language,
};
pub use project_model::discover_project_model_with_context;
pub use source_metadata::{
    ClassifiedSource, classify_discovered_sources_with_context, discover_classified_sources,
};
