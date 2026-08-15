//! Current Git/worktree change adapter (SPEC §12, §26).
//!
//! The adapter asks Git to compare `HEAD` with the final materialized
//! worktree and adds untracked, non-ignored Rust files. It never constructs
//! or inspects an administrative Git path, and repository-controlled paths
//! are passed as data rather than through a shell.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path};
use std::process::{Command, Output};

use chakra_domain::location::RepoRelativePath;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::ChangeKind;
use chakra_engine::{
    DiffWorkspace, WorkspaceDiff, WorkspaceDiffError, WorkspaceDiffProvider, WorkspaceFileChange,
};

const MAX_GIT_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_WORKSPACE_CHANGES: usize = 10_000;
const MAX_ERROR_CHARS: usize = 1_024;

/// Fixed-argument Git implementation for the active materialized worktree.
#[derive(Debug, Default)]
pub struct GitWorkspaceDiff;

fn bounded_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim()
        .chars()
        .take(MAX_ERROR_CHARS)
        .collect()
}

fn git_output(
    root: &Path,
    display: &'static str,
    args: &[&OsStr],
) -> Result<Output, WorkspaceDiffError> {
    let output = Command::new("git")
        .current_dir(root)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .args(args)
        .output()
        .map_err(|error| {
            WorkspaceDiffError::new(format!("failed to execute `{display}`: {error}"))
        })?;
    if !output.status.success() {
        return Err(WorkspaceDiffError::new(format!(
            "`{display}` exited with status {}: {}",
            output.status.code().unwrap_or(-1),
            bounded_text(&output.stderr)
        )));
    }
    if output.stdout.len() > MAX_GIT_OUTPUT_BYTES {
        return Err(WorkspaceDiffError::new(format!(
            "`{display}` output exceeded the {MAX_GIT_OUTPUT_BYTES}-byte safety budget"
        )));
    }
    Ok(output)
}

fn has_head(root: &Path) -> Result<bool, WorkspaceDiffError> {
    let worktree = git_output(
        root,
        "git rev-parse --is-inside-work-tree",
        &[OsStr::new("rev-parse"), OsStr::new("--is-inside-work-tree")],
    )?;
    if worktree.stdout != b"true\n" {
        return Err(WorkspaceDiffError::new(
            "repository root is not inside a Git worktree",
        ));
    }

    let output = Command::new("git")
        .current_dir(root)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .map_err(|error| {
            WorkspaceDiffError::new(format!("failed to execute `git rev-parse HEAD`: {error}"))
        })?;
    if output.status.success() {
        Ok(true)
    } else if output.stdout.is_empty() && output.stderr.is_empty() {
        Ok(false)
    } else {
        Err(WorkspaceDiffError::new(format!(
            "`git rev-parse HEAD` exited with status {}: {}",
            output.status.code().unwrap_or(-1),
            bounded_text(&output.stderr)
        )))
    }
}

fn is_rust_source(path: &str) -> bool {
    let path = Path::new(path);
    path.extension() == Some(OsStr::new("rs"))
        && !path.components().any(|component| {
            matches!(component, Component::Normal(value) if value == OsStr::new(".git") || value == OsStr::new("target"))
        })
}

fn parse_path(raw: &[u8]) -> Result<RepoRelativePath, WorkspaceDiffError> {
    let path = std::str::from_utf8(raw)
        .map_err(|_| WorkspaceDiffError::new("Git returned a non-UTF-8 path"))?;
    RepoRelativePath::new(path).map_err(|error| {
        WorkspaceDiffError::new(format!("Git returned an invalid repository path: {error}"))
    })
}

fn insert_change(
    changes: &mut BTreeMap<RepoRelativePath, WorkspaceFileChange>,
    path: RepoRelativePath,
    previous_path: Option<RepoRelativePath>,
    change: ChangeKind,
    precision: Precision,
) {
    changes.insert(
        path.clone(),
        WorkspaceFileChange {
            path,
            previous_path,
            change,
            provenance: Provenance::Git,
            precision,
        },
    );
}

fn parse_tracked_changes(
    output: &[u8],
) -> Result<BTreeMap<RepoRelativePath, WorkspaceFileChange>, WorkspaceDiffError> {
    let mut fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut changes = BTreeMap::new();

    while let Some(raw_status) = fields.next() {
        let status = raw_status
            .first()
            .copied()
            .ok_or_else(|| WorkspaceDiffError::new("Git returned an empty change status"))?;
        if matches!(status, b'R' | b'C') {
            let old = fields
                .next()
                .ok_or_else(|| WorkspaceDiffError::new("Git rename is missing its former path"))?;
            let new = fields
                .next()
                .ok_or_else(|| WorkspaceDiffError::new("Git rename is missing its current path"))?;
            let old_text = std::str::from_utf8(old)
                .map_err(|_| WorkspaceDiffError::new("Git returned a non-UTF-8 path"))?;
            let new_text = std::str::from_utf8(new)
                .map_err(|_| WorkspaceDiffError::new("Git returned a non-UTF-8 path"))?;
            match (is_rust_source(old_text), is_rust_source(new_text), status) {
                (true, true, b'R') => insert_change(
                    &mut changes,
                    parse_path(new)?,
                    Some(parse_path(old)?),
                    ChangeKind::Renamed,
                    Precision::Heuristic,
                ),
                (true, false, _) => insert_change(
                    &mut changes,
                    parse_path(old)?,
                    None,
                    ChangeKind::Deleted,
                    Precision::Heuristic,
                ),
                (false, true, _) | (true, true, b'C') => insert_change(
                    &mut changes,
                    parse_path(new)?,
                    None,
                    ChangeKind::Added,
                    Precision::Heuristic,
                ),
                (false, false, _) => {}
                _ => {}
            }
            continue;
        }

        let raw_path = fields
            .next()
            .ok_or_else(|| WorkspaceDiffError::new("Git change is missing its path"))?;
        let path_text = std::str::from_utf8(raw_path)
            .map_err(|_| WorkspaceDiffError::new("Git returned a non-UTF-8 path"))?;
        if !is_rust_source(path_text) {
            continue;
        }
        let change = match status {
            b'A' => ChangeKind::Added,
            b'D' => ChangeKind::Deleted,
            b'M' | b'T' | b'U' => ChangeKind::Modified,
            other => {
                return Err(WorkspaceDiffError::new(format!(
                    "Git returned unsupported change status `{}`",
                    char::from(other)
                )));
            }
        };
        insert_change(
            &mut changes,
            parse_path(raw_path)?,
            None,
            change,
            Precision::Precise,
        );
    }
    Ok(changes)
}

fn add_untracked_changes(
    root: &Path,
    output: &[u8],
    document_paths: &HashSet<&RepoRelativePath>,
    changes: &mut BTreeMap<RepoRelativePath, WorkspaceFileChange>,
) -> Result<(), WorkspaceDiffError> {
    for raw_path in output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
    {
        let path_text = std::str::from_utf8(raw_path)
            .map_err(|_| WorkspaceDiffError::new("Git returned a non-UTF-8 path"))?;
        if !is_rust_source(path_text) {
            continue;
        }
        let path = parse_path(raw_path)?;
        if !document_paths.contains(&path) {
            continue;
        }
        if changes
            .get(&path)
            .is_some_and(|change| change.change == ChangeKind::Deleted)
        {
            if head_blob_id(root, &path)? == worktree_blob_id(root, &path)? {
                changes.remove(&path);
            } else {
                insert_change(
                    changes,
                    path,
                    None,
                    ChangeKind::Modified,
                    Precision::Precise,
                );
            }
        } else if !changes.contains_key(&path) {
            insert_change(changes, path, None, ChangeKind::Added, Precision::Precise);
        }
    }
    Ok(())
}

fn head_blob_id(root: &Path, path: &RepoRelativePath) -> Result<String, WorkspaceDiffError> {
    let output = git_output(
        root,
        "git ls-tree HEAD",
        &[
            OsStr::new("ls-tree"),
            OsStr::new("-z"),
            OsStr::new("HEAD"),
            OsStr::new("--"),
            OsStr::new(path.as_str()),
        ],
    )?;
    let record = output
        .stdout
        .split(|byte| *byte == 0)
        .find(|field| !field.is_empty())
        .ok_or_else(|| WorkspaceDiffError::new(format!("HEAD has no blob for `{path}`")))?;
    let metadata = record
        .split(|byte| *byte == b'\t')
        .next()
        .ok_or_else(|| WorkspaceDiffError::new("Git returned an invalid ls-tree record"))?;
    let metadata = std::str::from_utf8(metadata)
        .map_err(|_| WorkspaceDiffError::new("Git returned a non-UTF-8 object id"))?;
    metadata
        .split_whitespace()
        .nth(2)
        .map(str::to_owned)
        .ok_or_else(|| WorkspaceDiffError::new("Git returned an invalid ls-tree record"))
}

fn worktree_blob_id(root: &Path, path: &RepoRelativePath) -> Result<String, WorkspaceDiffError> {
    let output = git_output(
        root,
        "git hash-object",
        &[
            OsStr::new("hash-object"),
            OsStr::new("--"),
            OsStr::new(path.as_str()),
        ],
    )?;
    let object_id = std::str::from_utf8(&output.stdout)
        .map_err(|_| WorkspaceDiffError::new("Git returned a non-UTF-8 object id"))?
        .trim();
    if object_id.is_empty() {
        Err(WorkspaceDiffError::new(format!(
            "Git returned no object id for `{path}`"
        )))
    } else {
        Ok(object_id.to_owned())
    }
}

fn validate_current_sources(
    workspace: &DiffWorkspace,
    changes: &BTreeMap<RepoRelativePath, WorkspaceFileChange>,
) -> Result<(), WorkspaceDiffError> {
    let documents: HashMap<_, _> = workspace
        .documents
        .iter()
        .map(|document| (&document.path, document.source.as_ref()))
        .collect();
    for change in changes.values() {
        if change.change == ChangeKind::Deleted {
            continue;
        }
        let Some(snapshot_source) = documents.get(&change.path) else {
            continue;
        };
        let materialized = fs::read_to_string(workspace.repository_root.join(change.path.as_str()))
            .map_err(|error| {
                WorkspaceDiffError::new(format!(
                    "failed to verify current source `{}`: {error}",
                    change.path
                ))
            })?;
        if materialized != *snapshot_source {
            return Err(WorkspaceDiffError::new(format!(
                "materialized source `{}` changed after syntax revision {} was published",
                change.path, workspace.revision
            )));
        }
    }
    Ok(())
}

impl WorkspaceDiffProvider for GitWorkspaceDiff {
    fn diff(&self, workspace: DiffWorkspace) -> Result<WorkspaceDiff, WorkspaceDiffError> {
        let document_paths: HashSet<_> = workspace
            .documents
            .iter()
            .map(|document| &document.path)
            .collect();
        let head_exists = has_head(&workspace.repository_root)?;
        let mut changes = if head_exists {
            let tracked = git_output(
                &workspace.repository_root,
                "git diff --name-status HEAD",
                &[
                    OsStr::new("diff"),
                    OsStr::new("--name-status"),
                    OsStr::new("-z"),
                    OsStr::new("--find-renames"),
                    OsStr::new("--no-ext-diff"),
                    OsStr::new("--ignore-submodules=all"),
                    OsStr::new("HEAD"),
                    OsStr::new("--"),
                ],
            )?;
            parse_tracked_changes(&tracked.stdout)?
        } else {
            let mut unborn = BTreeMap::new();
            for document in &workspace.documents {
                if is_rust_source(document.path.as_str()) {
                    insert_change(
                        &mut unborn,
                        document.path.clone(),
                        None,
                        ChangeKind::Added,
                        Precision::Precise,
                    );
                }
            }
            unborn
        };

        if head_exists {
            let untracked = git_output(
                &workspace.repository_root,
                "git ls-files --others --exclude-standard",
                &[
                    OsStr::new("ls-files"),
                    OsStr::new("--others"),
                    OsStr::new("--exclude-standard"),
                    OsStr::new("-z"),
                ],
            )?;
            add_untracked_changes(
                &workspace.repository_root,
                &untracked.stdout,
                &document_paths,
                &mut changes,
            )?;
        }

        // Current files must belong to the exact syntax snapshot. This also
        // keeps skipped symlinks and a newer, not-yet-reconciled file out of
        // the joined result. Deleted paths intentionally have no document.
        changes.retain(|_, change| {
            change.change == ChangeKind::Deleted || document_paths.contains(&change.path)
        });
        validate_current_sources(&workspace, &changes)?;
        let truncated = changes.len() > MAX_WORKSPACE_CHANGES;
        let files = changes.into_values().take(MAX_WORKSPACE_CHANGES).collect();
        Ok(WorkspaceDiff {
            revision: workspace.revision,
            files,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;

    use chakra_domain::revision::Revision;
    use chakra_engine::{DiffDocument, DiffWorkspace, WorkspaceDiffProvider};
    use tempfile::TempDir;

    use super::*;

    fn git(root: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
        let status = Command::new("git").current_dir(root).args(args).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("git {} failed", args.join(" ")).into())
        }
    }

    fn write(root: &Path, path: &str, source: &str) -> Result<(), Box<dyn Error>> {
        let absolute = root.join(path);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(absolute, source)?;
        Ok(())
    }

    fn repository() -> Result<TempDir, Box<dyn Error>> {
        let repository = TempDir::new()?;
        git(repository.path(), &["init", "--quiet"])?;
        git(
            repository.path(),
            &["config", "user.email", "tests@example.invalid"],
        )?;
        git(repository.path(), &["config", "user.name", "Chakra Tests"])?;
        write(repository.path(), ".gitignore", "ignored.rs\ntarget/\n")?;
        for (path, source) in [
            ("src/staged.rs", "pub fn staged_original() {}\n"),
            ("src/unstaged.rs", "pub fn unstaged_original() {}\n"),
            ("src/deleted.rs", "pub fn deleted_original() {}\n"),
            ("src/cancelled.rs", "pub fn cancelled_original() {}\n"),
            ("src/staged_old.rs", "pub fn staged_rename() {}\n"),
            ("src/unstaged_old.rs", "pub fn unstaged_rename() {}\n"),
            (
                "src/index_removed_same.rs",
                "pub fn index_removed_same() {}\n",
            ),
            (
                "src/index_removed_modified.rs",
                "pub fn index_removed_original() {}\n",
            ),
        ] {
            write(repository.path(), path, source)?;
        }
        git(
            repository.path(),
            &[
                "add",
                ".gitignore",
                "src/staged.rs",
                "src/unstaged.rs",
                "src/deleted.rs",
                "src/cancelled.rs",
                "src/staged_old.rs",
                "src/unstaged_old.rs",
                "src/index_removed_same.rs",
                "src/index_removed_modified.rs",
            ],
        )?;
        git(repository.path(), &["commit", "--quiet", "-m", "base"])?;
        Ok(repository)
    }

    fn workspace(root: &Path, paths: &[&str]) -> Result<DiffWorkspace, Box<dyn Error>> {
        let documents = paths
            .iter()
            .map(|path| {
                Ok(DiffDocument {
                    path: RepoRelativePath::new(*path)?,
                    source: Arc::<str>::from(fs::read_to_string(root.join(path))?),
                })
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        Ok(DiffWorkspace {
            repository_root: root.to_path_buf(),
            revision: Revision(9),
            documents,
        })
    }

    #[test]
    fn combines_final_staged_unstaged_untracked_rename_and_delete_state()
    -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(root, "src/staged.rs", "pub fn staged_now() {}\n")?;
        git(root, &["add", "src/staged.rs"])?;
        write(root, "src/unstaged.rs", "pub fn unstaged_now() {}\n")?;
        fs::remove_file(root.join("src/deleted.rs"))?;
        git(root, &["mv", "src/staged_old.rs", "src/staged_new.rs"])?;
        fs::rename(
            root.join("src/unstaged_old.rs"),
            root.join("src/unstaged_new.rs"),
        )?;
        write(root, "src/untracked.rs", "pub fn untracked() {}\n")?;
        git(
            root,
            &[
                "rm",
                "--cached",
                "--quiet",
                "src/index_removed_same.rs",
                "src/index_removed_modified.rs",
            ],
        )?;
        write(
            root,
            "src/index_removed_modified.rs",
            "pub fn index_removed_now() {}\n",
        )?;
        write(root, "ignored.rs", "pub fn ignored() {}\n")?;
        write(root, "target/generated.rs", "pub fn generated() {}\n")?;

        // The index differs, but the materialized file equals HEAD. The
        // effective HEAD-to-worktree scope must not report it.
        write(root, "src/cancelled.rs", "pub fn staged_only() {}\n")?;
        git(root, &["add", "src/cancelled.rs"])?;
        write(root, "src/cancelled.rs", "pub fn cancelled_original() {}\n")?;

        let workspace = workspace(
            root,
            &[
                "src/staged.rs",
                "src/unstaged.rs",
                "src/cancelled.rs",
                "src/staged_new.rs",
                "src/unstaged_new.rs",
                "src/untracked.rs",
                "src/index_removed_same.rs",
                "src/index_removed_modified.rs",
            ],
        )?;
        let diff = GitWorkspaceDiff.diff(workspace)?;
        assert_eq!(diff.revision, Revision(9));
        assert!(!diff.truncated);
        let by_path: BTreeMap<_, _> = diff
            .files
            .iter()
            .map(|change| (change.path.as_str(), change))
            .collect();

        assert_eq!(by_path.len(), 8);
        assert_eq!(by_path["src/staged.rs"].change, ChangeKind::Modified);
        assert_eq!(by_path["src/unstaged.rs"].change, ChangeKind::Modified);
        assert_eq!(by_path["src/deleted.rs"].change, ChangeKind::Deleted);
        assert_eq!(by_path["src/untracked.rs"].change, ChangeKind::Added);
        assert_eq!(
            by_path["src/index_removed_modified.rs"].change,
            ChangeKind::Modified
        );
        let rename = by_path["src/staged_new.rs"];
        assert_eq!(rename.change, ChangeKind::Renamed);
        assert_eq!(
            rename.previous_path.as_ref().map(RepoRelativePath::as_str),
            Some("src/staged_old.rs")
        );
        assert_eq!(rename.precision, Precision::Heuristic);

        // An unstaged filesystem rename is represented honestly as the
        // tracked deletion plus an untracked addition; Git has not recorded
        // enough evidence to label it a rename.
        assert_eq!(by_path["src/unstaged_old.rs"].change, ChangeKind::Deleted);
        assert_eq!(by_path["src/unstaged_new.rs"].change, ChangeKind::Added);
        assert!(!by_path.contains_key("src/cancelled.rs"));
        assert!(!by_path.contains_key("src/index_removed_same.rs"));
        assert!(!by_path.contains_key("ignored.rs"));
        assert!(!by_path.contains_key("target/generated.rs"));
        assert!(
            diff.files
                .iter()
                .all(|change| change.provenance == Provenance::Git)
        );
        Ok(())
    }

    #[test]
    fn unborn_repository_reports_snapshot_documents_as_added() -> Result<(), Box<dyn Error>> {
        let repository = TempDir::new()?;
        git(repository.path(), &["init", "--quiet"])?;
        write(repository.path(), "src/lib.rs", "pub fn new_repo() {}\n")?;
        let workspace = workspace(repository.path(), &["src/lib.rs"])?;

        let diff = GitWorkspaceDiff.diff(workspace)?;
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path.as_str(), "src/lib.rs");
        assert_eq!(diff.files[0].change, ChangeKind::Added);
        assert_eq!(diff.files[0].precision, Precision::Precise);
        Ok(())
    }

    #[test]
    fn rejects_a_source_that_changed_after_the_snapshot() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        write(
            repository.path(),
            "src/unstaged.rs",
            "pub fn first_edit() {}\n",
        )?;
        let workspace = workspace(repository.path(), &["src/unstaged.rs"])?;
        write(
            repository.path(),
            "src/unstaged.rs",
            "pub fn second_edit() {}\n",
        )?;

        let error = match GitWorkspaceDiff.diff(workspace) {
            Ok(_) => return Err("source mismatch unexpectedly succeeded".into()),
            Err(error) => error,
        };
        assert!(error.to_string().contains("changed after syntax revision"));
        Ok(())
    }
}
