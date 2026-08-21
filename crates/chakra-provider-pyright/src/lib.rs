//! Live, optional pyright adapter for Python precise enrichment (ADR-0032).
//!
//! Only the v0.1 call-hierarchy operations cross this adapter internally.
//! Public contracts are Chakra-native, so LSP URIs, UTF-16 positions, and
//! protocol lifecycle details remain confined to this crate and chakra-lsp.
//!
//! An absent or failing pyright never fails Chakra startup: the provider
//! transitions to `Degraded` and queries keep their syntax results
//! (ADR-0006/0013).

mod convert;
mod worker;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::query::{ProviderMetrics, ProviderProgress};
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{
    PreciseProvider, PreciseQueryRequest, PreciseQueryResult, ProviderShutdownError,
    ProviderWorkspace,
};
use crossbeam_channel::{SendTimeoutError, Sender, bounded};
use thiserror::Error;

use crate::worker::Worker;

const DEFAULT_COMMAND_CAPACITY: usize = 8;
const DEFAULT_MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_QUERY_WAIT_TIMEOUT: Duration = Duration::from_secs(1);
pub const COMMAND_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolved pyright invocation. `pyright-langserver --stdio` when the
/// npm/pip-installed binary is used directly, or
/// `node <bundle>/dist/pyright-langserver.js --stdio` when only the global
/// npm package is resolvable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyrightCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl PyrightCommand {
    /// Explicit `pyright-langserver --stdio` invocation for a configured
    /// executable path.
    pub fn stdio(executable: impl Into<OsString>) -> Self {
        Self {
            program: executable.into(),
            args: vec![OsString::from("--stdio")],
        }
    }

    /// Node-launched invocation for a resolved global `pyright` npm bundle
    /// (the `dist/pyright-langserver.js` entry point).
    pub fn node_bundle(node: impl Into<OsString>, bundle: impl Into<OsString>) -> Self {
        Self {
            program: node.into(),
            args: vec![bundle.into(), OsString::from("--stdio")],
        }
    }

    /// Best-effort discovery: a `pyright-langserver` executable on `PATH`
    /// first (npm or pip install), then a global `pyright` npm package
    /// resolved through `npm root -g` and launched with `node`. Returns
    /// `None` when neither is available; callers then start the provider
    /// degraded.
    pub fn discover() -> Option<Self> {
        Self::discover_with_context(&OperationContext::unbounded())
            .ok()
            .flatten()
    }

    pub fn discover_with_context(
        operation: &OperationContext,
    ) -> Result<Option<Self>, OperationAbort> {
        operation.check()?;
        if let Some(executable) = find_on_path("pyright-langserver") {
            return Ok(Some(Self::stdio(executable.into_os_string())));
        }
        let Some(node) = find_on_path("node") else {
            return Ok(None);
        };
        let Some(npm) = find_on_path("npm") else {
            return Ok(None);
        };
        let Some(root) = npm_global_root(&npm, operation)? else {
            return Ok(None);
        };
        let bundle = root
            .join("pyright")
            .join("dist")
            .join("pyright-langserver.js");
        if bundle.is_file() {
            return Ok(Some(Self::node_bundle(
                node.into_os_string(),
                bundle.into_os_string(),
            )));
        }
        Ok(None)
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|directory| {
        let candidate = directory.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let executable = directory.join(format!("{name}.exe"));
            if executable.is_file() {
                return Some(executable);
            }
        }
        None
    })
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

fn npm_global_root(
    npm: &PathBuf,
    operation: &OperationContext,
) -> Result<Option<PathBuf>, OperationAbort> {
    operation.check()?;
    let child = std::process::Command::new(npm)
        .args(["root", "-g"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return Ok(None);
    };
    let deadline = Instant::now() + COMMAND_DISCOVERY_TIMEOUT;
    loop {
        if let Err(abort) = operation.check() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(abort);
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
        }
    }
    operation.check()?;
    let Ok(output) = child.wait_with_output() else {
        return Ok(None);
    };
    if !output.status.success() {
        return Ok(None);
    }
    let Ok(root) = String::from_utf8(output.stdout) else {
        return Ok(None);
    };
    let root = root.trim();
    if root.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(root)))
}

/// Process and bounded-wait settings for the optional provider.
#[derive(Debug, Clone)]
pub struct PyrightConfig {
    pub command: PyrightCommand,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub barrier_timeout: Duration,
    pub query_wait_timeout: Duration,
    pub restart_base_delay: Duration,
    pub restart_max_delay: Duration,
    pub command_capacity: usize,
    pub max_message_bytes: usize,
}

impl Default for PyrightConfig {
    fn default() -> Self {
        Self {
            command: PyrightCommand::stdio(OsString::from("pyright-langserver")),
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(5),
            barrier_timeout: Duration::from_millis(750),
            query_wait_timeout: DEFAULT_QUERY_WAIT_TIMEOUT,
            restart_base_delay: Duration::from_millis(200),
            restart_max_delay: Duration::from_secs(2),
            command_capacity: DEFAULT_COMMAND_CAPACITY,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

#[derive(Debug, Error)]
pub enum StartError {
    #[error("provider command capacity and message size bound must be non-zero")]
    InvalidCapacity,
    #[error("provider startup, request, and barrier timeouts must be non-zero")]
    InvalidTimeout,
    #[error("failed to spawn the pyright owner thread: {0}")]
    ThreadSpawn(#[source] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ShutdownError {
    #[error("pyright owner thread panicked")]
    WorkerPanicked,
    #[error("pyright owner lock is poisoned")]
    LockPoisoned,
}

#[derive(Debug)]
pub(crate) struct SharedState {
    pub(crate) state: ProviderState,
    pub(crate) synced_revision: Option<Revision>,
    pub(crate) provider_epoch: u64,
    pub(crate) last_error: Option<String>,
    pub(crate) progress: Option<ProviderProgress>,
    pub(crate) metrics: ProviderMetrics,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            state: ProviderState::Initializing,
            synced_revision: None,
            provider_epoch: 0,
            last_error: None,
            progress: None,
            metrics: ProviderMetrics::default(),
        }
    }
}

pub(crate) enum Command {
    Enrich {
        request: Box<PreciseQueryRequest>,
        operation: OperationContext,
        response: Sender<PreciseQueryResult>,
    },
}

/// Owned pyright process and worker lifecycle.
pub struct PyrightProvider {
    commands: Sender<Command>,
    shared: Arc<Mutex<SharedState>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    stopped: AtomicBool,
    force_stop: Arc<AtomicBool>,
    config: PyrightConfig,
}

impl fmt::Debug for PyrightProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PyrightProvider")
            .field("state", &self.state_snapshot())
            .field("stopped", &self.stopped.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl PyrightProvider {
    /// Starts the owner thread. A missing or failing pyright does not fail
    /// Chakra startup: the handle transitions to `Degraded` and later queries
    /// retain syntax results.
    pub fn start(
        initial_workspace: ProviderWorkspace,
        config: PyrightConfig,
    ) -> Result<Arc<Self>, StartError> {
        if config.command_capacity == 0 || config.max_message_bytes == 0 {
            return Err(StartError::InvalidCapacity);
        }
        if config.startup_timeout.is_zero()
            || config.request_timeout.is_zero()
            || config.barrier_timeout.is_zero()
            || config.query_wait_timeout.is_zero()
        {
            return Err(StartError::InvalidTimeout);
        }
        let (commands, receiver) = bounded(config.command_capacity);
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let force_stop = Arc::new(AtomicBool::new(false));
        let worker_shared = shared.clone();
        let worker_stop = force_stop.clone();
        let worker_config = config.clone();
        let worker = thread::Builder::new()
            .name("chakra-pyright".to_owned())
            .spawn(move || {
                Worker::new(
                    receiver,
                    worker_shared,
                    worker_stop,
                    worker_config,
                    initial_workspace,
                )
                .run();
            })
            .map_err(StartError::ThreadSpawn)?;
        Ok(Arc::new(Self {
            commands,
            shared,
            worker: Mutex::new(Some(worker)),
            stopped: AtomicBool::new(false),
            force_stop,
            config,
        }))
    }

    pub fn last_error(&self) -> Option<String> {
        self.shared
            .lock()
            .ok()
            .and_then(|state| state.last_error.clone())
    }

    pub fn progress(&self) -> Option<ProviderProgress> {
        self.shared
            .lock()
            .ok()
            .and_then(|state| state.progress.clone())
    }

    pub fn metrics(&self) -> Option<ProviderMetrics> {
        self.shared.lock().ok().map(|state| state.metrics.clone())
    }

    /// Idempotent cooperative shutdown followed by joining the owned worker.
    /// The owned process group is terminated, so no `node` child remains.
    pub fn shutdown(&self) -> Result<(), ShutdownError> {
        self.stopped.store(true, Ordering::Release);
        self.force_stop.store(true, Ordering::Release);
        let worker = self
            .worker
            .lock()
            .map_err(|_| ShutdownError::LockPoisoned)?
            .take();
        if let Some(worker) = worker {
            worker.join().map_err(|_| ShutdownError::WorkerPanicked)?;
        }
        Ok(())
    }

    fn state_snapshot(&self) -> (ProviderState, Option<Revision>, u64) {
        self.shared
            .lock()
            .map_or((ProviderState::Degraded, None, 0), |state| {
                (state.state, state.synced_revision, state.provider_epoch)
            })
    }
}

impl PreciseProvider for PyrightProvider {
    fn name(&self) -> &'static str {
        "pyright"
    }

    fn supports(&self, language: Language) -> bool {
        language == Language::Python
    }

    fn state_for(&self, revision: Revision) -> ProviderState {
        if self.stopped.load(Ordering::Acquire) {
            return ProviderState::Degraded;
        }
        let (state, synced_revision, _) = self.state_snapshot();
        match state {
            ProviderState::Ready if synced_revision == Some(revision) => ProviderState::Ready,
            ProviderState::Ready | ProviderState::CatchingUp => ProviderState::CatchingUp,
            other => other,
        }
    }

    fn last_error(&self) -> Option<String> {
        PyrightProvider::last_error(self)
    }

    fn progress(&self) -> Option<ProviderProgress> {
        PyrightProvider::progress(self)
    }

    fn metrics(&self) -> Option<ProviderMetrics> {
        PyrightProvider::metrics(self)
    }

    fn query_wait_budget(&self) -> Option<Duration> {
        Some(self.config.query_wait_timeout)
    }

    fn shutdown(&self) -> Result<(), ProviderShutdownError> {
        PyrightProvider::shutdown(self)
            .map_err(|error| ProviderShutdownError::new(error.to_string()))
    }

    fn enrich(&self, request: PreciseQueryRequest) -> PreciseQueryResult {
        self.enrich_with_context(request, &OperationContext::unbounded())
    }

    fn enrich_with_context(
        &self,
        request: PreciseQueryRequest,
        operation: &OperationContext,
    ) -> PreciseQueryResult {
        let revision = request.workspace.revision;
        if self.stopped.load(Ordering::Acquire) {
            return PreciseQueryResult::unavailable(revision, ProviderState::Degraded);
        }
        let provider_operation = operation.bounded_by(self.config.query_wait_timeout);
        if provider_operation.check().is_err() {
            return PreciseQueryResult::unavailable(revision, ProviderState::CatchingUp);
        }
        let (sender, receiver) = bounded(1);
        let queue_operation = provider_operation.bounded_by(self.config.barrier_timeout);
        let mut command = Command::Enrich {
            request: Box::new(request),
            operation: provider_operation.clone(),
            response: sender,
        };
        loop {
            let Ok(wait) = queue_operation.poll_timeout(Duration::from_millis(10)) else {
                return PreciseQueryResult::unavailable(revision, ProviderState::CatchingUp);
            };
            match self.commands.send_timeout(command, wait) {
                Ok(()) => break,
                Err(SendTimeoutError::Timeout(returned)) => command = returned,
                Err(SendTimeoutError::Disconnected(_)) => {
                    return PreciseQueryResult::unavailable(revision, ProviderState::Degraded);
                }
            }
        }
        loop {
            let Ok(poll) = provider_operation.poll_timeout(Duration::from_millis(10)) else {
                return PreciseQueryResult::unavailable(revision, ProviderState::CatchingUp);
            };
            match receiver.recv_timeout(poll) {
                Ok(result) => return result,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    return PreciseQueryResult::unavailable(revision, ProviderState::Degraded);
                }
            }
        }
    }
}

impl Drop for PyrightProvider {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Resolves the provider command: an explicit executable path first, then
/// `pyright-langserver`/npm discovery on `PATH`. Exposed for CLI wiring and
/// diagnostics.
pub fn resolve_command(explicit: Option<&OsStr>) -> PyrightCommand {
    resolve_command_with_context(explicit, &OperationContext::unbounded())
        .unwrap_or_else(|_| PyrightCommand::stdio(OsString::from("pyright-langserver")))
}

pub fn resolve_command_with_context(
    explicit: Option<&OsStr>,
    operation: &OperationContext,
) -> Result<PyrightCommand, OperationAbort> {
    operation.check()?;
    match explicit {
        Some(path) => Ok(PyrightCommand::stdio(path.to_owned())),
        None => Ok(PyrightCommand::discover_with_context(operation)?
            .unwrap_or_else(|| PyrightCommand::stdio(OsString::from("pyright-langserver")))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
    use chakra_engine::{CallHierarchyDirections, ProviderDocument};

    #[test]
    fn default_config_uses_a_side_effect_free_command_fallback() {
        let config = PyrightConfig::default();
        assert_eq!(
            config.command,
            PyrightCommand::stdio(OsString::from("pyright-langserver"))
        );
    }

    #[test]
    fn command_discovery_observes_the_caller_deadline() {
        let operation = OperationContext::with_timeout(Duration::ZERO);
        assert_eq!(
            resolve_command_with_context(None, &operation),
            Err(OperationAbort::DeadlineExceeded)
        );
    }

    #[test]
    fn missing_executable_degrades_without_failing_queries()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let workspace = ProviderWorkspace::from_documents(
            root.path().to_path_buf(),
            Revision(1),
            vec![ProviderDocument {
                path: RepoRelativePath::new("src/index.py")?,
                source: Arc::from("def target():\n    pass\n"),
                language: Language::Python,
            }],
        );
        let provider = PyrightProvider::start(
            workspace.clone(),
            PyrightConfig {
                command: PyrightCommand::stdio(OsString::from("chakra-definitely-missing-pyright")),
                request_timeout: Duration::from_millis(100),
                barrier_timeout: Duration::from_millis(50),
                ..PyrightConfig::default()
            },
        )?;
        let result = provider.enrich(PreciseQueryRequest {
            workspace,
            symbol: chakra_engine::ProviderSymbol {
                name: "target".to_owned(),
                declaration: SourceRange::new(
                    RepoRelativePath::new("src/index.py")?,
                    TextPosition::new(1, 1)?,
                    TextPosition::new(1, 14)?,
                )?,
                language: Language::Python,
            },
            directions: CallHierarchyDirections {
                incoming: true,
                outgoing: false,
            },
            limit: 20,
            priority: chakra_engine::ProviderRequestPriority::Normal,
        });
        assert_eq!(result.state, ProviderState::Degraded);
        assert_eq!(provider.name(), "pyright");
        assert!(provider.supports(Language::Python));
        assert!(!provider.supports(Language::Rust));
        provider.shutdown()?;
        Ok(())
    }
}
