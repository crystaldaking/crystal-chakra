//! Git-backed source discovery and current worktree change adapter.
//!
//! The adapter asks Git to compare a resolved commit baseline with the final
//! materialized worktree and adds untracked, non-ignored supported source
//! files. It never constructs or inspects an administrative Git path, and
//! repository-controlled paths are passed as data rather than through a shell.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{ChangeKind, DiffScope, ResolvedDiffScope};
use chakra_engine::{
    DiffWorkspace, WorkspaceDiff, WorkspaceDiffError, WorkspaceDiffProvider, WorkspaceFileChange,
};

mod discovery;
mod source_metadata;

pub use discovery::{
    DiscoveryError, WorkspaceInventory, discover_language_files, discover_source_files,
    discover_source_files_in_worktree, discover_source_files_in_worktree_with_context,
    discover_workspace_inventory_in_worktree_with_context, resolve_git_administrative_paths,
    resolve_repository_identity, resolve_repository_root, resolve_repository_root_with_context,
    resolve_workspace_identity, source_language,
};
pub use source_metadata::{
    ClassifiedSource, classify_discovered_sources_with_context, discover_classified_sources,
};

const MAX_GIT_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_WORKSPACE_CHANGES: usize = 10_000;
const MAX_ERROR_CHARS: usize = 1_024;
const MAX_GIT_STDERR_BYTES: usize = 16 * 1024;
const MAX_GIT_REFERENCE_CHARS: usize = 1_024;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Fixed-argument Git implementation for the active materialized worktree.
#[derive(Debug, Default)]
pub struct GitWorkspaceDiff;

#[derive(Debug)]
struct ResolvedBaseline {
    public: ResolvedDiffScope,
    head_commit: Option<String>,
    reference_commit: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct GitInventory {
    tracked: Vec<u8>,
    index: Vec<u8>,
    untracked: Vec<u8>,
}

#[derive(Debug)]
struct BoundedRead {
    bytes: Vec<u8>,
    exceeded: bool,
}

#[derive(Debug)]
struct GitOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_exceeded: bool,
}

fn bounded_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim()
        .chars()
        .take(MAX_ERROR_CHARS)
        .collect()
}

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<BoundedRead> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut exceeded = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retained = remaining.min(read);
        bytes.extend_from_slice(&buffer[..retained]);
        exceeded |= retained < read;
    }
    Ok(BoundedRead { bytes, exceeded })
}

fn terminate_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn capture_git(
    root: &Path,
    display: &'static str,
    args: &[&OsStr],
    operation: &OperationContext,
) -> Result<GitOutput, WorkspaceDiffError> {
    capture_command(OsStr::new("git"), root, display, args, operation)
}

fn capture_command(
    executable: &OsStr,
    root: &Path,
    display: &'static str,
    args: &[&OsStr],
    operation: &OperationContext,
) -> Result<GitOutput, WorkspaceDiffError> {
    let operation = operation.bounded_by(GIT_COMMAND_TIMEOUT);
    operation
        .check()
        .map_err(|error| WorkspaceDiffError::new(error.to_string()))?;
    let mut child = Command::new(executable)
        .current_dir(root)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args)
        .spawn()
        .map_err(|error| {
            WorkspaceDiffError::new(format!("failed to execute `{display}`: {error}"))
        })?;
    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return Err(WorkspaceDiffError::new(format!(
            "failed to execute `{display}`: Git stdout pipe is unavailable"
        )));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_child(&mut child);
        return Err(WorkspaceDiffError::new(format!(
            "failed to execute `{display}`: Git stderr pipe is unavailable"
        )));
    };
    let stdout_reader = match thread::Builder::new()
        .name("chakra-git-diff-stdout".to_owned())
        .spawn(move || read_bounded(stdout, MAX_GIT_OUTPUT_BYTES))
    {
        Ok(reader) => reader,
        Err(error) => {
            terminate_child(&mut child);
            return Err(WorkspaceDiffError::new(format!(
                "failed to execute `{display}`: {error}"
            )));
        }
    };
    let stderr_reader = match thread::Builder::new()
        .name("chakra-git-diff-stderr".to_owned())
        .spawn(move || read_bounded(stderr, MAX_GIT_STDERR_BYTES))
    {
        Ok(reader) => reader,
        Err(error) => {
            terminate_child(&mut child);
            let _ = stdout_reader.join();
            return Err(WorkspaceDiffError::new(format!(
                "failed to start `{display}` stderr reader: {error}"
            )));
        }
    };
    let status = loop {
        if let Err(error) = operation.check() {
            terminate_child(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(WorkspaceDiffError::new(error.to_string()));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(CHILD_POLL_INTERVAL),
            Err(error) => {
                terminate_child(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(WorkspaceDiffError::new(format!(
                    "failed to wait for `{display}`: {error}"
                )));
            }
        }
    };
    // Join both pipe owners before propagating either result. Returning after
    // the first failed join would detach the other reader thread.
    let stdout = stdout_reader.join();
    let stderr = stderr_reader.join();
    let stdout = stdout
        .map_err(|_| WorkspaceDiffError::new(format!("`{display}` stdout reader panicked")))?
        .map_err(|error| {
            WorkspaceDiffError::new(format!("failed to read `{display}` stdout: {error}"))
        })?;
    let stderr = stderr
        .map_err(|_| WorkspaceDiffError::new(format!("`{display}` stderr reader panicked")))?
        .map_err(|error| {
            WorkspaceDiffError::new(format!("failed to read `{display}` stderr: {error}"))
        })?;
    Ok(GitOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_exceeded: stdout.exceeded,
    })
}

fn git_output(
    root: &Path,
    display: &'static str,
    args: &[&OsStr],
    operation: &OperationContext,
) -> Result<GitOutput, WorkspaceDiffError> {
    let output = capture_git(root, display, args, operation)?;
    if !output.status.success() {
        return Err(WorkspaceDiffError::new(format!(
            "`{display}` exited with status {}: {}",
            output.status.code().unwrap_or(-1),
            bounded_text(&output.stderr)
        )));
    }
    if output.stdout_exceeded {
        return Err(WorkspaceDiffError::new(format!(
            "`{display}` output exceeded the {MAX_GIT_OUTPUT_BYTES}-byte safety budget"
        )));
    }
    Ok(output)
}

fn ensure_worktree(root: &Path, operation: &OperationContext) -> Result<(), WorkspaceDiffError> {
    let worktree = git_output(
        root,
        "git rev-parse --is-inside-work-tree",
        &[OsStr::new("rev-parse"), OsStr::new("--is-inside-work-tree")],
        operation,
    )?;
    if worktree.stdout != b"true\n" {
        return Err(WorkspaceDiffError::new(
            "repository root is not inside a Git worktree",
        ));
    }
    Ok(())
}

fn parse_object_id(output: &[u8], operation: &str) -> Result<String, WorkspaceDiffError> {
    let output = std::str::from_utf8(output)
        .map_err(|_| WorkspaceDiffError::new(format!("{operation} returned non-UTF-8 output")))?;
    let mut lines = output.lines().filter(|line| !line.is_empty());
    let object_id = lines
        .next()
        .ok_or_else(|| WorkspaceDiffError::new(format!("{operation} returned no commit")))?;
    if lines.next().is_some() {
        return Err(WorkspaceDiffError::new(format!(
            "{operation} returned more than one commit"
        )));
    }
    if !object_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(WorkspaceDiffError::new(format!(
            "{operation} returned an invalid object id"
        )));
    }
    Ok(object_id.to_owned())
}

fn resolve_head(
    root: &Path,
    operation: &OperationContext,
) -> Result<Option<String>, WorkspaceDiffError> {
    let output = capture_git(
        root,
        "git rev-parse HEAD",
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new("HEAD"),
        ],
        operation,
    )?;
    if output.stdout_exceeded {
        return Err(WorkspaceDiffError::new(format!(
            "`git rev-parse HEAD` output exceeded the {MAX_GIT_OUTPUT_BYTES}-byte safety budget"
        )));
    }
    if output.status.success() {
        parse_object_id(&output.stdout, "git rev-parse HEAD").map(Some)
    } else if output.stdout.is_empty() && output.stderr.is_empty() {
        Ok(None)
    } else {
        Err(WorkspaceDiffError::new(format!(
            "`git rev-parse HEAD` exited with status {}: {}",
            output.status.code().unwrap_or(-1),
            bounded_text(&output.stderr)
        )))
    }
}

fn validate_reference(reference: &str) -> Result<(), WorkspaceDiffError> {
    if reference.trim().is_empty() {
        return Err(WorkspaceDiffError::new(
            "Git base reference must not be empty",
        ));
    }
    if reference.chars().count() > MAX_GIT_REFERENCE_CHARS {
        return Err(WorkspaceDiffError::new(format!(
            "Git base reference exceeds the {MAX_GIT_REFERENCE_CHARS}-character request budget"
        )));
    }
    if reference.chars().any(char::is_control) {
        return Err(WorkspaceDiffError::new(
            "Git base reference must not contain control characters",
        ));
    }
    Ok(())
}

fn resolve_commit(
    root: &Path,
    reference: &str,
    operation: &OperationContext,
) -> Result<String, WorkspaceDiffError> {
    validate_reference(reference)?;
    let peeled = format!("{reference}^{{commit}}");
    let output = capture_git(
        root,
        "git rev-parse base reference",
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--end-of-options"),
            OsStr::new(&peeled),
        ],
        operation,
    )?;
    if output.stdout_exceeded {
        return Err(WorkspaceDiffError::new(format!(
            "`git rev-parse base reference` output exceeded the {MAX_GIT_OUTPUT_BYTES}-byte safety budget"
        )));
    }
    if !output.status.success() {
        return Err(WorkspaceDiffError::new(format!(
            "invalid Git base reference `{reference}`: {}",
            bounded_text(&output.stderr)
        )));
    }
    if !output.stderr.is_empty() {
        let diagnostics = bounded_text(&output.stderr);
        if diagnostics.contains("ambiguous") {
            return Err(WorkspaceDiffError::new(format!(
                "ambiguous Git base reference `{reference}`: {diagnostics}"
            )));
        }
        return Err(WorkspaceDiffError::new(format!(
            "Git base reference `{reference}` produced diagnostics: {diagnostics}"
        )));
    }
    parse_object_id(&output.stdout, "git rev-parse base reference")
}

fn resolve_merge_base(
    root: &Path,
    head: &str,
    reference_commit: &str,
    operation: &OperationContext,
) -> Result<String, WorkspaceDiffError> {
    let output = capture_git(
        root,
        "git merge-base --all",
        &[
            OsStr::new("merge-base"),
            OsStr::new("--all"),
            OsStr::new(head),
            OsStr::new(reference_commit),
        ],
        operation,
    )?;
    if output.stdout_exceeded {
        return Err(WorkspaceDiffError::new(format!(
            "`git merge-base --all` output exceeded the {MAX_GIT_OUTPUT_BYTES}-byte safety budget"
        )));
    }
    if !output.status.success() {
        return Err(WorkspaceDiffError::new(format!(
            "Git base reference and HEAD do not have a merge base: {}",
            bounded_text(&output.stderr)
        )));
    }
    parse_object_id(&output.stdout, "git merge-base --all").map_err(|error| {
        WorkspaceDiffError::new(format!(
            "Git base reference and HEAD do not have a unique merge base: {error}"
        ))
    })
}

fn resolve_baseline(
    root: &Path,
    scope: DiffScope,
    operation: &OperationContext,
) -> Result<ResolvedBaseline, WorkspaceDiffError> {
    ensure_worktree(root, operation)?;
    match scope {
        DiffScope::Worktree => {
            let head_commit = resolve_head(root, operation)?;
            Ok(ResolvedBaseline {
                public: ResolvedDiffScope {
                    requested: DiffScope::Worktree,
                    base_commit: head_commit.clone(),
                },
                head_commit,
                reference_commit: None,
            })
        }
        DiffScope::BaseRef { reference } => {
            let reference_commit = resolve_commit(root, &reference, operation)?;
            Ok(ResolvedBaseline {
                public: ResolvedDiffScope {
                    requested: DiffScope::BaseRef { reference },
                    base_commit: Some(reference_commit.clone()),
                },
                head_commit: None,
                reference_commit: Some(reference_commit),
            })
        }
        DiffScope::MergeBase { reference } => {
            let head_commit = resolve_head(root, operation)?.ok_or_else(|| {
                WorkspaceDiffError::new(
                    "merge-base diff scope requires a repository with a HEAD commit",
                )
            })?;
            let reference_commit = resolve_commit(root, &reference, operation)?;
            let base_commit = resolve_merge_base(root, &head_commit, &reference_commit, operation)?;
            Ok(ResolvedBaseline {
                public: ResolvedDiffScope {
                    requested: DiffScope::MergeBase { reference },
                    base_commit: Some(base_commit),
                },
                head_commit: Some(head_commit),
                reference_commit: Some(reference_commit),
            })
        }
    }
}

fn verify_baseline(
    root: &Path,
    baseline: &ResolvedBaseline,
    operation: &OperationContext,
) -> Result<(), WorkspaceDiffError> {
    match &baseline.public.requested {
        DiffScope::Worktree => {
            if resolve_head(root, operation)? != baseline.head_commit {
                return Err(WorkspaceDiffError::new(
                    "HEAD changed while Git diff state was being read",
                ));
            }
        }
        DiffScope::BaseRef { reference } => {
            if Some(resolve_commit(root, reference, operation)?) != baseline.reference_commit {
                return Err(WorkspaceDiffError::new(format!(
                    "Git base reference `{reference}` changed while diff state was being read"
                )));
            }
        }
        DiffScope::MergeBase { reference } => {
            if resolve_head(root, operation)? != baseline.head_commit
                || Some(resolve_commit(root, reference, operation)?) != baseline.reference_commit
            {
                return Err(WorkspaceDiffError::new(format!(
                    "HEAD or Git base reference `{reference}` changed while diff state was being read"
                )));
            }
        }
    }
    Ok(())
}

fn read_git_inventory(
    root: &Path,
    base_commit: &str,
    operation: &OperationContext,
) -> Result<GitInventory, WorkspaceDiffError> {
    let tracked = git_output(
        root,
        "git diff --name-status base commit",
        &[
            OsStr::new("diff"),
            OsStr::new("--name-status"),
            OsStr::new("-z"),
            OsStr::new("--find-renames"),
            OsStr::new("--no-ext-diff"),
            OsStr::new("--ignore-submodules=all"),
            OsStr::new(base_commit),
            OsStr::new("--"),
        ],
        operation,
    )?;
    let index = git_output(
        root,
        "git ls-files --stage -v",
        &[
            OsStr::new("ls-files"),
            OsStr::new("--stage"),
            OsStr::new("-v"),
            OsStr::new("-z"),
        ],
        operation,
    )?;
    let untracked = git_output(
        root,
        "git ls-files --others --exclude-standard",
        &[
            OsStr::new("ls-files"),
            OsStr::new("--others"),
            OsStr::new("--exclude-standard"),
            OsStr::new("-z"),
        ],
        operation,
    )?;
    Ok(GitInventory {
        tracked: tracked.stdout,
        index: index.stdout,
        untracked: untracked.stdout,
    })
}

fn is_supported_source(path: &str) -> bool {
    source_language(path).is_some()
}

fn raw_is_supported_source(raw: &[u8]) -> Result<bool, WorkspaceDiffError> {
    let looks_supported = raw.ends_with(b".rs")
        || raw.ends_with(b".php")
        || raw.ends_with(b".ts")
        || raw.ends_with(b".tsx")
        || raw.ends_with(b".mts")
        || raw.ends_with(b".cts")
        || raw.ends_with(b".py")
        || raw.ends_with(b".pyi");
    if !looks_supported {
        return Ok(false);
    }
    let path = std::str::from_utf8(raw)
        .map_err(|_| WorkspaceDiffError::new("Git returned a non-UTF-8 source path"))?;
    Ok(is_supported_source(path))
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
    operation: &OperationContext,
) -> Result<(BTreeMap<RepoRelativePath, WorkspaceFileChange>, bool), WorkspaceDiffError> {
    let mut fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut changes = BTreeMap::new();

    while let Some(raw_status) = fields.next() {
        operation
            .check()
            .map_err(|error| WorkspaceDiffError::new(error.to_string()))?;
        if changes.len() > MAX_WORKSPACE_CHANGES {
            return Ok((changes, true));
        }
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
            let old_is_source = raw_is_supported_source(old)?;
            let new_is_source = raw_is_supported_source(new)?;
            match (old_is_source, new_is_source, status) {
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
        if !raw_is_supported_source(raw_path)? {
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
    Ok((changes, false))
}

fn add_untracked_changes(
    root: &Path,
    base_commit: &str,
    output: &[u8],
    document_paths: &HashSet<&RepoRelativePath>,
    changes: &mut BTreeMap<RepoRelativePath, WorkspaceFileChange>,
    operation: &OperationContext,
) -> Result<(), WorkspaceDiffError> {
    for raw_path in output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
    {
        operation
            .check()
            .map_err(|error| WorkspaceDiffError::new(error.to_string()))?;
        if changes.len() > MAX_WORKSPACE_CHANGES {
            break;
        }
        if !raw_is_supported_source(raw_path)? {
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
            if base_blob_id(root, base_commit, &path, operation)?
                == worktree_blob_id(root, &path, operation)?
            {
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

fn add_index_hidden_changes(
    root: &Path,
    base_commit: &str,
    output: &[u8],
    document_paths: &HashSet<&RepoRelativePath>,
    changes: &mut BTreeMap<RepoRelativePath, WorkspaceFileChange>,
    operation: &OperationContext,
) -> Result<(), WorkspaceDiffError> {
    for record in output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        operation
            .check()
            .map_err(|error| WorkspaceDiffError::new(error.to_string()))?;
        if changes.len() > MAX_WORKSPACE_CHANGES {
            break;
        }
        let Some((&tag, raw_path)) = record.split_first() else {
            continue;
        };
        let raw_path = if let Some(tab) = raw_path.iter().position(|byte| *byte == b'\t') {
            &raw_path[tab + 1..]
        } else {
            raw_path.strip_prefix(b" ").ok_or_else(|| {
                WorkspaceDiffError::new("Git returned an invalid ls-files status record")
            })?
        };
        // `git ls-files --stage -v` lowercases the normal tag for
        // assume-unchanged entries; `S` denotes skip-worktree. Both can
        // suppress ordinary `git diff` inspection even when a regular file
        // is materialized. The stage/object fields also make the retained
        // inventory sensitive to concurrent index changes.
        if !(tag.is_ascii_lowercase() || tag == b'S') || !raw_is_supported_source(raw_path)? {
            continue;
        }
        let path = parse_path(raw_path)?;
        if changes.contains_key(&path) || !document_paths.contains(&path) {
            continue;
        }
        if base_blob_id(root, base_commit, &path, operation)?
            != worktree_blob_id(root, &path, operation)?
        {
            insert_change(
                changes,
                path,
                None,
                ChangeKind::Modified,
                Precision::Precise,
            );
        }
    }
    Ok(())
}

fn base_entry(
    root: &Path,
    base_commit: &str,
    path: &RepoRelativePath,
    operation: &OperationContext,
) -> Result<Option<(String, String)>, WorkspaceDiffError> {
    let output = git_output(
        root,
        "git ls-tree base commit",
        &[
            OsStr::new("ls-tree"),
            OsStr::new("-z"),
            OsStr::new(base_commit),
            OsStr::new("--"),
            OsStr::new(path.as_str()),
        ],
        operation,
    )?;
    let Some(record) = output
        .stdout
        .split(|byte| *byte == 0)
        .find(|field| !field.is_empty())
    else {
        return Ok(None);
    };
    let metadata = record
        .split(|byte| *byte == b'\t')
        .next()
        .ok_or_else(|| WorkspaceDiffError::new("Git returned an invalid ls-tree record"))?;
    let metadata = std::str::from_utf8(metadata)
        .map_err(|_| WorkspaceDiffError::new("Git returned a non-UTF-8 object id"))?;
    let mut fields = metadata.split_whitespace();
    let mode = fields
        .next()
        .ok_or_else(|| WorkspaceDiffError::new("Git returned an invalid ls-tree mode"))?;
    let _kind = fields
        .next()
        .ok_or_else(|| WorkspaceDiffError::new("Git returned an invalid ls-tree kind"))?;
    let object_id = fields
        .next()
        .ok_or_else(|| WorkspaceDiffError::new("Git returned an invalid ls-tree object id"))?;
    Ok(Some((mode.to_owned(), object_id.to_owned())))
}

fn base_blob_id(
    root: &Path,
    base_commit: &str,
    path: &RepoRelativePath,
    operation: &OperationContext,
) -> Result<String, WorkspaceDiffError> {
    base_entry(root, base_commit, path, operation)?
        .map(|(_, object_id)| object_id)
        .ok_or_else(|| WorkspaceDiffError::new(format!("diff baseline has no blob for `{path}`")))
}

fn worktree_blob_id(
    root: &Path,
    path: &RepoRelativePath,
    operation: &OperationContext,
) -> Result<String, WorkspaceDiffError> {
    let output = git_output(
        root,
        "git hash-object",
        &[
            OsStr::new("hash-object"),
            OsStr::new("--"),
            OsStr::new(path.as_str()),
        ],
        operation,
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
    operation: &OperationContext,
) -> Result<(), WorkspaceDiffError> {
    let documents: HashMap<_, _> = workspace
        .documents
        .iter()
        .map(|document| (&document.path, document.source.as_ref()))
        .collect();
    for change in changes.values() {
        operation
            .check()
            .map_err(|error| WorkspaceDiffError::new(error.to_string()))?;
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
        operation
            .check()
            .map_err(|error| WorkspaceDiffError::new(error.to_string()))?;
        if materialized != *snapshot_source {
            return Err(WorkspaceDiffError::new(format!(
                "materialized source `{}` changed after syntax revision {} was published",
                change.path, workspace.revision
            )));
        }
    }
    Ok(())
}

fn validate_deleted_sources(
    workspace: &DiffWorkspace,
    base_commit: &str,
    changes: &mut BTreeMap<RepoRelativePath, WorkspaceFileChange>,
    operation: &OperationContext,
) -> Result<(), WorkspaceDiffError> {
    let deleted: Vec<_> = changes
        .values()
        .filter(|change| change.change == ChangeKind::Deleted)
        .map(|change| change.path.clone())
        .collect();
    for path in deleted {
        operation
            .check()
            .map_err(|error| WorkspaceDiffError::new(error.to_string()))?;
        // A tracked symlink (or submodule) with an `.rs` suffix can appear in
        // Git's name diff, but syntax discovery never indexed it as a source.
        if !base_entry(&workspace.repository_root, base_commit, &path, operation)?
            .is_some_and(|(mode, _)| mode == "100644" || mode == "100755")
        {
            changes.remove(&path);
            continue;
        }
        match fs::symlink_metadata(workspace.repository_root.join(path.as_str())) {
            Ok(_) => {
                return Err(WorkspaceDiffError::new(format!(
                    "materialized path `{path}` reappeared after syntax revision {} was published",
                    workspace.revision
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(WorkspaceDiffError::new(format!(
                    "failed to verify deleted path `{path}`: {error}"
                )));
            }
        }
    }
    Ok(())
}

impl WorkspaceDiffProvider for GitWorkspaceDiff {
    fn diff(&self, workspace: DiffWorkspace) -> Result<WorkspaceDiff, WorkspaceDiffError> {
        self.diff_with_context(workspace, &OperationContext::unbounded())
    }

    fn diff_with_context(
        &self,
        workspace: DiffWorkspace,
        operation: &OperationContext,
    ) -> Result<WorkspaceDiff, WorkspaceDiffError> {
        operation
            .check()
            .map_err(|error| WorkspaceDiffError::new(error.to_string()))?;
        let document_paths: HashSet<_> = workspace
            .documents
            .iter()
            .map(|document| &document.path)
            .collect();
        let baseline = resolve_baseline(
            &workspace.repository_root,
            workspace.scope.clone(),
            operation,
        )?;
        let mut inventory = None;
        let (mut changes, mut work_truncated) = if let Some(base_commit) =
            baseline.public.base_commit.as_deref()
        {
            let captured = read_git_inventory(&workspace.repository_root, base_commit, operation)?;
            let changes = parse_tracked_changes(&captured.tracked, operation)?;
            inventory = Some(captured);
            changes
        } else {
            let mut unborn = BTreeMap::new();
            for document in &workspace.documents {
                if is_supported_source(document.path.as_str()) {
                    insert_change(
                        &mut unborn,
                        document.path.clone(),
                        None,
                        ChangeKind::Added,
                        Precision::Precise,
                    );
                }
            }
            let truncated = unborn.len() > MAX_WORKSPACE_CHANGES;
            (unborn, truncated)
        };

        if let Some(base_commit) = baseline.public.base_commit.as_deref() {
            let captured = inventory.as_ref().ok_or_else(|| {
                WorkspaceDiffError::new("Git inventory was not captured for the resolved baseline")
            })?;
            add_index_hidden_changes(
                &workspace.repository_root,
                base_commit,
                &captured.index,
                &document_paths,
                &mut changes,
                operation,
            )?;
            add_untracked_changes(
                &workspace.repository_root,
                base_commit,
                &captured.untracked,
                &document_paths,
                &mut changes,
                operation,
            )?;
        }

        // Current files must belong to the exact syntax snapshot. This also
        // keeps skipped symlinks and a newer, not-yet-reconciled file out of
        // the joined result. Deleted paths intentionally have no document.
        changes.retain(|_, change| {
            change.change == ChangeKind::Deleted || document_paths.contains(&change.path)
        });
        if let Some(base_commit) = baseline.public.base_commit.as_deref() {
            validate_deleted_sources(&workspace, base_commit, &mut changes, operation)?;
        }
        validate_current_sources(&workspace, &changes, operation)?;
        if let (Some(base_commit), Some(captured)) =
            (baseline.public.base_commit.as_deref(), inventory.as_ref())
            && read_git_inventory(&workspace.repository_root, base_commit, operation)? != *captured
        {
            return Err(WorkspaceDiffError::new(
                "Git index or materialized change inventory changed while diff state was being read",
            ));
        }
        verify_baseline(&workspace.repository_root, &baseline, operation)?;
        work_truncated |= changes.len() > MAX_WORKSPACE_CHANGES;
        let files = changes.into_values().take(MAX_WORKSPACE_CHANGES).collect();
        let truncation = work_truncated.then_some(chakra_engine::DiffInventoryTruncation {
            limit: MAX_WORKSPACE_CHANGES,
            omitted: None,
        });
        Ok(WorkspaceDiff {
            revision: workspace.revision,
            scope: baseline.public,
            files,
            truncation,
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
    #[cfg(unix)]
    use std::sync::mpsc;
    #[cfg(unix)]
    use std::time::Instant;

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
            scope: DiffScope::Worktree,
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
        assert!(diff.truncation.is_none());
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

    #[test]
    fn materialized_assume_unchanged_file_is_still_reported() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        git(
            root,
            &["update-index", "--assume-unchanged", "src/unstaged.rs"],
        )?;
        write(root, "src/unstaged.rs", "pub fn hidden_edit() {}\n")?;
        let workspace = workspace(root, &["src/unstaged.rs"])?;

        let diff = GitWorkspaceDiff.diff(workspace)?;
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path.as_str(), "src/unstaged.rs");
        assert_eq!(diff.files[0].change, ChangeKind::Modified);
        assert_eq!(diff.files[0].precision, Precision::Precise);
        Ok(())
    }

    #[test]
    fn a_deleted_path_that_reappeared_is_not_joined_to_an_older_snapshot()
    -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let path = RepoRelativePath::new("src/deleted.rs")?;
        let mut changes = BTreeMap::from([(
            path.clone(),
            WorkspaceFileChange {
                path,
                previous_path: None,
                change: ChangeKind::Deleted,
                provenance: Provenance::Git,
                precision: Precision::Precise,
            },
        )]);
        let snapshot = workspace(repository.path(), &[])?;

        let operation = OperationContext::unbounded();
        let base_commit =
            resolve_head(repository.path(), &operation)?.ok_or("HEAD commit missing")?;
        let error =
            match validate_deleted_sources(&snapshot, &base_commit, &mut changes, &operation) {
                Ok(()) => return Err("reappeared path was accepted as deleted".into()),
                Err(error) => error,
            };
        assert!(error.to_string().contains("reappeared"));
        Ok(())
    }

    #[test]
    fn a_base_ref_that_moves_during_the_read_is_rejected() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        let operation = OperationContext::unbounded();
        git(root, &["branch", "moving-base"])?;
        let baseline = resolve_baseline(
            root,
            DiffScope::BaseRef {
                reference: "moving-base".to_owned(),
            },
            &operation,
        )?;

        write(root, "src/new_commit.rs", "pub fn later() {}\n")?;
        git(root, &["add", "src/new_commit.rs"])?;
        git(root, &["commit", "--quiet", "-m", "move target"])?;
        git(root, &["branch", "--force", "moving-base", "HEAD"])?;

        let error = verify_baseline(root, &baseline, &operation)
            .err()
            .ok_or("moved base reference was accepted")?;
        assert!(error.to_string().contains("changed while diff state"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_deleted_tracked_symlink_is_not_reported_as_a_source() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let repository = repository()?;
        let root = repository.path();
        symlink("unstaged.rs", root.join("src/linked.rs"))?;
        git(root, &["add", "src/linked.rs"])?;
        git(root, &["commit", "--quiet", "-m", "track symlink"])?;
        fs::remove_file(root.join("src/linked.rs"))?;

        let diff = GitWorkspaceDiff.diff(workspace(root, &[])?)?;
        assert!(diff.files.is_empty());
        Ok(())
    }

    #[test]
    fn bounded_reader_drains_but_retains_only_the_budget() -> Result<(), Box<dyn Error>> {
        let mut input = std::io::Cursor::new(b"0123456789");
        let captured = read_bounded(&mut input, 4)?;
        assert_eq!(captured.bytes, b"0123");
        assert!(captured.exceeded);
        assert_eq!(input.position(), 10);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_and_reaps_an_owned_process() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new()?;
        let executable = directory.path().join("fake-git");
        let marker = directory.path().join("pid");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$1\"\nwhile :; do :; done\n",
        )?;
        let mut permissions = fs::metadata(&executable)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions)?;

        let operation = OperationContext::unbounded();
        let worker_operation = operation.clone();
        let worker_executable = executable.clone();
        let worker_marker = marker.clone();
        let (completed, result) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let response = capture_command(
                worker_executable.as_os_str(),
                directory.path(),
                "fake cancellable Git",
                &[worker_marker.as_os_str()],
                &worker_operation,
            );
            let _ = completed.send(response);
        });

        let marker_deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            if Instant::now() >= marker_deadline {
                return Err("fake Git process did not start".into());
            }
            std::thread::yield_now();
        }
        let pid = fs::read_to_string(&marker)?;
        operation.cancel();
        let response = result
            .recv_timeout(Duration::from_millis(250))
            .map_err(|_| "cancelled Git process was not reaped promptly")?;
        let error = match response {
            Ok(_) => return Err("cancelled Git process unexpectedly succeeded".into()),
            Err(error) => error,
        };
        assert!(error.to_string().contains("cancelled"));
        worker.join().map_err(|_| "Git worker thread panicked")?;

        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        assert!(!alive.success(), "cancelled Git child still exists");
        Ok(())
    }

    #[test]
    fn unrelated_non_utf8_file_does_not_break_rust_diff() -> Result<(), Box<dyn Error>> {
        let mut changes = BTreeMap::new();
        add_untracked_changes(
            Path::new("."),
            "HEAD",
            b"unrelated-\xff.bin\0",
            &HashSet::new(),
            &mut changes,
            &OperationContext::unbounded(),
        )?;
        assert!(changes.is_empty());
        Ok(())
    }

    #[test]
    fn php_modify_untracked_and_delete_use_the_same_diff_scope() -> Result<(), Box<dyn Error>> {
        let repository = TempDir::new()?;
        let root = repository.path();
        git(root, &["init", "--quiet"])?;
        git(root, &["config", "user.email", "tests@example.invalid"])?;
        git(root, &["config", "user.name", "Chakra Tests"])?;
        write(root, "src/service.php", "<?php function pay() {}\n")?;
        write(root, "src/deleted.php", "<?php function removed() {}\n")?;
        git(root, &["add", "src"])?;
        git(root, &["commit", "--quiet", "-m", "base"])?;

        write(root, "src/service.php", "<?php function payNow() {}\n")?;
        fs::remove_file(root.join("src/deleted.php"))?;
        write(root, "src/untracked.php", "<?php function added() {}\n")?;
        let workspace = workspace(root, &["src/service.php", "src/untracked.php"])?;
        let diff = GitWorkspaceDiff.diff(workspace)?;
        let changes: BTreeMap<_, _> = diff
            .files
            .iter()
            .map(|change| (change.path.as_str(), change.change))
            .collect();
        assert_eq!(changes["src/service.php"], ChangeKind::Modified);
        assert_eq!(changes["src/deleted.php"], ChangeKind::Deleted);
        assert_eq!(changes["src/untracked.php"], ChangeKind::Added);
        Ok(())
    }

    #[test]
    fn typescript_modify_untracked_and_delete_use_the_same_diff_scope() -> Result<(), Box<dyn Error>>
    {
        let repository = TempDir::new()?;
        let root = repository.path();
        git(root, &["init", "--quiet"])?;
        git(root, &["config", "user.email", "tests@example.invalid"])?;
        git(root, &["config", "user.name", "Chakra Tests"])?;
        write(root, "src/service.ts", "export function pay(): void {}\n")?;
        write(
            root,
            "src/deleted.tsx",
            "export function removed() { return null; }\n",
        )?;
        git(root, &["add", "src"])?;
        git(root, &["commit", "--quiet", "-m", "base"])?;

        write(
            root,
            "src/service.ts",
            "export function payNow(): void {}\n",
        )?;
        fs::remove_file(root.join("src/deleted.tsx"))?;
        write(
            root,
            "src/untracked.mts",
            "export function added(): void {}\n",
        )?;
        let workspace = workspace(root, &["src/service.ts", "src/untracked.mts"])?;
        let diff = GitWorkspaceDiff.diff(workspace)?;
        let changes: BTreeMap<_, _> = diff
            .files
            .iter()
            .map(|change| (change.path.as_str(), change.change))
            .collect();
        assert_eq!(changes["src/service.ts"], ChangeKind::Modified);
        assert_eq!(changes["src/deleted.tsx"], ChangeKind::Deleted);
        assert_eq!(changes["src/untracked.mts"], ChangeKind::Added);
        Ok(())
    }

    #[test]
    fn python_modify_untracked_and_delete_use_the_same_diff_scope() -> Result<(), Box<dyn Error>> {
        let repository = TempDir::new()?;
        let root = repository.path();
        git(root, &["init", "--quiet"])?;
        git(root, &["config", "user.email", "tests@example.invalid"])?;
        git(root, &["config", "user.name", "Chakra Tests"])?;
        write(root, "src/service.py", "def pay():\n    pass\n")?;
        write(root, "src/deleted.pyi", "def removed() -> None: ...\n")?;
        git(root, &["add", "src"])?;
        git(root, &["commit", "--quiet", "-m", "base"])?;

        write(root, "src/service.py", "def pay_now():\n    pass\n")?;
        fs::remove_file(root.join("src/deleted.pyi"))?;
        write(root, "src/untracked.py", "def added():\n    pass\n")?;
        let workspace = workspace(root, &["src/service.py", "src/untracked.py"])?;
        let diff = GitWorkspaceDiff.diff(workspace)?;
        let changes: BTreeMap<_, _> = diff
            .files
            .iter()
            .map(|change| (change.path.as_str(), change.change))
            .collect();
        assert_eq!(changes["src/service.py"], ChangeKind::Modified);
        assert_eq!(changes["src/deleted.pyi"], ChangeKind::Deleted);
        assert_eq!(changes["src/untracked.py"], ChangeKind::Added);
        Ok(())
    }
}
