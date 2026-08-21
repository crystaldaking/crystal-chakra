//! Live, optional clangd adapter for C/C++ precise enrichment (ADR-0039).
//!
//! Only the v0.1 call-hierarchy operations cross this adapter internally.
//! Public contracts are Chakra-native, so LSP URIs, UTF-16 positions, and
//! protocol lifecycle details remain confined to this crate and chakra-lsp.
//!
//! An absent or failing clangd never fails Chakra startup: the provider
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
use std::time::Duration;

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
/// Resolved `clangd` stdio invocation with bounded project-wide indexing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClangdCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl ClangdCommand {
    /// Explicit stdio invocation for a configured executable path.
    pub fn stdio(executable: impl Into<OsString>) -> Self {
        Self {
            program: executable.into(),
            args: vec![
                OsString::from("--background-index"),
                OsString::from("--limit-results=500"),
                OsString::from("--log=error"),
            ],
        }
    }

    /// Side-effect-free discovery of clangd on `PATH`.
    pub fn discover() -> Option<Self> {
        Self::discover_with_context(&OperationContext::unbounded())
            .ok()
            .flatten()
    }

    pub fn discover_with_context(
        operation: &OperationContext,
    ) -> Result<Option<Self>, OperationAbort> {
        operation.check()?;
        if let Some(executable) = find_on_path("clangd") {
            return Ok(Some(Self::stdio(executable.into_os_string())));
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

/// Process and bounded-wait settings for the optional provider.
#[derive(Debug, Clone)]
pub struct ClangdConfig {
    pub command: ClangdCommand,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub barrier_timeout: Duration,
    pub query_wait_timeout: Duration,
    pub restart_base_delay: Duration,
    pub restart_max_delay: Duration,
    pub command_capacity: usize,
    pub max_message_bytes: usize,
}

impl Default for ClangdConfig {
    fn default() -> Self {
        Self {
            command: ClangdCommand::stdio(OsString::from("clangd")),
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
    #[error("failed to spawn the clangd owner thread: {0}")]
    ThreadSpawn(#[source] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ShutdownError {
    #[error("clangd owner thread panicked")]
    WorkerPanicked,
    #[error("clangd owner lock is poisoned")]
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

/// Owned clangd process and worker lifecycle.
pub struct ClangdProvider {
    commands: Sender<Command>,
    shared: Arc<Mutex<SharedState>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    stopped: AtomicBool,
    force_stop: Arc<AtomicBool>,
    config: ClangdConfig,
}

impl fmt::Debug for ClangdProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClangdProvider")
            .field("state", &self.state_snapshot())
            .field("stopped", &self.stopped.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl ClangdProvider {
    /// Starts the owner thread. A missing or failing clangd does not fail
    /// Chakra startup: the handle transitions to `Degraded` and later queries
    /// retain syntax results.
    pub fn start(
        initial_workspace: ProviderWorkspace,
        config: ClangdConfig,
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
            .name("chakra-clangd".to_owned())
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
    /// The owned process group is terminated, so no language-server child
    /// remains.
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

impl PreciseProvider for ClangdProvider {
    fn name(&self) -> &'static str {
        "clangd"
    }

    fn supports(&self, language: Language) -> bool {
        language == Language::Cpp
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
        ClangdProvider::last_error(self)
    }

    fn progress(&self) -> Option<ProviderProgress> {
        ClangdProvider::progress(self)
    }

    fn metrics(&self) -> Option<ProviderMetrics> {
        ClangdProvider::metrics(self)
    }

    fn query_wait_budget(&self) -> Option<Duration> {
        Some(self.config.query_wait_timeout)
    }

    fn shutdown(&self) -> Result<(), ProviderShutdownError> {
        ClangdProvider::shutdown(self)
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

impl Drop for ClangdProvider {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Resolves the provider command: an explicit executable path first, then
/// side-effect-free `clangd` discovery on `PATH`.
pub fn resolve_command(explicit: Option<&OsStr>) -> ClangdCommand {
    resolve_command_with_context(explicit, &OperationContext::unbounded())
        .unwrap_or_else(|_| ClangdCommand::stdio(OsString::from("clangd")))
}

pub fn resolve_command_with_context(
    explicit: Option<&OsStr>,
    operation: &OperationContext,
) -> Result<ClangdCommand, OperationAbort> {
    operation.check()?;
    match explicit {
        Some(path) => Ok(ClangdCommand::stdio(path.to_owned())),
        None => Ok(ClangdCommand::discover_with_context(operation)?
            .unwrap_or_else(|| ClangdCommand::stdio(OsString::from("clangd")))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
    use chakra_engine::{CallHierarchyDirections, ProviderDocument};

    #[test]
    fn default_config_uses_a_side_effect_free_command_fallback() {
        let config = ClangdConfig::default();
        assert_eq!(
            config.command,
            ClangdCommand::stdio(OsString::from("clangd"))
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
                path: RepoRelativePath::new("src/index.cpp")?,
                source: Arc::from("namespace sample { void target() {} }\n"),
                language: Language::Cpp,
            }],
        );
        let provider = ClangdProvider::start(
            workspace.clone(),
            ClangdConfig {
                command: ClangdCommand::stdio(OsString::from("chakra-definitely-missing-clangd")),
                request_timeout: Duration::from_millis(100),
                barrier_timeout: Duration::from_millis(50),
                ..ClangdConfig::default()
            },
        )?;
        let result = provider.enrich(PreciseQueryRequest {
            workspace,
            symbol: chakra_engine::ProviderSymbol {
                name: "target".to_owned(),
                declaration: SourceRange::new(
                    RepoRelativePath::new("src/index.cpp")?,
                    TextPosition::new(1, 23)?,
                    TextPosition::new(1, 31)?,
                )?,
                language: Language::Cpp,
            },
            directions: CallHierarchyDirections {
                incoming: true,
                outgoing: false,
            },
            limit: 20,
            priority: chakra_engine::ProviderRequestPriority::Normal,
        });
        assert_eq!(result.state, ProviderState::Degraded);
        assert_eq!(provider.name(), "clangd");
        assert!(provider.supports(Language::Cpp));
        assert!(!provider.supports(Language::Rust));
        provider.shutdown()?;
        Ok(())
    }
}
