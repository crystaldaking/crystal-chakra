//! Live, optional kotlin-lsp adapter for Kotlin precise enrichment
//! (ADR-0056, issue #209).
//!
//! The language-neutral worker mechanics (session lifecycle, revision-scoped
//! document synchronization, the post-synchronization request barrier,
//! observability, restart, and shutdown) live in `chakra-provider-worker`;
//! this crate keeps only kotlin-lsp-specific seams: command discovery,
//! defaults, companion Java synchronization, build-model import, readiness,
//! and call binding through hierarchy or definition/reference requests.
//!
//! The public operations provide v0.4 call intelligence; internal LSP
//! definitions and references support KMP and mixed Kotlin/Java workspaces.
//! Public contracts are Chakra-native, so LSP URIs, UTF-16 positions, and
//! protocol lifecycle details remain confined to the adapter crates and
//! chakra-lsp.
//!
//! An absent or failing kotlin-lsp never fails Chakra startup: the provider
//! transitions to `Degraded` and queries keep their syntax results
//! (ADR-0006/0013). The selected server is JetBrains' official standalone
//! `kotlin-server` distribution (Alpha, `bin/intellij-server` launcher,
//! JDK 25); ADR-0056 records the evaluation.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
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

mod import;
mod semantic;

pub use chakra_provider_worker::{StartError, WorkerShutdownError as ShutdownError};

const DEFAULT_COMMAND_CAPACITY: usize = 8;
const DEFAULT_MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_QUERY_WAIT_TIMEOUT: Duration = Duration::from_secs(1);

/// Resolved `intellij-server --stdio` invocation of the standalone
/// kotlin-server distribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KotlinLspCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl KotlinLspCommand {
    /// Explicit stdio invocation for a configured executable path.
    pub fn stdio(executable: impl Into<OsString>) -> Self {
        Self {
            program: executable.into(),
            args: vec![OsString::from("--stdio")],
        }
    }

    /// Side-effect-free discovery of a `kotlin-lsp` executable on `PATH`.
    /// The standalone distribution launches through `bin/intellij-server`;
    /// installations commonly expose a `kotlin-lsp` wrapper, which is what
    /// discovery looks for. An explicit path (private configuration or CLI)
    /// always wins.
    pub fn discover() -> Option<Self> {
        Self::discover_with_context(&OperationContext::unbounded())
            .ok()
            .flatten()
    }

    pub fn discover_with_context(
        operation: &OperationContext,
    ) -> Result<Option<Self>, OperationAbort> {
        operation.check()?;
        if let Some(executable) = find_on_path("kotlin-lsp") {
            return Ok(Some(Self::stdio(executable.into_os_string())));
        }
        Ok(None)
    }
}

impl From<KotlinLspCommand> for ProviderCommandSpec {
    fn from(command: KotlinLspCommand) -> Self {
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
pub struct KotlinLspConfig {
    pub command: KotlinLspCommand,
    pub startup_timeout: Duration,
    /// Bounds cold project import and indexing after LSP initialization.
    /// Readiness requires completed work-done progress and a hierarchy query
    /// uninterrupted by further import/indexing, not merely a prepared item.
    pub readiness_timeout: Duration,
    pub request_timeout: Duration,
    pub barrier_timeout: Duration,
    pub query_wait_timeout: Duration,
    pub restart_base_delay: Duration,
    pub restart_max_delay: Duration,
    pub command_capacity: usize,
    pub max_message_bytes: usize,
}

impl Default for KotlinLspConfig {
    fn default() -> Self {
        Self {
            command: KotlinLspCommand::stdio(OsString::from("kotlin-lsp")),
            // The Alpha server imports Gradle/Maven projects on first use;
            // startup gets the same generous bound as the JVM jdtls route.
            startup_timeout: Duration::from_secs(180),
            readiness_timeout: Duration::from_secs(180),
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

impl From<KotlinLspConfig> for WorkerConfig {
    fn from(config: KotlinLspConfig) -> Self {
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

/// kotlin-lsp hooks keep imported models, companion documents and readiness
/// aligned with the requested revision; queries remain Kotlin-owned.
#[derive(Debug, Default)]
struct KotlinLspHooks {
    readiness_timeout: Duration,
    import_state: AtomicU8,
    automatic_import: import::ProjectImport,
}

const IMPORT_PENDING: u8 = 0;
const IMPORT_READY: u8 = 1;
const IMPORT_FAILED: u8 = 2;

impl ProviderHooks for KotlinLspHooks {
    fn initialization_root(&self) -> Option<PathBuf> {
        self.automatic_import.initialization_root()
    }

    fn prepare_session(
        &self,
        workspace: &chakra_engine::ProviderWorkspace,
        deadline: Instant,
        check: &dyn Fn() -> Result<(), WorkerError>,
    ) -> Result<(), WorkerError> {
        self.automatic_import.prepare(workspace, deadline, check)
    }

    fn restart_for_delta(&self, delta: &chakra_engine::ProviderWorkspaceDelta) -> bool {
        let build_script = |path: &RepoRelativePath| {
            let path = path.as_str();
            path.ends_with(".gradle.kts") || path.starts_with("buildSrc/")
        };
        !delta.inputs_created.is_empty()
            || !delta.inputs_changed.is_empty()
            || !delta.inputs_deleted.is_empty()
            || delta
                .created
                .iter()
                .chain(&delta.changed)
                .any(|document| build_script(&document.path))
            || delta.deleted.iter().any(build_script)
    }

    fn before_session_start(&self) -> Result<(), WorkerError> {
        self.import_state.store(IMPORT_PENDING, Ordering::Release);
        Ok(())
    }

    fn observe_notification(&self, method: &str, params: &serde_json::Value) {
        if method != "intellij/workspaceImportState" {
            return;
        }
        let state = if params["phase"].as_str() != Some("FINISHED") {
            IMPORT_PENDING
        } else if params["folders"].as_array().is_some_and(|folders| {
            !folders.is_empty()
                && folders
                    .iter()
                    .all(|folder| folder["status"].as_str() == Some("SUCCESS"))
        }) {
            IMPORT_READY
        } else {
            IMPORT_FAILED
        };
        self.import_state.store(state, Ordering::Release);
    }
    fn name(&self) -> &'static str {
        "kotlin-lsp"
    }

    fn provenance(&self) -> Provenance {
        Provenance::KotlinLsp
    }

    fn synchronizes(&self, language: Language) -> bool {
        matches!(language, Language::Kotlin | Language::Java)
    }

    fn supports_query_language(&self, language: Language) -> bool {
        language == Language::Kotlin
    }

    fn synchronizes_path(&self, language: Language, path: &RepoRelativePath) -> bool {
        // The pinned server imports Gradle projects but does not resolve the
        // Kotlin DSL scripts themselves. They remain tracked project inputs.
        self.synchronizes(language) && !path.as_str().ends_with(".gradle.kts")
    }

    fn language_id(&self, path: &RepoRelativePath) -> &'static str {
        if path.as_str().ends_with(".java") {
            "java"
        } else {
            "kotlin"
        }
    }

    fn readiness_timeout(&self) -> Option<Duration> {
        Some(self.readiness_timeout)
    }

    fn cold_start_outlives_caller_wait(&self) -> bool {
        true
    }

    fn verify_capabilities(&self, result: &InitializeResult) -> Result<(), WorkerError> {
        semantic::verify_capabilities(result)?;
        if self.initialization_root().is_some() {
            return Ok(());
        }
        CallHierarchyDriver::verify_call_hierarchy(result)
    }

    fn query(
        &self,
        channel: &mut dyn QueryChannel,
        request: &PreciseQueryRequest,
        deadlines: QueryDeadlines,
    ) -> Result<QueryOutcome, WorkerError> {
        if !self
            .automatic_import
            .covers(&request.workspace, request.symbol.declaration.file())
        {
            return Ok(QueryOutcome::ready(PreciseQueryResult::unavailable(
                request.workspace.revision,
                chakra_domain::state::ProviderState::Degraded,
            )));
        }
        let cold = !channel.sync_barrier_confirmed();
        let semantic_queries =
            self.initialization_root().is_some() || request.workspace.has_language(Language::Java);
        let deadline = if cold {
            deadlines.readiness
        } else {
            deadlines.request
        };
        loop {
            if Instant::now() >= deadline {
                return Err(WorkerError::Timeout);
            }
            channel.wait_until(Instant::now())?;
            import::verify_inputs(&request.workspace, &|| {
                if Instant::now() >= deadline {
                    Err(WorkerError::Timeout)
                } else {
                    Ok(())
                }
            })?;
            match self.import_state.load(Ordering::Acquire) {
                IMPORT_READY => {}
                IMPORT_FAILED => {
                    return Err(WorkerError::ProjectImport(
                        "kotlin-lsp did not successfully import every workspace folder".to_owned(),
                    ));
                }
                _ => {
                    channel
                        .wait_until(deadline.min(Instant::now() + Duration::from_millis(250)))?;
                    continue;
                }
            }
            let Some(idle_epoch) = channel.work_done_idle_epoch() else {
                channel.wait_until(deadline.min(Instant::now() + Duration::from_millis(250)))?;
                continue;
            };
            // Kotlin can prepare a hierarchy item before its reference index
            // is ready. Both prepare and incoming/outgoing requests therefore
            // share the bounded cold-import budget, not the short warm budget.
            let query_deadlines = QueryDeadlines {
                request: deadline,
                readiness: deadline,
            };
            let mut outcome = if semantic_queries {
                semantic::query(channel, request, deadline)?
            } else {
                CallHierarchyDriver.query(
                    channel,
                    request,
                    query_deadlines,
                    Provenance::KotlinLsp,
                )?
            };
            if !self.automatic_import.coverage_complete() {
                outcome.result.incoming_truncated |= request.directions.incoming;
                outcome.result.outgoing_truncated |= request.directions.outgoing;
            }
            channel.wait_until(Instant::now())?;
            if channel.work_done_idle_epoch() != Some(idle_epoch)
                || self.import_state.load(Ordering::Acquire) != IMPORT_READY
            {
                // Even nonempty Kotlin answers can be approximate while the
                // server imports/indexes. Discard a query spanning such work.
                continue;
            }
            if !outcome.may_improve_when_ready {
                import::verify_inputs(&request.workspace, &|| {
                    if Instant::now() >= deadline {
                        Err(WorkerError::Timeout)
                    } else {
                        Ok(())
                    }
                })?;
                return Ok(outcome);
            }
            if !cold {
                // No item in an already synchronized session is query-local
                // fallback, never evidence that the symbol has no callers.
                return Ok(QueryOutcome::ready(PreciseQueryResult::unavailable(
                    request.workspace.revision,
                    chakra_domain::state::ProviderState::Degraded,
                )));
            }
            channel.wait_until(deadline.min(Instant::now() + Duration::from_millis(250)))?;
        }
    }
}

/// Owned kotlin-lsp process and worker lifecycle.
pub struct KotlinLspProvider {
    inner: Arc<ProviderHandle<KotlinLspHooks>>,
}

impl std::fmt::Debug for KotlinLspProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, formatter)
    }
}

impl KotlinLspProvider {
    /// Starts the owner thread. A missing or failing kotlin-lsp does not
    /// fail Chakra startup: the handle transitions to `Degraded` and later
    /// queries retain syntax results.
    pub fn start(
        initial_workspace: chakra_engine::ProviderWorkspace,
        config: KotlinLspConfig,
    ) -> Result<Arc<Self>, StartError> {
        if config.readiness_timeout.is_zero() {
            return Err(StartError::InvalidTimeout);
        }
        let hooks = KotlinLspHooks {
            readiness_timeout: config.readiness_timeout,
            import_state: AtomicU8::new(IMPORT_PENDING),
            automatic_import: import::ProjectImport::default(),
        };
        let inner = ProviderHandle::start(initial_workspace, config.into(), hooks)?;
        Ok(Arc::new(Self { inner }))
    }

    /// Idempotent cooperative shutdown followed by joining the owned worker.
    /// The owned process group is terminated, so no kotlin-lsp child
    /// remains.
    pub fn shutdown(&self) -> Result<(), ShutdownError> {
        self.inner.shutdown()
    }
}

impl chakra_engine::PreciseProvider for KotlinLspProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn supports(&self, language: Language) -> bool {
        self.inner.supports(language)
    }

    fn supports_path(&self, language: Language, path: &RepoRelativePath) -> bool {
        self.inner.supports_path(language, path)
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

impl Drop for KotlinLspProvider {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Resolves the provider command: an explicit executable path first, then
/// side-effect-free `kotlin-lsp` discovery on `PATH`.
pub fn resolve_command(explicit: Option<&OsStr>) -> KotlinLspCommand {
    resolve_command_with_context(explicit, &OperationContext::unbounded())
        .unwrap_or_else(|_| KotlinLspCommand::stdio(OsString::from("kotlin-lsp")))
}

pub fn resolve_command_with_context(
    explicit: Option<&OsStr>,
    operation: &OperationContext,
) -> Result<KotlinLspCommand, OperationAbort> {
    operation.check()?;
    match explicit {
        Some(path) => Ok(KotlinLspCommand::stdio(path.to_owned())),
        None => Ok(KotlinLspCommand::discover_with_context(operation)?
            .unwrap_or_else(|| KotlinLspCommand::stdio(OsString::from("kotlin-lsp")))),
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
        let config = KotlinLspConfig::default();
        assert_eq!(
            config.command,
            KotlinLspCommand::stdio(OsString::from("kotlin-lsp"))
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
                path: RepoRelativePath::new("src/main/kotlin/Main.kt")?,
                source: Arc::from("package sample\n\nfun target() {}\n"),
                language: Language::Kotlin,
            }],
        );
        let provider = KotlinLspProvider::start(
            workspace.clone(),
            KotlinLspConfig {
                command: KotlinLspCommand::stdio(OsString::from(
                    "chakra-definitely-missing-kotlin-lsp",
                )),
                request_timeout: Duration::from_millis(100),
                barrier_timeout: Duration::from_millis(50),
                ..KotlinLspConfig::default()
            },
        )?;
        let result = provider.enrich(PreciseQueryRequest {
            workspace,
            symbol: ProviderSymbol {
                name: "target".to_owned(),
                declaration: SourceRange::new(
                    RepoRelativePath::new("src/main/kotlin/Main.kt")?,
                    TextPosition::new(3, 1)?,
                    TextPosition::new(3, 17)?,
                )?,
                language: Language::Kotlin,
            },
            directions: CallHierarchyDirections {
                incoming: true,
                outgoing: false,
            },
            limit: 20,
            priority: chakra_engine::ProviderRequestPriority::Normal,
        });
        assert_eq!(result.state, ProviderState::Degraded);
        assert_eq!(provider.name(), "kotlin-lsp");
        assert!(provider.supports(Language::Kotlin));
        assert!(!provider.supports(Language::Go));
        assert!(!provider.supports(Language::Java));
        provider.shutdown()?;
        Ok(())
    }
}
