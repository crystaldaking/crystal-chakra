//! Git-aware supported-language file discovery (SPEC §20; roadmap §11).

use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use chakra_domain::identity::{IdentityError, RepositoryId, WorkspaceIdentity};
use chakra_domain::location::{RepoPathError, RepoRelativePath};
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::symbol::Language;
use thiserror::Error;

const MAX_GIT_STDOUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_GIT_STDERR_BYTES: usize = 16 * 1024;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(1);
const MAX_REPOSITORY_ROOTS: usize = 64;

/// Git-visible inputs that can affect one syntax revision.
///
/// Source files are parsed into the graph. Metadata inputs are not indexed as
/// source, but ecosystem metadata can change query-visible package and
/// source-role facts, so freshness reconciliation must pin it in the same
/// pre/post inventory proof.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceInventory {
    pub sources: Vec<RepoRelativePath>,
    pub metadata_inputs: Vec<RepoRelativePath>,
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

/// Failure to establish the Git worktree or enumerate its source files.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error(transparent)]
    Operation(#[from] OperationAbort),
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
    #[error("Git command `{command}` output exceeded the {limit}-byte safety budget")]
    OutputTooLarge { command: &'static str, limit: usize },
    #[error("Git command `{command}` exceeded the {seconds} second process deadline")]
    Timeout { command: &'static str, seconds: u64 },
    #[error("Git returned an invalid repository root object id: {0}")]
    InvalidRootObjectId(String),
    #[error("repository has more than the {MAX_REPOSITORY_ROOTS} supported root objects")]
    TooManyRootObjects,
    #[error("Git returned a non-UTF-8 administrative path")]
    NonUtf8AdministrativePath,
    #[error("Git returned an empty administrative path")]
    EmptyAdministrativePath,
    #[error("failed to canonicalize Git administrative path {path}: {source}")]
    CanonicalizeAdministrativePath {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to inspect Git administrative path {path}: {source}")]
    AdministrativeMetadata {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Identity(#[from] IdentityError),
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
    current_dir: &Path,
    command_name: &'static str,
    args: &[&OsStr],
    operation: &OperationContext,
) -> Result<GitOutput, DiscoveryError> {
    let operation = operation.bounded_by(GIT_COMMAND_TIMEOUT);
    operation.check()?;
    let mut child = Command::new("git")
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args)
        .spawn()
        .map_err(|source| DiscoveryError::Spawn {
            command: command_name,
            source,
        })?;
    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return Err(DiscoveryError::Spawn {
            command: command_name,
            source: io::Error::other("Git stdout pipe is unavailable"),
        });
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_child(&mut child);
        return Err(DiscoveryError::Spawn {
            command: command_name,
            source: io::Error::other("Git stderr pipe is unavailable"),
        });
    };
    let stdout_reader = match thread::Builder::new()
        .name("chakra-git-discovery-stdout".to_owned())
        .spawn(move || read_bounded(stdout, MAX_GIT_STDOUT_BYTES))
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
    let stderr_reader = match thread::Builder::new()
        .name("chakra-git-discovery-stderr".to_owned())
        .spawn(move || read_bounded(stderr, MAX_GIT_STDERR_BYTES))
    {
        Ok(reader) => reader,
        Err(source) => {
            terminate_child(&mut child);
            let _ = stdout_reader.join();
            return Err(DiscoveryError::Spawn {
                command: command_name,
                source,
            });
        }
    };
    let status = loop {
        if let Err(error) = operation.check() {
            terminate_child(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(error.into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(CHILD_POLL_INTERVAL),
            Err(source) => {
                terminate_child(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(DiscoveryError::Spawn {
                    command: command_name,
                    source,
                });
            }
        }
    };
    // Join both pipe owners before propagating either result. Returning after
    // the first failed join would detach the other reader thread.
    let stdout = stdout_reader.join();
    let stderr = stderr_reader.join();
    let stdout = stdout
        .map_err(|_| DiscoveryError::Spawn {
            command: command_name,
            source: io::Error::other("Git stdout reader panicked"),
        })?
        .map_err(|source| DiscoveryError::Spawn {
            command: command_name,
            source,
        })?;
    let stderr = stderr
        .map_err(|_| DiscoveryError::Spawn {
            command: command_name,
            source: io::Error::other("Git stderr reader panicked"),
        })?
        .map_err(|source| DiscoveryError::Spawn {
            command: command_name,
            source,
        })?;
    Ok(GitOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_exceeded: stdout.exceeded,
    })
}

fn git_output(
    current_dir: &Path,
    command_name: &'static str,
    args: &[&OsStr],
    operation: &OperationContext,
) -> Result<GitOutput, DiscoveryError> {
    let output = capture_git(current_dir, command_name, args, operation)?;
    if output.stdout_exceeded {
        return Err(DiscoveryError::OutputTooLarge {
            command: command_name,
            limit: MAX_GIT_STDOUT_BYTES,
        });
    }
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
    let operation = OperationContext::unbounded();
    resolve_repository_root_with_context(candidate, &operation)
}

/// Context-aware repository-root resolution for owned indexing/query work.
pub fn resolve_repository_root_with_context(
    candidate: &Path,
    operation: &OperationContext,
) -> Result<PathBuf, DiscoveryError> {
    let output = git_output(
        candidate,
        "rev-parse --show-toplevel",
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--path-format=absolute"),
            OsStr::new("--show-toplevel"),
        ],
        operation,
    )?;
    let raw = std::str::from_utf8(&output.stdout).map_err(|_| DiscoveryError::NonUtf8Root)?;
    // `rev-parse` terminates this one path with a newline. Strip only line
    // terminators so other path whitespace stays significant.
    let raw = raw.strip_suffix('\n').unwrap_or(raw);
    let root = PathBuf::from(raw.strip_suffix('\r').unwrap_or(raw));
    std::fs::canonicalize(&root)
        .map_err(|source| DiscoveryError::Canonicalize { path: root, source })
}

/// Resolves a repository identity from Git object history rather than an
/// absolute worktree path.
///
/// Root commit ids are stable across ordinary path moves, linked worktrees,
/// repositories without remotes, and remote URL changes. An unborn
/// repository has no objects to identify it, so Chakra uses the filesystem
/// identity of the Git-reported common administrative directory on supported
/// platforms. No `.git` layout is assumed.
pub fn resolve_repository_identity(candidate: &Path) -> Result<RepositoryId, DiscoveryError> {
    let operation = OperationContext::unbounded();
    let root = resolve_repository_root(candidate)?;
    let roots = git_output(
        &root,
        "rev-list --max-parents=0 --all",
        &[
            OsStr::new("rev-list"),
            OsStr::new("--max-parents=0"),
            OsStr::new("--all"),
        ],
        &operation,
    )?;
    let mut object_ids = parse_root_object_ids(&roots.stdout)?;
    if !object_ids.is_empty() {
        object_ids.sort_unstable();
        object_ids.dedup();
        return Ok(RepositoryId::from_stable_key(format!(
            "git-roots:{}",
            object_ids.join(",")
        ))?);
    }

    let common_dir = git_output(
        &root,
        "rev-parse --git-common-dir",
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--path-format=absolute"),
            OsStr::new("--git-common-dir"),
        ],
        &operation,
    )?;
    let common_dir = parse_single_path(&common_dir.stdout)?;
    let common_dir = std::fs::canonicalize(&common_dir).map_err(|source| {
        DiscoveryError::CanonicalizeAdministrativePath {
            path: common_dir,
            source,
        }
    })?;
    unborn_repository_id(&common_dir)
}

/// Resolves both Git-aware repository identity and the current worktree
/// identity for production engine startup.
pub fn resolve_workspace_identity(candidate: &Path) -> Result<WorkspaceIdentity, DiscoveryError> {
    let root = resolve_repository_root(candidate)?;
    let repository = resolve_repository_identity(&root)?;
    Ok(WorkspaceIdentity::for_repository(&root, repository)?)
}

/// Resolves the worktree-specific and shared Git administrative directories
/// through Git itself. Callers can use these paths to ignore Git's own
/// filesystem churn without assuming that administration lives in `.git`.
pub fn resolve_git_administrative_paths(candidate: &Path) -> Result<Vec<PathBuf>, DiscoveryError> {
    let operation = OperationContext::unbounded();
    let mut paths = Vec::new();
    for (command, argument) in [
        ("rev-parse --git-dir", "--git-dir"),
        ("rev-parse --git-common-dir", "--git-common-dir"),
    ] {
        let output = git_output(
            candidate,
            command,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--path-format=absolute"),
                OsStr::new(argument),
            ],
            &operation,
        )?;
        let path = parse_single_path(&output.stdout)?;
        let canonical = std::fs::canonicalize(&path).map_err(|source| {
            DiscoveryError::CanonicalizeAdministrativePath {
                path: path.clone(),
                source,
            }
        })?;
        paths.push(canonical);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn parse_root_object_ids(output: &[u8]) -> Result<Vec<String>, DiscoveryError> {
    let text = std::str::from_utf8(output)
        .map_err(|_| DiscoveryError::InvalidRootObjectId("non-UTF-8 output".to_owned()))?;
    let mut roots = Vec::new();
    for raw in text.lines() {
        let value = raw.trim();
        if value.is_empty() {
            continue;
        }
        if roots.len() == MAX_REPOSITORY_ROOTS {
            return Err(DiscoveryError::TooManyRootObjects);
        }
        if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DiscoveryError::InvalidRootObjectId(
                value.chars().take(80).collect(),
            ));
        }
        roots.push(value.to_ascii_lowercase());
    }
    Ok(roots)
}

fn parse_single_path(output: &[u8]) -> Result<PathBuf, DiscoveryError> {
    let raw = std::str::from_utf8(output).map_err(|_| DiscoveryError::NonUtf8AdministrativePath)?;
    let raw = raw.strip_suffix('\n').unwrap_or(raw);
    let raw = raw.strip_suffix('\r').unwrap_or(raw);
    if raw.is_empty() {
        return Err(DiscoveryError::EmptyAdministrativePath);
    }
    Ok(PathBuf::from(raw))
}

#[cfg(unix)]
fn unborn_repository_id(common_dir: &Path) -> Result<RepositoryId, DiscoveryError> {
    use std::os::unix::fs::MetadataExt;

    let metadata =
        std::fs::metadata(common_dir).map_err(|source| DiscoveryError::AdministrativeMetadata {
            path: common_dir.to_path_buf(),
            source,
        })?;
    Ok(RepositoryId::from_stable_key(format!(
        "git-unborn:unix:{:x}:{:x}",
        metadata.dev(),
        metadata.ino()
    ))?)
}

#[cfg(windows)]
fn unborn_repository_id(common_dir: &Path) -> Result<RepositoryId, DiscoveryError> {
    use std::os::windows::fs::MetadataExt;

    let metadata =
        std::fs::metadata(common_dir).map_err(|source| DiscoveryError::AdministrativeMetadata {
            path: common_dir.to_path_buf(),
            source,
        })?;
    let volume =
        metadata
            .volume_serial_number()
            .ok_or_else(|| DiscoveryError::AdministrativeMetadata {
                path: common_dir.to_path_buf(),
                source: io::Error::other("volume serial number is unavailable"),
            })?;
    let file = metadata
        .file_index()
        .ok_or_else(|| DiscoveryError::AdministrativeMetadata {
            path: common_dir.to_path_buf(),
            source: io::Error::other("file index is unavailable"),
        })?;
    Ok(RepositoryId::from_stable_key(format!(
        "git-unborn:windows:{volume:x}:{file:x}"
    ))?)
}

#[cfg(not(any(unix, windows)))]
fn unborn_repository_id(common_dir: &Path) -> Result<RepositoryId, DiscoveryError> {
    Ok(RepositoryId::from_stable_key(format!(
        "git-unborn:path:{}",
        common_dir.display()
    ))?)
}

fn is_excluded(path: &Path) -> bool {
    path.components().any(
        |component| matches!(component, Component::Normal(value) if value == OsStr::new("target")),
    )
}

/// Recognizes a v0.1 source language without interpreting `target` build
/// output as source. Git administration is resolved separately through Git
/// and is never inferred from a literal worktree path.
pub fn source_language(path: &str) -> Option<Language> {
    let path = Path::new(path);
    if is_excluded(path) {
        return None;
    }
    match path.extension() {
        Some(extension) if extension == OsStr::new("rs") => Some(Language::Rust),
        Some(extension) if extension == OsStr::new("php") => Some(Language::Php),
        Some(extension) if matches!(extension.to_str(), Some("ts" | "tsx" | "mts" | "cts")) => {
            Some(Language::TypeScript)
        }
        Some(extension) if matches!(extension.to_str(), Some("py" | "pyi")) => {
            Some(Language::Python)
        }
        Some(extension) if matches!(extension.to_str(), Some("js" | "jsx" | "mjs" | "cjs")) => {
            Some(Language::JavaScript)
        }
        Some(extension) if extension == OsStr::new("java") => Some(Language::Java),
        Some(extension) if extension == OsStr::new("cs") => Some(Language::CSharp),
        _ => None,
    }
}

/// Returns tracked plus untracked, non-ignored files for one supported
/// language in deterministic repository-relative order.
pub fn discover_language_files(
    root: &Path,
    language: Language,
) -> Result<Vec<RepoRelativePath>, DiscoveryError> {
    discover_files(root, Some(language))
}

/// Returns tracked plus untracked, non-ignored Rust and PHP sources.
pub fn discover_source_files(root: &Path) -> Result<Vec<RepoRelativePath>, DiscoveryError> {
    discover_files(root, None)
}

/// Discovers supported sources when `root` is already the Git-resolved
/// worktree root.
///
/// Live reconciliation retains that canonical root for its lifetime, so it
/// can avoid a redundant `rev-parse` subprocess on every freshness check.
/// Discovery still goes through Git and never assumes an administrative
/// `.git` layout.
pub fn discover_source_files_in_worktree(
    root: &Path,
) -> Result<Vec<RepoRelativePath>, DiscoveryError> {
    discover_source_files_in_worktree_with_context(root, &OperationContext::unbounded())
}

/// Context-aware worktree discovery used by freshness reconciliation.
pub fn discover_source_files_in_worktree_with_context(
    root: &Path,
    operation: &OperationContext,
) -> Result<Vec<RepoRelativePath>, DiscoveryError> {
    Ok(discover_workspace_inventory_in_worktree_with_context(root, operation)?.sources)
}

/// Returns one Git inventory for every source and classification input used by
/// a syntax revision. `root` must already be the Git-resolved worktree root.
pub fn discover_workspace_inventory_in_worktree_with_context(
    root: &Path,
    operation: &OperationContext,
) -> Result<WorkspaceInventory, DiscoveryError> {
    let output = git_output(
        root,
        "ls-files --cached --others --exclude-standard",
        &[
            OsStr::new("ls-files"),
            OsStr::new("--cached"),
            OsStr::new("--others"),
            OsStr::new("--exclude-standard"),
            OsStr::new("-z"),
        ],
        operation,
    )?;
    workspace_inventory_from_git_output(root, &output.stdout, operation)
}

fn discover_files(
    root: &Path,
    language: Option<Language>,
) -> Result<Vec<RepoRelativePath>, DiscoveryError> {
    let root = resolve_repository_root(root)?;
    discover_files_in_root(&root, language, &OperationContext::unbounded())
}

fn discover_files_in_root(
    root: &Path,
    language: Option<Language>,
    operation: &OperationContext,
) -> Result<Vec<RepoRelativePath>, DiscoveryError> {
    let inventory = discover_workspace_inventory_in_worktree_with_context(root, operation)?;
    Ok(inventory
        .sources
        .into_iter()
        .filter(|path| {
            language.is_none_or(|expected| source_language(path.as_str()) == Some(expected))
        })
        .collect())
}

fn workspace_inventory_from_git_output(
    root: &Path,
    output: &[u8],
    operation: &OperationContext,
) -> Result<WorkspaceInventory, DiscoveryError> {
    let mut inventory = WorkspaceInventory::default();
    for raw in output.split(|byte| *byte == 0) {
        operation.check()?;
        if raw.is_empty() {
            continue;
        }
        let source = raw.ends_with(b".rs")
            || raw.ends_with(b".php")
            || raw.ends_with(b".ts")
            || raw.ends_with(b".tsx")
            || raw.ends_with(b".mts")
            || raw.ends_with(b".cts")
            || raw.ends_with(b".py")
            || raw.ends_with(b".pyi")
            || raw.ends_with(b".js")
            || raw.ends_with(b".jsx")
            || raw.ends_with(b".mjs")
            || raw.ends_with(b".cjs")
            || raw.ends_with(b".java")
            || raw.ends_with(b".cs");
        let metadata_input = raw_is_metadata_input(raw);
        if !source && !metadata_input {
            continue;
        }
        let raw = std::str::from_utf8(raw).map_err(|_| DiscoveryError::NonUtf8Path)?;
        let candidate = Path::new(raw);
        // A tracked path may currently be deleted. Symlinks are skipped so
        // repository content cannot make the indexer read outside the root.
        let Ok(metadata) = std::fs::symlink_metadata(root.join(candidate)) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let path = RepoRelativePath::new(raw)?;
        let is_source = source_language(raw).is_some();
        if is_source {
            inventory.sources.push(path.clone());
        }
        // A `setup.py` is both a Python source and a project-scope manifest:
        // it joins the source inventory and is still a metadata input.
        if metadata_input
            && (!is_source
                || raw == "setup.py"
                || raw
                    .strip_suffix("setup.py")
                    .is_some_and(|p| p.ends_with('/')))
        {
            inventory.metadata_inputs.push(path);
        }
    }
    inventory.sources.sort();
    inventory.sources.dedup();
    inventory.metadata_inputs.sort();
    inventory.metadata_inputs.dedup();
    Ok(inventory)
}

fn raw_is_metadata_input(raw: &[u8]) -> bool {
    if [
        b".csproj".as_slice(),
        b".sln".as_slice(),
        b".slnx".as_slice(),
    ]
    .into_iter()
    .any(|suffix| raw.ends_with(suffix))
    {
        return true;
    }
    [
        b"Cargo.toml".as_slice(),
        b"Cargo.lock".as_slice(),
        b"composer.json".as_slice(),
        b"package.json".as_slice(),
        b"tsconfig.json".as_slice(),
        b"jsconfig.json".as_slice(),
        b"pyproject.toml".as_slice(),
        b"setup.py".as_slice(),
        b"setup.cfg".as_slice(),
        b"pom.xml".as_slice(),
        b"build.gradle".as_slice(),
        b"build.gradle.kts".as_slice(),
        b"settings.gradle".as_slice(),
        b"settings.gradle.kts".as_slice(),
        b"Directory.Build.props".as_slice(),
        b"Directory.Build.targets".as_slice(),
        b"Directory.Packages.props".as_slice(),
        b"global.json".as_slice(),
        b".cargo/config".as_slice(),
        b".cargo/config.toml".as_slice(),
        b"rust-toolchain".as_slice(),
        b"rust-toolchain.toml".as_slice(),
    ]
    .into_iter()
    .any(|name| {
        raw == name
            || raw
                .strip_suffix(name)
                .is_some_and(|prefix| prefix.ends_with(b"/"))
    })
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

        let files = discover_language_files(repository.path(), Language::Rust)?;
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

        let files = discover_language_files(repository.path(), Language::Rust)?;
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
        let files = discover_language_files(&worktree, Language::Rust)?;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].as_str(), "src/lib.rs");
        assert_eq!(discover_source_files_in_worktree(&worktree)?, files);
        let administrative = resolve_git_administrative_paths(&worktree)?;
        assert!(!administrative.is_empty());
        assert!(administrative.iter().all(|path| path.is_absolute()));
        assert!(
            administrative
                .iter()
                .all(|path| !path.starts_with(&worktree)),
            "linked-worktree administration must be resolved through Git, not assumed below the worktree"
        );
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

    #[test]
    fn rejects_an_empty_git_administrative_path() {
        assert!(matches!(
            parse_single_path(b"\n"),
            Err(DiscoveryError::EmptyAdministrativePath)
        ));
    }

    #[test]
    fn unrelated_non_utf8_file_does_not_break_rust_discovery() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let inventory = workspace_inventory_from_git_output(
            repository.path(),
            b"src/lib.rs\0unrelated-\xff.bin\0",
            &OperationContext::unbounded(),
        )?;
        assert_eq!(inventory.sources.len(), 1);
        assert_eq!(inventory.sources[0].as_str(), "src/lib.rs");
        Ok(())
    }

    #[test]
    fn shared_inventory_retains_source_and_classification_inputs() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        for (path, contents) in [
            ("Cargo.toml", "[workspace]\n"),
            ("Cargo.lock", "version = 4\n"),
            ("composer.json", "{}\n"),
        ] {
            fs::write(repository.path().join(path), contents)?;
        }
        fs::create_dir_all(repository.path().join("target/generated"))?;
        fs::write(
            repository.path().join("target/generated/Cargo.toml"),
            "[workspace]\n",
        )?;
        fs::create_dir_all(repository.path().join("ignored"))?;
        fs::write(repository.path().join("ignored/composer.json"), "{}\n")?;
        fs::write(
            repository.path().join(".gitignore"),
            "ignored.rs\ntarget/\nignored/\n",
        )?;
        let inventory = discover_workspace_inventory_in_worktree_with_context(
            repository.path(),
            &OperationContext::unbounded(),
        )?;
        assert_eq!(inventory.sources, [RepoRelativePath::new("src/lib.rs")?]);
        assert_eq!(
            inventory.metadata_inputs,
            [
                RepoRelativePath::new("Cargo.lock")?,
                RepoRelativePath::new("Cargo.toml")?,
                RepoRelativePath::new("composer.json")?,
            ]
        );
        Ok(())
    }

    #[test]
    fn discovers_php_without_mixing_languages() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(
            repository.path().join("src/service.php"),
            "<?php function pay() {}\n",
        )?;

        let php = discover_language_files(repository.path(), Language::Php)?;
        assert_eq!(php.len(), 1);
        assert_eq!(php[0].as_str(), "src/service.php");
        let all = discover_source_files(repository.path())?;
        assert_eq!(all.len(), 2);
        Ok(())
    }

    #[test]
    fn discovers_typescript_extensions_without_mixing_languages() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        for (path, source) in [
            ("src/service.ts", "export function pay(): void {}\n"),
            ("src/view.tsx", "export function View() { return null; }\n"),
            ("src/entry.mts", "export function entry(): void {}\n"),
            ("src/legacy.cts", "export function legacy(): void {}\n"),
        ] {
            fs::write(repository.path().join(path), source)?;
        }

        let typescript = discover_language_files(repository.path(), Language::TypeScript)?;
        let paths: Vec<&str> = typescript.iter().map(RepoRelativePath::as_str).collect();
        assert_eq!(
            paths,
            [
                "src/entry.mts",
                "src/legacy.cts",
                "src/service.ts",
                "src/view.tsx"
            ]
        );
        let rust = discover_language_files(repository.path(), Language::Rust)?;
        assert_eq!(rust.len(), 1);
        let all = discover_source_files(repository.path())?;
        assert_eq!(all.len(), 5);
        Ok(())
    }

    #[test]
    fn discovers_javascript_extensions_without_mixing_metadata() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        for (path, source) in [
            ("src/service.js", "export function pay() {}\n"),
            (
                "src/view.jsx",
                "export function Panel() { return <section/>; }\n",
            ),
            ("src/modern.mjs", "export function modern() {}\n"),
            ("src/legacy.cjs", "module.exports = {};\n"),
        ] {
            fs::write(repository.path().join(path), source)?;
        }
        fs::write(repository.path().join("jsconfig.json"), "{}\n")?;

        let javascript = discover_language_files(repository.path(), Language::JavaScript)?;
        let paths: Vec<&str> = javascript.iter().map(RepoRelativePath::as_str).collect();
        assert_eq!(
            paths,
            [
                "src/legacy.cjs",
                "src/modern.mjs",
                "src/service.js",
                "src/view.jsx"
            ],
            "jsconfig.json is a metadata input, never a JavaScript source"
        );
        let rust = discover_language_files(repository.path(), Language::Rust)?;
        assert_eq!(rust.len(), 1);
        Ok(())
    }

    #[test]
    fn discovers_java_extension_without_mixing_metadata() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        fs::create_dir_all(root.join("src/main/java/chakra"))?;
        fs::write(
            root.join("src/main/java/chakra/Service.java"),
            "package chakra;\nclass Service {}\n",
        )?;
        fs::write(
            root.join("pom.xml"),
            "<project><artifactId>app</artifactId></project>\n",
        )?;
        fs::write(root.join("build.gradle"), "plugins { id 'java' }\n")?;

        let java = discover_language_files(root, Language::Java)?;
        let paths: Vec<&str> = java.iter().map(RepoRelativePath::as_str).collect();
        assert_eq!(
            paths,
            ["src/main/java/chakra/Service.java"],
            "pom.xml and build.gradle are metadata inputs, never Java sources"
        );
        let rust = discover_language_files(root, Language::Rust)?;
        assert_eq!(rust.len(), 1);
        Ok(())
    }

    #[test]
    fn discovers_python_extensions_and_setup_py_metadata_without_mixing()
    -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        for (path, source) in [
            ("src/service.py", "def pay():\n    pass\n"),
            ("src/service.pyi", "def pay() -> None: ...\n"),
            ("setup.py", "from setuptools import setup\nsetup()\n"),
        ] {
            fs::write(repository.path().join(path), source)?;
        }

        let python = discover_language_files(repository.path(), Language::Python)?;
        let paths: Vec<&str> = python.iter().map(RepoRelativePath::as_str).collect();
        assert_eq!(paths, ["setup.py", "src/service.py", "src/service.pyi"]);
        let rust = discover_language_files(repository.path(), Language::Rust)?;
        assert_eq!(rust.len(), 1);
        let classified = crate::discover_classified_sources(repository.path(), Language::Python)?;
        let setup = classified
            .iter()
            .find(|source| source.path.as_str() == "setup.py")
            .ok_or("setup.py missing from classified sources")?;
        assert_eq!(
            setup.metadata.classification,
            chakra_domain::source::SourceClassification::PyprojectMetadata
        );
        Ok(())
    }

    #[test]
    fn discovers_csharp_without_mixing_dotnet_metadata() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        fs::create_dir_all(root.join("src/Payments"))?;
        fs::write(
            root.join("src/Payments/Service.cs"),
            "namespace Payments; class Service {}\n",
        )?;
        fs::write(
            root.join("src/Payments/Payments.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\" />\n",
        )?;
        fs::write(
            root.join("Payments.sln"),
            "Microsoft Visual Studio Solution File\n",
        )?;
        fs::write(root.join("Directory.Build.props"), "<Project />\n")?;

        let csharp = discover_language_files(root, Language::CSharp)?;
        let paths: Vec<&str> = csharp.iter().map(RepoRelativePath::as_str).collect();
        assert_eq!(paths, ["src/Payments/Service.cs"]);
        let inventory = discover_workspace_inventory_in_worktree_with_context(
            root,
            &OperationContext::unbounded(),
        )?;
        let metadata: Vec<&str> = inventory
            .metadata_inputs
            .iter()
            .map(RepoRelativePath::as_str)
            .filter(|path| path.contains("Payments") || *path == "Directory.Build.props")
            .collect();
        assert_eq!(
            metadata,
            [
                "Directory.Build.props",
                "Payments.sln",
                "src/Payments/Payments.csproj"
            ]
        );
        Ok(())
    }
}
