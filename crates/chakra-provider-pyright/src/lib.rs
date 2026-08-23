//! Live, optional pyright adapter for Python precise enrichment (ADR-0032).
//!
//! The language-neutral worker mechanics (session lifecycle, revision-scoped
//! document synchronization, the post-synchronization request barrier,
//! observability, restart, and shutdown) live in `chakra-provider-worker`
//! (issue #94); this crate keeps only pyright-specific seams: command
//! discovery, defaults, the `python` language id, and the call-hierarchy
//! capability gate.
//!
//! Only the v0.1 call-hierarchy operations cross this adapter internally.
//! Public contracts are Chakra-native, so LSP URIs, UTF-16 positions, and
//! protocol lifecycle details remain confined to the adapter crates and
//! chakra-lsp.
//!
//! An absent or failing pyright never fails Chakra startup: the provider
//! transitions to `Degraded` and queries keep their syntax results
//! (ADR-0006/0013).

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::provenance::Provenance;
use chakra_domain::symbol::Language;
use chakra_engine::{PreciseQueryRequest, PreciseQueryResult};
use chakra_provider_worker::{
    CallHierarchyDriver, ProviderCommandSpec, ProviderHandle, ProviderHooks, QueryChannel,
    QueryDeadlines, QueryOutcome, WorkerConfig, WorkerError,
};
use lsp_types::InitializeResult;

pub use chakra_provider_worker::{StartError, WorkerShutdownError as ShutdownError};

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

impl From<PyrightCommand> for ProviderCommandSpec {
    fn from(command: PyrightCommand) -> Self {
        Self {
            program: command.program,
            args: command.args,
        }
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

impl From<PyrightConfig> for WorkerConfig {
    fn from(config: PyrightConfig) -> Self {
        Self {
            command: config.command.into(),
            startup_timeout: config.startup_timeout,
            request_timeout: config.request_timeout,
            barrier_timeout: config.barrier_timeout,
            query_wait_timeout: config.query_wait_timeout,
            restart_base_delay: config.restart_base_delay,
            restart_max_delay: config.restart_max_delay,
            command_capacity: config.command_capacity,
            max_message_bytes: config.max_message_bytes,
        }
    }
}

/// pyright language hooks: Python documents synchronize through the session
/// and the precise surface is the LSP call-hierarchy trio verified at
/// initialization.
#[derive(Debug, Clone, Copy, Default)]
struct PyrightHooks;

impl ProviderHooks for PyrightHooks {
    fn name(&self) -> &'static str {
        "pyright"
    }

    fn provenance(&self) -> Provenance {
        Provenance::Pyright
    }

    fn synchronizes(&self, language: Language) -> bool {
        language == Language::Python
    }

    fn language_id(&self, _path: &RepoRelativePath) -> &'static str {
        "python"
    }

    fn verify_capabilities(&self, result: &InitializeResult) -> Result<(), WorkerError> {
        CallHierarchyDriver::verify_call_hierarchy(result)
    }

    fn query(
        &self,
        channel: &mut dyn QueryChannel,
        request: &PreciseQueryRequest,
        deadlines: QueryDeadlines,
    ) -> Result<QueryOutcome, WorkerError> {
        CallHierarchyDriver.query(channel, request, deadlines, Provenance::Pyright)
    }
}

/// Owned pyright process and worker lifecycle.
pub struct PyrightProvider {
    inner: Arc<ProviderHandle<PyrightHooks>>,
}

impl std::fmt::Debug for PyrightProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, formatter)
    }
}

impl PyrightProvider {
    /// Starts the owner thread. A missing or failing pyright does not fail
    /// Chakra startup: the handle transitions to `Degraded` and later queries
    /// retain syntax results.
    pub fn start(
        initial_workspace: chakra_engine::ProviderWorkspace,
        config: PyrightConfig,
    ) -> Result<Arc<Self>, StartError> {
        let inner = ProviderHandle::start(initial_workspace, config.into(), PyrightHooks)?;
        Ok(Arc::new(Self { inner }))
    }

    /// Idempotent cooperative shutdown followed by joining the owned worker.
    /// The owned process group is terminated, so no `node` child remains.
    pub fn shutdown(&self) -> Result<(), ShutdownError> {
        self.inner.shutdown()
    }
}

impl chakra_engine::PreciseProvider for PyrightProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn supports(&self, language: Language) -> bool {
        self.inner.supports(language)
    }

    fn state_for(
        &self,
        revision: chakra_domain::revision::Revision,
    ) -> chakra_domain::state::ProviderState {
        self.inner.state_for(revision)
    }

    fn last_error(&self) -> Option<String> {
        self.inner.last_error()
    }

    fn progress(&self) -> Option<chakra_domain::query::ProviderProgress> {
        self.inner.progress()
    }

    fn metrics(&self) -> Option<chakra_domain::query::ProviderMetrics> {
        self.inner.metrics()
    }

    fn orchestration_metrics(&self) -> Option<chakra_domain::query::ProviderOrchestrationMetrics> {
        self.inner.orchestration_metrics()
    }

    fn query_wait_budget(&self) -> Option<Duration> {
        self.inner.query_wait_budget()
    }

    fn shutdown(&self) -> Result<(), chakra_engine::ProviderShutdownError> {
        chakra_engine::PreciseProvider::shutdown(self.inner.as_ref())
    }

    fn enrich(&self, request: PreciseQueryRequest) -> PreciseQueryResult {
        self.inner.enrich(request)
    }

    fn enrich_with_context(
        &self,
        request: PreciseQueryRequest,
        operation: &OperationContext,
    ) -> PreciseQueryResult {
        self.inner.enrich_with_context(request, operation)
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
    use chakra_domain::revision::Revision;
    use chakra_domain::state::ProviderState;
    use chakra_engine::{
        CallHierarchyDirections, PreciseProvider, ProviderDocument, ProviderSymbol,
    };

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
        let workspace = chakra_engine::ProviderWorkspace::from_documents(
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
            symbol: ProviderSymbol {
                name: "target".to_owned(),
                declaration: SourceRange::new(
                    RepoRelativePath::new("src/index.py")?,
                    TextPosition::new(1, 1)?,
                    TextPosition::new(1, 12)?,
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
