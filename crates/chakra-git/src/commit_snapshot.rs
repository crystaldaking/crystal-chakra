//! Materialization-independent source capture from immutable Git objects.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;

use crate::discovery::{
    DiscoveryError, capture_git, git_output, raw_may_be_source, read_bounded,
    resolve_repository_root_with_context, source_language,
};

const MAX_BATCH_STDERR_BYTES: usize = 16 * 1024;
const BATCH_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CHILD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1);

/// Resource envelope applied while Git blobs are captured for a commit
/// snapshot. Inventory remains exact even when source bodies are omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitSnapshotLimits {
    pub max_files: u64,
    pub max_source_file_bytes: u64,
    pub max_workspace_source_bytes: u64,
}

/// Immutable source inventory for one exact commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommitSnapshot {
    pub repository_root: PathBuf,
    pub commit: Option<String>,
    pub sources: BTreeMap<RepoRelativePath, Arc<str>>,
    pub discovered_files: u64,
    pub source_bytes: u64,
    pub oversized_files: u64,
    pub workspace_omitted_files: u64,
    pub non_utf8_files: u64,
}

#[derive(Debug)]
struct TreeSource {
    path: RepoRelativePath,
    object_id: String,
    size: u64,
}

#[derive(Debug)]
struct CommitBlobRead {
    sources: BTreeMap<RepoRelativePath, Arc<str>>,
    source_bytes: u64,
    non_utf8_files: u64,
}

/// Resolves the exact commit currently named by `HEAD`. An unborn repository
/// returns `None` instead of inventing a baseline.
pub fn resolve_head_commit_with_context(
    root: &Path,
    operation: &OperationContext,
) -> Result<Option<String>, DiscoveryError> {
    let root = resolve_repository_root_with_context(root, operation)?;
    let output = capture_git(
        &root,
        "rev-parse --verify HEAD^{commit}",
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new("HEAD^{commit}"),
        ],
        operation,
    )?;
    if !output.status.success() {
        if output.status.code() == Some(1) && output.stderr.is_empty() {
            return Ok(None);
        }
        return Err(DiscoveryError::Git {
            command: "rev-parse --verify HEAD^{commit}",
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let raw = std::str::from_utf8(&output.stdout)
        .map_err(|_| DiscoveryError::InvalidRootObjectId("non-UTF-8 HEAD".to_owned()))?;
    let commit = raw.trim_end_matches(['\r', '\n']);
    validate_object_id(commit)?;
    Ok(Some(commit.to_owned()))
}

/// Captures `HEAD` exclusively through Git tree/blob objects.
pub fn read_head_commit_snapshot_with_context(
    root: &Path,
    limits: CommitSnapshotLimits,
    operation: &OperationContext,
) -> Result<GitCommitSnapshot, DiscoveryError> {
    let repository_root = resolve_repository_root_with_context(root, operation)?;
    let commit = resolve_head_commit_with_context(&repository_root, operation)?;
    read_commit_snapshot_with_context(&repository_root, commit.as_deref(), limits, operation)
}

/// Captures an already resolved immutable commit object. `None` produces the
/// empty commit layer used by an unborn repository.
pub fn read_commit_snapshot_with_context(
    root: &Path,
    commit: Option<&str>,
    limits: CommitSnapshotLimits,
    operation: &OperationContext,
) -> Result<GitCommitSnapshot, DiscoveryError> {
    let repository_root = resolve_repository_root_with_context(root, operation)?;
    let Some(commit) = commit else {
        return Ok(GitCommitSnapshot {
            repository_root,
            commit: None,
            sources: BTreeMap::new(),
            discovered_files: 0,
            source_bytes: 0,
            oversized_files: 0,
            workspace_omitted_files: 0,
            non_utf8_files: 0,
        });
    };
    validate_object_id(commit)?;
    operation.check()?;
    let output = git_output(
        &repository_root,
        "ls-tree -r --long",
        &[
            OsStr::new("ls-tree"),
            OsStr::new("-r"),
            OsStr::new("-z"),
            OsStr::new("--long"),
            OsStr::new("--full-tree"),
            OsStr::new(commit),
            OsStr::new("--"),
        ],
        operation,
    )?;
    let tree = parse_tree_sources(&output.stdout, operation)?;
    let discovered_files = tree.len() as u64;
    let mut requested = Vec::new();
    let mut reserved_source_bytes = 0_u64;
    let mut oversized_files = 0_u64;
    let mut workspace_omitted_files = 0_u64;
    for (index, entry) in tree.into_iter().enumerate() {
        operation.check()?;
        if index as u64 >= limits.max_files {
            continue;
        }
        if entry.size > limits.max_source_file_bytes {
            oversized_files = oversized_files.saturating_add(1);
            continue;
        }
        if reserved_source_bytes.saturating_add(entry.size) > limits.max_workspace_source_bytes {
            workspace_omitted_files = workspace_omitted_files.saturating_add(1);
            continue;
        }
        reserved_source_bytes = reserved_source_bytes.saturating_add(entry.size);
        requested.push(entry);
    }
    let blobs = read_commit_blobs(&repository_root, requested, operation)?;
    Ok(GitCommitSnapshot {
        repository_root,
        commit: Some(commit.to_owned()),
        sources: blobs.sources,
        discovered_files,
        source_bytes: blobs.source_bytes,
        oversized_files,
        workspace_omitted_files,
        non_utf8_files: blobs.non_utf8_files,
    })
}

fn read_commit_blobs(
    repository_root: &Path,
    entries: Vec<TreeSource>,
    operation: &OperationContext,
) -> Result<CommitBlobRead, DiscoveryError> {
    if entries.is_empty() {
        return Ok(CommitBlobRead {
            sources: BTreeMap::new(),
            source_bytes: 0,
            non_utf8_files: 0,
        });
    }
    let command_name = "cat-file --batch";
    let mut child = Command::new("git")
        .current_dir(repository_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["cat-file", "--batch"])
        .spawn()
        .map_err(|source| DiscoveryError::Spawn {
            command: command_name,
            source,
        })?;
    let stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            terminate_child(&mut child);
            return Err(DiscoveryError::Spawn {
                command: command_name,
                source: std::io::Error::other("Git stdin pipe is unavailable"),
            });
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_child(&mut child);
            return Err(DiscoveryError::Spawn {
                command: command_name,
                source: std::io::Error::other("Git stdout pipe is unavailable"),
            });
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_child(&mut child);
            return Err(DiscoveryError::Spawn {
                command: command_name,
                source: std::io::Error::other("Git stderr pipe is unavailable"),
            });
        }
    };
    let stderr_reader = match thread::Builder::new()
        .name("chakra-git-commit-stderr".to_owned())
        .spawn(move || read_bounded(stderr, MAX_BATCH_STDERR_BYTES))
    {
        Ok(reader) => reader,
        Err(source) => {
            terminate_child(&mut child);
            return Err(DiscoveryError::Spawn {
                command: command_name,
                source,
            });
        }
    };
    let bounded_operation = operation.bounded_by(BATCH_COMMAND_TIMEOUT);
    let reader_operation = bounded_operation.clone();
    let blob_reader = match thread::Builder::new()
        .name("chakra-git-commit-stdout".to_owned())
        .spawn(move || read_blob_batch(stdin, stdout, entries, &reader_operation))
    {
        Ok(reader) => reader,
        Err(source) => {
            terminate_child(&mut child);
            let _ = stderr_reader.join();
            return Err(DiscoveryError::Spawn {
                command: command_name,
                source,
            });
        }
    };
    let mut abort = None;
    let status = loop {
        if let Err(error) = bounded_operation.check() {
            abort = Some(error);
            terminate_child(&mut child);
            break None;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(Ok(status)),
            Ok(None) => thread::sleep(CHILD_POLL_INTERVAL),
            Err(source) => {
                terminate_child(&mut child);
                break Some(Err(source));
            }
        }
    };
    let result = blob_reader.join();
    let stderr = stderr_reader.join();
    let result = result.map_err(|_| DiscoveryError::Spawn {
        command: command_name,
        source: std::io::Error::other("Git commit blob reader panicked"),
    })?;
    let stderr = stderr
        .map_err(|_| DiscoveryError::Spawn {
            command: command_name,
            source: std::io::Error::other("Git stderr reader panicked"),
        })?
        .map_err(|source| DiscoveryError::Spawn {
            command: command_name,
            source,
        })?;
    if let Some(error) = abort {
        return Err(error.into());
    }
    let status = status
        .ok_or_else(|| DiscoveryError::MalformedCommitObject("missing child status".to_owned()))?
        .map_err(|source| DiscoveryError::Spawn {
            command: command_name,
            source,
        })?;
    let values = result?;
    if stderr.exceeded {
        return Err(DiscoveryError::OutputTooLarge {
            command: command_name,
            limit: MAX_BATCH_STDERR_BYTES,
        });
    }
    if !status.success() {
        return Err(DiscoveryError::Git {
            command: command_name,
            status: status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&stderr.bytes).trim().to_owned(),
        });
    }
    Ok(values)
}

fn read_blob_batch(
    mut stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    entries: Vec<TreeSource>,
    operation: &OperationContext,
) -> Result<CommitBlobRead, DiscoveryError> {
    let command_name = "cat-file --batch";
    let mut stdout = BufReader::new(stdout);
    let mut sources = BTreeMap::new();
    let mut source_bytes = 0_u64;
    let mut non_utf8_files = 0_u64;
    for entry in entries {
        operation.check()?;
        writeln!(stdin, "{}", entry.object_id).map_err(|source| DiscoveryError::Spawn {
            command: command_name,
            source,
        })?;
        stdin.flush().map_err(|source| DiscoveryError::Spawn {
            command: command_name,
            source,
        })?;
        let mut header = String::new();
        stdout
            .read_line(&mut header)
            .map_err(|source| DiscoveryError::Spawn {
                command: command_name,
                source,
            })?;
        let fields: Vec<_> = header.split_ascii_whitespace().collect();
        if fields.len() != 3 || fields[0] != entry.object_id || fields[1] != "blob" {
            return Err(DiscoveryError::MalformedCommitObject(format!(
                "unexpected batch header for {}",
                entry.path
            )));
        }
        let size = fields[2].parse::<u64>().map_err(|_| {
            DiscoveryError::MalformedCommitObject(format!(
                "invalid batch blob size for {}",
                entry.path
            ))
        })?;
        if size != entry.size {
            return Err(DiscoveryError::MalformedCommitObject(format!(
                "tree/blob size mismatch for {}",
                entry.path
            )));
        }
        let size = usize::try_from(size).map_err(|_| {
            DiscoveryError::MalformedCommitObject(format!(
                "blob size does not fit memory address space for {}",
                entry.path
            ))
        })?;
        let mut blob = vec![0_u8; size];
        stdout
            .read_exact(&mut blob)
            .map_err(|source| DiscoveryError::Spawn {
                command: command_name,
                source,
            })?;
        let mut terminator = [0_u8; 1];
        stdout
            .read_exact(&mut terminator)
            .map_err(|source| DiscoveryError::Spawn {
                command: command_name,
                source,
            })?;
        if terminator != *b"\n" {
            return Err(DiscoveryError::MalformedCommitObject(format!(
                "missing batch delimiter after {}",
                entry.path
            )));
        }
        let Ok(source) = String::from_utf8(blob) else {
            non_utf8_files = non_utf8_files.saturating_add(1);
            continue;
        };
        source_bytes = source_bytes.saturating_add(source.len() as u64);
        sources.insert(entry.path, Arc::<str>::from(source));
    }
    Ok(CommitBlobRead {
        sources,
        source_bytes,
        non_utf8_files,
    })
}

fn terminate_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn validate_object_id(value: &str) -> Result<(), DiscoveryError> {
    if matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(DiscoveryError::InvalidRootObjectId(value.to_owned()))
    }
}

fn parse_tree_sources(
    output: &[u8],
    operation: &OperationContext,
) -> Result<Vec<TreeSource>, DiscoveryError> {
    let mut sources = Vec::new();
    for record in output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        operation.check()?;
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(DiscoveryError::MalformedCommitObject(
                "malformed ls-tree record".to_owned(),
            ));
        };
        let header = &record[..tab];
        let raw_path = &record[tab.saturating_add(1)..];
        if !raw_may_be_source(raw_path) {
            continue;
        }
        let path = std::str::from_utf8(raw_path).map_err(|_| DiscoveryError::NonUtf8Path)?;
        if source_language(path).is_none() {
            continue;
        }
        let header = std::str::from_utf8(header).map_err(|_| {
            DiscoveryError::MalformedCommitObject("non-UTF-8 ls-tree header".to_owned())
        })?;
        let mut fields = header.split_ascii_whitespace();
        let mode = fields.next().unwrap_or_default();
        let kind = fields.next().unwrap_or_default();
        let object_id = fields.next().unwrap_or_default();
        let size = fields.next().unwrap_or_default();
        if !matches!(mode, "100644" | "100755") || kind != "blob" {
            continue;
        }
        validate_object_id(object_id)?;
        let size = size
            .parse::<u64>()
            .map_err(|_| DiscoveryError::MalformedCommitObject("invalid blob size".to_owned()))?;
        sources.push(TreeSource {
            path: RepoRelativePath::new(path)?,
            object_id: object_id.to_owned(),
            size,
        });
    }
    sources.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn git(root: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
        let output = Command::new("git").current_dir(root).args(args).output()?;
        if !output.status.success() {
            return Err(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
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
        fs::write(repository.path().join("src/lib.rs"), "pub fn base() {}\n")?;
        git(repository.path(), &["add", "src/lib.rs"])?;
        git(repository.path(), &["commit", "--quiet", "-m", "base"])?;
        Ok(repository)
    }

    fn limits() -> CommitSnapshotLimits {
        CommitSnapshotLimits {
            max_files: 100,
            max_source_file_bytes: 1024 * 1024,
            max_workspace_source_bytes: 16 * 1024 * 1024,
        }
    }

    #[test]
    fn head_snapshot_ignores_dirty_deleted_and_untracked_worktree_state()
    -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        let head = git(root, &["rev-parse", "HEAD"])?;
        fs::write(root.join("src/lib.rs"), "pub fn dirty() {}\n")?;
        fs::write(root.join("src/untracked.rs"), "pub fn untracked() {}\n")?;

        let snapshot =
            read_head_commit_snapshot_with_context(root, limits(), &OperationContext::unbounded())?;
        assert_eq!(snapshot.commit.as_deref(), Some(head.as_str()));
        assert_eq!(snapshot.sources.len(), 1);
        assert_eq!(
            snapshot.sources[&RepoRelativePath::new("src/lib.rs")?].as_ref(),
            "pub fn base() {}\n"
        );
        assert!(
            !snapshot
                .sources
                .contains_key(&RepoRelativePath::new("src/untracked.rs")?)
        );

        fs::remove_file(root.join("src/lib.rs"))?;
        let deleted =
            read_head_commit_snapshot_with_context(root, limits(), &OperationContext::unbounded())?;
        assert_eq!(deleted.sources.len(), 1);
        Ok(())
    }

    #[test]
    fn explicit_commit_remains_stable_after_head_moves() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        let base = git(root, &["rev-parse", "HEAD"])?;
        fs::write(root.join("src/lib.rs"), "pub fn second() {}\n")?;
        git(root, &["add", "src/lib.rs"])?;
        git(root, &["commit", "--quiet", "-m", "second"])?;

        let snapshot = read_commit_snapshot_with_context(
            root,
            Some(&base),
            limits(),
            &OperationContext::unbounded(),
        )?;
        assert_eq!(snapshot.commit.as_deref(), Some(base.as_str()));
        assert_eq!(
            snapshot.sources[&RepoRelativePath::new("src/lib.rs")?].as_ref(),
            "pub fn base() {}\n"
        );
        Ok(())
    }

    #[test]
    fn unborn_repository_has_an_empty_commit_layer() -> Result<(), Box<dyn Error>> {
        let repository = TempDir::new()?;
        git(repository.path(), &["init", "--quiet"])?;
        let snapshot = read_head_commit_snapshot_with_context(
            repository.path(),
            limits(),
            &OperationContext::unbounded(),
        )?;
        assert_eq!(snapshot.commit, None);
        assert!(snapshot.sources.is_empty());
        Ok(())
    }
}
