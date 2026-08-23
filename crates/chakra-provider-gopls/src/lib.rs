//! Live, optional gopls adapter for Go precise enrichment (ADR-0041).
//!
//! The language-neutral worker mechanics (session lifecycle, revision-scoped
//! document synchronization, the post-synchronization request barrier,
//! observability, restart, and shutdown) live in `chakra-provider-worker`
//! (issue #94); this crate keeps only gopls-specific seams: command
//! discovery, defaults, the `go` language id, and the call-hierarchy
//! capability gate.
//!
//! Only the v0.1 call-hierarchy operations cross this adapter internally.
//! Public contracts are Chakra-native, so LSP URIs, UTF-16 positions, and
//! protocol lifecycle details remain confined to the adapter crates and
//! chakra-lsp.
//!
//! An absent or failing gopls never fails Chakra startup: the provider
//! transitions to `Degraded` and queries keep their syntax results
//! (ADR-0006/0013).

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::provenance::Provenance;
use chakra_domain::symbol::Language;
use chakra_engine::{PreciseQueryRequest, PreciseQueryResult};
use chakra_provider_worker::{
    CallHierarchyDriver, ProviderCommandSpec, ProviderHandle, ProviderHooks, QueryChannel,
    QueryOutcome, WorkerConfig, WorkerError,
};
use lsp_types::InitializeResult;

pub use chakra_provider_worker::{StartError, WorkerShutdownError as ShutdownError};

const DEFAULT_COMMAND_CAPACITY: usize = 8;
const DEFAULT_MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_QUERY_WAIT_TIMEOUT: Duration = Duration::from_secs(1);

/// Resolved `gopls serve` stdio invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoplsCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl GoplsCommand {
    /// Explicit stdio invocation for a configured executable path.
    pub fn stdio(executable: impl Into<OsString>) -> Self {
        Self {
            program: executable.into(),
            args: vec![OsString::from("serve")],
        }
    }

    /// Side-effect-free discovery of gopls on `PATH`.
    pub fn discover() -> Option<Self> {
        Self::discover_with_context(&OperationContext::unbounded())
            .ok()
            .flatten()
    }

    pub fn discover_with_context(
        operation: &OperationContext,
    ) -> Result<Option<Self>, OperationAbort> {
        operation.check()?;
        if let Some(executable) = find_on_path("gopls") {
            return Ok(Some(Self::stdio(executable.into_os_string())));
        }
        Ok(None)
    }
}

impl From<GoplsCommand> for ProviderCommandSpec {
    fn from(command: GoplsCommand) -> Self {
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

/// Process and bounded-wait settings for the optional provider.
#[derive(Debug, Clone)]
pub struct GoplsConfig {
    pub command: GoplsCommand,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub barrier_timeout: Duration,
    pub query_wait_timeout: Duration,
    pub restart_base_delay: Duration,
    pub restart_max_delay: Duration,
    pub command_capacity: usize,
    pub max_message_bytes: usize,
}

impl Default for GoplsConfig {
    fn default() -> Self {
        Self {
            command: GoplsCommand::stdio(OsString::from("gopls")),
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

impl From<GoplsConfig> for WorkerConfig {
    fn from(config: GoplsConfig) -> Self {
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

/// gopls language hooks: Go documents synchronize through the session and
/// the precise surface is the LSP call-hierarchy trio verified at
/// initialization.
#[derive(Debug, Clone, Copy, Default)]
struct GoplsHooks;

impl ProviderHooks for GoplsHooks {
    fn name(&self) -> &'static str {
        "gopls"
    }

    fn provenance(&self) -> Provenance {
        Provenance::Gopls
    }

    fn synchronizes(&self, language: Language) -> bool {
        language == Language::Go
    }

    fn language_id(&self, _path: &RepoRelativePath) -> &'static str {
        "go"
    }

    fn verify_capabilities(&self, result: &InitializeResult) -> Result<(), WorkerError> {
        CallHierarchyDriver::verify_call_hierarchy(result)
    }

    fn query(
        &self,
        channel: &mut dyn QueryChannel,
        request: &PreciseQueryRequest,
        deadline: std::time::Instant,
    ) -> Result<QueryOutcome, WorkerError> {
        CallHierarchyDriver.query(channel, request, deadline, Provenance::Gopls)
    }
}

/// Owned gopls process and worker lifecycle.
pub struct GoplsProvider {
    inner: Arc<ProviderHandle<GoplsHooks>>,
}

impl std::fmt::Debug for GoplsProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, formatter)
    }
}

impl GoplsProvider {
    /// Starts the owner thread. A missing or failing gopls does not fail
    /// Chakra startup: the handle transitions to `Degraded` and later queries
    /// retain syntax results.
    pub fn start(
        initial_workspace: chakra_engine::ProviderWorkspace,
        config: GoplsConfig,
    ) -> Result<Arc<Self>, StartError> {
        let inner = ProviderHandle::start(initial_workspace, config.into(), GoplsHooks)?;
        Ok(Arc::new(Self { inner }))
    }

    /// Idempotent cooperative shutdown followed by joining the owned worker.
    /// The owned process group is terminated, so no gopls child remains.
    pub fn shutdown(&self) -> Result<(), ShutdownError> {
        self.inner.shutdown()
    }
}

impl chakra_engine::PreciseProvider for GoplsProvider {
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

impl Drop for GoplsProvider {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Resolves the provider command: an explicit executable path first, then
/// side-effect-free `gopls` discovery on `PATH`.
pub fn resolve_command(explicit: Option<&OsStr>) -> GoplsCommand {
    resolve_command_with_context(explicit, &OperationContext::unbounded())
        .unwrap_or_else(|_| GoplsCommand::stdio(OsString::from("gopls")))
}

pub fn resolve_command_with_context(
    explicit: Option<&OsStr>,
    operation: &OperationContext,
) -> Result<GoplsCommand, OperationAbort> {
    operation.check()?;
    match explicit {
        Some(path) => Ok(GoplsCommand::stdio(path.to_owned())),
        None => Ok(GoplsCommand::discover_with_context(operation)?
            .unwrap_or_else(|| GoplsCommand::stdio(OsString::from("gopls")))),
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
        let config = GoplsConfig::default();
        assert_eq!(config.command, GoplsCommand::stdio(OsString::from("gopls")));
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
                path: RepoRelativePath::new("src/main.go")?,
                source: Arc::from("package main\n\nfunc target() {}\n"),
                language: Language::Go,
            }],
        );
        let provider = GoplsProvider::start(
            workspace.clone(),
            GoplsConfig {
                command: GoplsCommand::stdio(OsString::from("chakra-definitely-missing-gopls")),
                request_timeout: Duration::from_millis(100),
                barrier_timeout: Duration::from_millis(50),
                ..GoplsConfig::default()
            },
        )?;
        let result = provider.enrich(PreciseQueryRequest {
            workspace,
            symbol: ProviderSymbol {
                name: "target".to_owned(),
                declaration: SourceRange::new(
                    RepoRelativePath::new("src/main.go")?,
                    TextPosition::new(3, 1)?,
                    TextPosition::new(3, 17)?,
                )?,
                language: Language::Go,
            },
            directions: CallHierarchyDirections {
                incoming: true,
                outgoing: false,
            },
            limit: 20,
            priority: chakra_engine::ProviderRequestPriority::Normal,
        });
        assert_eq!(result.state, ProviderState::Degraded);
        assert_eq!(provider.name(), "gopls");
        assert!(provider.supports(Language::Go));
        assert!(!provider.supports(Language::Rust));
        provider.shutdown()?;
        Ok(())
    }
}
