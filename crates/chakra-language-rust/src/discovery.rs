//! Git-aware repository file discovery (SPEC §20; roadmap §11).

use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use chakra_domain::location::{RepoPathError, RepoRelativePath};
use thiserror::Error;

/// Failure to establish the Git worktree or enumerate its source files.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("failed to execute Git command `{command}`: {source}")]
    Spawn {
        command: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("Git command `{command}` failed with status {status}: {stderr}")]
    Git {
        command: &'static str,
        status: i32,
        stderr: String,
    },
    #[error("Git returned a non-UTF-8 repository path")]
    NonUtf8Root,
    #[error("Git returned a non-UTF-8 repository-relative path")]
    NonUtf8Path,
    #[error("failed to canonicalize Git worktree root {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Git returned an invalid repository-relative path: {0}")]
    InvalidPath(#[from] RepoPathError),
}

fn git_output(
    current_dir: &Path,
    command_name: &'static str,
    args: &[&OsStr],
) -> Result<Output, DiscoveryError> {
    let output = Command::new("git")
        .current_dir(current_dir)
        .args(args)
        .output()
        .map_err(|source| DiscoveryError::Spawn {
            command: command_name,
            source,
        })?;
    if !output.status.success() {
        return Err(DiscoveryError::Git {
            command: command_name,
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(output)
}

/// Resolves the containing worktree root through Git itself.
///
/// This deliberately never assumes that Git administration lives at
/// `<root>/.git`; linked worktrees commonly use a `.git` indirection file.
pub fn resolve_repository_root(candidate: &Path) -> Result<PathBuf, DiscoveryError> {
    let output = git_output(
        candidate,
        "rev-parse --show-toplevel",
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--path-format=absolute"),
            OsStr::new("--show-toplevel"),
        ],
    )?;
    let raw = std::str::from_utf8(&output.stdout).map_err(|_| DiscoveryError::NonUtf8Root)?;
    // `rev-parse` terminates this one path with a newline. Strip only line
    // terminators so other path whitespace stays significant.
    let raw = raw.strip_suffix('\n').unwrap_or(raw);
    let root = PathBuf::from(raw.strip_suffix('\r').unwrap_or(raw));
    std::fs::canonicalize(&root)
        .map_err(|source| DiscoveryError::Canonicalize { path: root, source })
}

fn is_excluded(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(component, Component::Normal(value) if value == OsStr::new(".git") || value == OsStr::new("target"))
    })
}

/// Returns tracked plus untracked, non-ignored Rust files in deterministic
/// repository-relative order.
pub fn discover_rust_files(root: &Path) -> Result<Vec<RepoRelativePath>, DiscoveryError> {
    let root = resolve_repository_root(root)?;
    let output = git_output(
        &root,
        "ls-files --cached --others --exclude-standard",
        &[
            OsStr::new("ls-files"),
            OsStr::new("--cached"),
            OsStr::new("--others"),
            OsStr::new("--exclude-standard"),
            OsStr::new("-z"),
        ],
    )?;

    let mut files = Vec::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let raw = std::str::from_utf8(raw).map_err(|_| DiscoveryError::NonUtf8Path)?;
        let candidate = Path::new(raw);
        if candidate.extension() != Some(OsStr::new("rs")) || is_excluded(candidate) {
            continue;
        }
        // A tracked path may currently be deleted. Symlinks are skipped so
        // repository content cannot make the indexer read outside the root.
        let Ok(metadata) = std::fs::symlink_metadata(root.join(candidate)) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        files.push(RepoRelativePath::new(raw)?);
    }
    files.sort();
    files.dedup();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn git(root: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
        let status = Command::new("git").current_dir(root).args(args).status()?;
        if !status.success() {
            return Err(format!("git {} failed", args.join(" ")).into());
        }
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
        fs::create_dir_all(repository.path().join("src"))?;
        fs::write(
            repository.path().join("src/lib.rs"),
            "pub fn tracked() {}\n",
        )?;
        fs::write(
            repository.path().join(".gitignore"),
            "ignored.rs\ntarget/\n",
        )?;
        git(repository.path(), &["add", "src/lib.rs", ".gitignore"])?;
        git(repository.path(), &["commit", "--quiet", "-m", "fixture"])?;
        Ok(repository)
    }

    #[test]
    fn includes_tracked_and_untracked_but_not_ignored_or_target() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(
            repository.path().join("src/untracked.rs"),
            "fn new_file() {}\n",
        )?;
        fs::write(repository.path().join("ignored.rs"), "fn ignored() {}\n")?;
        fs::create_dir_all(repository.path().join("target"))?;
        fs::write(
            repository.path().join("target/generated.rs"),
            "fn generated() {}\n",
        )?;

        let files = discover_rust_files(repository.path())?;
        let paths: Vec<&str> = files.iter().map(RepoRelativePath::as_str).collect();
        assert_eq!(paths, ["src/lib.rs", "src/untracked.rs"]);
        Ok(())
    }

    #[test]
    fn tracked_file_remains_visible_even_when_an_ignore_rule_matches() -> Result<(), Box<dyn Error>>
    {
        let repository = repository()?;
        fs::write(
            repository.path().join("tracked_generated.rs"),
            "fn kept() {}\n",
        )?;
        fs::write(
            repository.path().join(".gitignore"),
            "ignored.rs\ntarget/\ntracked_generated.rs\n",
        )?;
        git(repository.path(), &["add", "-f", "tracked_generated.rs"])?;

        let files = discover_rust_files(repository.path())?;
        assert!(
            files
                .iter()
                .any(|path| path.as_str() == "tracked_generated.rs")
        );
        Ok(())
    }

    #[test]
    fn resolves_and_scans_a_linked_worktree_without_git_layout_assumptions()
    -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let worktree_parent = TempDir::new()?;
        let worktree = worktree_parent.path().join("linked");
        git(
            repository.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "fixture-linked",
                worktree.to_str().ok_or("worktree path is not UTF-8")?,
            ],
        )?;

        assert!(worktree.join(".git").is_file());
        assert_eq!(
            resolve_repository_root(&worktree.join("src"))?,
            fs::canonicalize(&worktree)?
        );
        let files = discover_rust_files(&worktree)?;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].as_str(), "src/lib.rs");
        Ok(())
    }
}
