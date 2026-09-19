//! Ephemeral, owner-thread KMP project import. Gradle resolves its own model;
//! no build process runs from offline indexing or the MCP adapter.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chakra_domain::symbol::Language;
use chakra_engine::ProviderWorkspace;
use chakra_provider_worker::WorkerError;

const SCRIPT: &str = include_str!("kmp-import.gradle");
const MAX_LOG: u64 = 16 * 1024 * 1024;
const MAX_MODEL: u64 = 16 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(25);

#[derive(Debug)]
struct Prepared {
    directory: tempfile::TempDir,
    multiplatform: bool,
    source_roots: Vec<PathBuf>,
    complete: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ProjectImport {
    prepared: Mutex<Option<Prepared>>,
}

impl ProjectImport {
    pub(crate) fn coverage_complete(&self) -> bool {
        self.prepared
            .lock()
            .is_ok_and(|prepared| prepared.as_ref().is_none_or(|prepared| prepared.complete))
    }

    pub(crate) fn covers(
        &self,
        workspace: &ProviderWorkspace,
        path: &chakra_domain::location::RepoRelativePath,
    ) -> bool {
        self.prepared.lock().is_ok_and(|prepared| {
            prepared.as_ref().is_none_or(|prepared| {
                !prepared.multiplatform
                    || prepared.source_roots.iter().any(|root| {
                        workspace
                            .repository_root
                            .join(path.as_str())
                            .starts_with(root)
                    })
            })
        })
    }
    pub(crate) fn root(&self) -> Option<PathBuf> {
        self.prepared.lock().ok().and_then(|prepared| {
            prepared
                .as_ref()
                .filter(|item| item.multiplatform)
                .map(|item| item.directory.path().to_owned())
        })
    }

    pub(crate) fn prepare(
        &self,
        workspace: &ProviderWorkspace,
        deadline: Instant,
        check: &dyn Fn() -> Result<(), WorkerError>,
    ) -> Result<(), WorkerError> {
        *self
            .prepared
            .lock()
            .map_err(|_| failure("import state unavailable"))? = None;
        verify_inputs(workspace, check)?;
        let root = &workspace.repository_root;
        if !root.join("build.gradle.kts").is_file()
            && !root.join("build.gradle").is_file()
            && !root.join("settings.gradle.kts").is_file()
            && !root.join("settings.gradle").is_file()
        {
            return Ok(());
        }
        // A settings-only workspace can be an unconfigured single-file project.
        // With no wrapper/Gradle installation retain the server's ordinary import.
        let Some(program) = gradle(root) else {
            if root.join("build.gradle.kts").is_file() || root.join("build.gradle").is_file() {
                return Err(failure(
                    "Gradle import requires a project Gradle wrapper or gradle on PATH",
                ));
            }
            return Ok(());
        };
        check()?;
        let directory = tempfile::Builder::new()
            .prefix("chakra-kotlin-")
            .tempdir()
            .map_err(io_error)?;
        let script = directory.path().join("import.gradle");
        fs::write(&script, SCRIPT).map_err(io_error)?;
        let model_directory = directory.path().join("model");
        fs::create_dir(&model_directory).map_err(io_error)?;
        let mut command = Command::new(program);
        command
            .current_dir(root)
            .args(["--no-daemon", "--console=plain", "--max-workers=2"])
            .arg(format!(
                "-Dchakra.kotlin.modelDir={}",
                model_directory.display()
            ))
            .arg("-I")
            .arg(&script)
            .arg("chakraExportKotlinModel");
        let log = directory.path().join("gradle.log");
        run_owned(&mut command, &log, deadline, check)?;
        check()?;
        verify_inputs(workspace, check)?;
        let model = model_directory.join("workspace.json");
        let (multiplatform, source_roots) = if model.is_file() {
            (true, validate_model(&model, root)?)
        } else if model_directory.join("not-multiplatform").is_file() {
            (false, Vec::new())
        } else {
            return Err(failure("Gradle did not produce a project model"));
        };
        check()?;
        if Instant::now() >= deadline {
            return Err(WorkerError::Timeout);
        }
        let complete = !multiplatform
            || !model_directory
                .join("incomplete-coverage")
                .try_exists()
                .map_err(io_error)?;
        *self
            .prepared
            .lock()
            .map_err(|_| failure("import state unavailable"))? = Some(Prepared {
            directory,
            multiplatform,
            source_roots,
            complete,
        });
        Ok(())
    }

    pub(crate) fn initialization_root(&self) -> Option<PathBuf> {
        self.root().map(|root| root.join("model"))
    }
}

pub(crate) fn verify_inputs(
    workspace: &ProviderWorkspace,
    check: &dyn Fn() -> Result<(), WorkerError>,
) -> Result<(), WorkerError> {
    for input in workspace.inputs_for(Language::Kotlin) {
        check()?;
        if !fs::metadata(workspace.repository_root.join(input.path.as_str()))
            .is_ok_and(|metadata| input.matches_metadata(&metadata))
        {
            // The model would describe another worktree state. A newer
            // published revision must retry; never publish it under this one.
            return Err(WorkerError::InputsChanged);
        }
    }
    Ok(())
}

fn gradle(root: &Path) -> Option<PathBuf> {
    let wrapper = root.join(if cfg!(windows) {
        "gradlew.bat"
    } else {
        "gradlew"
    });
    if super::is_executable_file(&wrapper) {
        return Some(wrapper);
    }
    super::find_on_path(if cfg!(windows) {
        "gradle.bat"
    } else {
        "gradle"
    })
}

fn validate_model(file: &Path, root: &Path) -> Result<Vec<PathBuf>, WorkerError> {
    let root = fs::canonicalize(root).map_err(io_error)?;
    if fs::metadata(file).map_err(io_error)?.len() > MAX_MODEL {
        return Err(failure("KMP project model exceeds 16 MiB"));
    }
    let mut bytes = Vec::new();
    File::open(file)
        .map_err(io_error)?
        .take(MAX_MODEL + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_MODEL {
        return Err(failure("KMP project model exceeds 16 MiB"));
    }
    let model: serde_json::Value = serde_json::from_slice(&bytes)?;
    let modules = model["modules"]
        .as_array()
        .filter(|modules| !modules.is_empty() && modules.len() <= 512)
        .ok_or_else(|| failure("KMP model has no bounded source-set graph"))?;
    let mut roots = Vec::new();
    for module in modules {
        let content_roots = module["contentRoots"]
            .as_array()
            .ok_or_else(|| failure("KMP model lacks content roots"))?;
        for content in content_roots {
            let source_roots = content["sourceRoots"]
                .as_array()
                .ok_or_else(|| failure("KMP model lacks source roots"))?;
            for source in source_roots {
                let path = source["path"]
                    .as_str()
                    .map(Path::new)
                    .ok_or_else(|| failure("invalid KMP source root"))?;
                let path = canonical_source_root(path)?;
                if !path.starts_with(&root) {
                    return Err(failure("KMP source root is outside the canonical worktree"));
                }
                roots.push(path);
                if roots.len() > 4096 {
                    return Err(failure("KMP model exceeds 4096 source roots"));
                }
            }
        }
    }
    roots.sort();
    roots.dedup();
    if roots.is_empty() {
        return Err(failure("KMP model contains no source roots"));
    }
    Ok(roots)
}

// Gradle includes directories that do not exist yet. Resolve their nearest
// existing ancestor so Java's Windows paths and Rust's verbatim paths compare
// consistently, and existing symlinks cannot disguise an external source root.
fn canonical_source_root(path: &Path) -> Result<PathBuf, WorkerError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        return Err(failure("invalid absolute KMP source root"));
    }
    let mut ancestor = path;
    loop {
        match fs::canonicalize(ancestor) {
            Ok(canonical) => {
                if !canonical.is_dir() {
                    return Err(failure("KMP source root ancestor is not a directory"));
                }
                let suffix = path
                    .strip_prefix(ancestor)
                    .map_err(|_| failure("invalid KMP source root ancestor"))?;
                return Ok(canonical.join(suffix));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if fs::symlink_metadata(ancestor).is_ok() {
                    return Err(failure("KMP source root contains a dangling symlink"));
                }
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| failure("KMP source root has no existing ancestor"))?;
            }
            Err(error) => return Err(io_error(error)),
        }
    }
}

fn run_owned(
    command: &mut Command,
    log: &Path,
    deadline: Instant,
    check: &dyn Fn() -> Result<(), WorkerError>,
) -> Result<(), WorkerError> {
    check()?;
    if Instant::now() >= deadline {
        return Err(WorkerError::Timeout);
    }
    let mut output = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(log)
        .map_err(io_error)?;
    command
        .stdin(Stdio::null())
        .stdout(output.try_clone().map_err(io_error)?)
        .stderr(output.try_clone().map_err(io_error)?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = OwnedChild(command.spawn().map_err(io_error)?);
    loop {
        check()?;
        if Instant::now() >= deadline {
            return Err(WorkerError::Timeout);
        }
        if output.metadata().map_err(io_error)?.len() > MAX_LOG {
            return Err(failure("Gradle import exceeded the 16 MiB log limit"));
        }
        if let Some(status) = child.0.try_wait().map_err(io_error)? {
            if status.success() {
                return Ok(());
            }
            let length = output.metadata().map_err(io_error)?.len();
            output
                .seek(SeekFrom::Start(length.saturating_sub(4096)))
                .map_err(io_error)?;
            let mut tail = Vec::new();
            output.take(4096).read_to_end(&mut tail).map_err(io_error)?;
            tracing::debug!(target: "chakra_kotlin_import", output = %String::from_utf8_lossy(&tail), "Gradle import failed");
            return Err(failure(format!(
                "Gradle import exited with {status}; inspect Gradle configuration and dependencies"
            )));
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.0.id()) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        #[cfg(windows)]
        if let Ok(mut terminator) = Command::new("taskkill")
            .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let deadline = Instant::now() + Duration::from_secs(1);
            while matches!(terminator.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = terminator.kill();
            let _ = terminator.wait();
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn io_error(error: std::io::Error) -> WorkerError {
    failure(error.to_string())
}
fn failure(message: impl Into<String>) -> WorkerError {
    WorkerError::ProjectImport(message.into())
}

#[cfg(test)]
mod tests;
