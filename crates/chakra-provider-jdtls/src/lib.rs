//! Live, optional jdtls adapter for Java precise enrichment (ADR-0032/0036).
//!
//! The language-neutral worker mechanics (session lifecycle, revision-scoped
//! document synchronization, the post-synchronization request barrier,
//! observability, restart, and shutdown) live in `chakra-provider-worker`
//! (issue #94); this crate keeps the jdtls-specific seams: launcher
//! discovery, the mandatory per-workspace `-data` directory with its safety
//! rules (ADR-0036), the generous cold-import readiness budget, and the
//! call-hierarchy capability gate.
//!
//! Only the v0.1 call-hierarchy operations cross this adapter internally.
//! Public contracts are Chakra-native, so LSP URIs, UTF-16 positions, and
//! protocol lifecycle details remain confined to the adapter crates and
//! chakra-lsp.
//!
//! An absent or failing jdtls never fails Chakra startup: the provider
//! transitions to `Degraded` and queries keep their syntax results
//! (ADR-0006/0013).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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

pub use chakra_provider_worker::WorkerShutdownError as ShutdownError;

const DEFAULT_COMMAND_CAPACITY: usize = 8;
const DEFAULT_MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_QUERY_WAIT_TIMEOUT: Duration = Duration::from_secs(1);

/// Resolved jdtls invocation. jdtls is a JVM application: both the `jdtls`
/// launcher script and the `jdt-language-server` binary accept the same
/// flags, including the mandatory per-workspace `-data` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JdtlsCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    bind_workspace_data_dir: bool,
}

impl JdtlsCommand {
    /// Invocation of one launcher executable with the per-workspace data
    /// directory (`-data <dir>`).
    pub fn stdio(executable: impl Into<OsString>, data_dir: impl Into<OsString>) -> Self {
        Self {
            program: executable.into(),
            args: vec![OsString::from("-data"), data_dir.into()],
            bind_workspace_data_dir: false,
        }
    }

    /// Invocation whose mandatory `-data` directory is derived from the
    /// workspace root when the provider starts. This is the safe constructor
    /// for defaults and callers that do not need an explicit cache location.
    pub fn for_workspace(executable: impl Into<OsString>) -> Self {
        Self {
            program: executable.into(),
            args: Vec::new(),
            bind_workspace_data_dir: true,
        }
    }

    /// Best-effort discovery: a `jdtls` executable on `PATH` first, then a
    /// `jdt-language-server` executable. Returns `None` when neither is
    /// available; callers then start the provider degraded.
    pub fn discover(data_dir: &Path) -> Option<Self> {
        if let Some(executable) = find_on_path("jdtls") {
            return Some(Self::stdio(
                executable.into_os_string(),
                data_dir.as_os_str().to_owned(),
            ));
        }
        let executable = find_on_path("jdt-language-server")?;
        Some(Self::stdio(
            executable.into_os_string(),
            data_dir.as_os_str().to_owned(),
        ))
    }

    fn bind_to_workspace(mut self, repository_root: &Path) -> Self {
        if self.bind_workspace_data_dir {
            self.args = vec![
                OsString::from("-data"),
                workspace_data_dir(repository_root).into_os_string(),
            ];
            self.bind_workspace_data_dir = false;
        }
        self
    }

    fn data_dir(&self) -> Option<&Path> {
        let mut occurrences = self
            .args
            .iter()
            .enumerate()
            .filter(|(_, argument)| *argument == "-data");
        let (index, _) = occurrences.next()?;
        if occurrences.next().is_some() {
            return None;
        }
        self.args.get(index + 1).map(Path::new)
    }
}

impl From<JdtlsCommand> for ProviderCommandSpec {
    fn from(command: JdtlsCommand) -> Self {
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

/// FNV-1a over the workspace root bytes: a deterministic, dependency-free
/// key for the per-workspace data directory name.
fn workspace_hash(root: &Path) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn data_dir_is_safe(repository_root: &Path, data_dir: &Path) -> bool {
    if !data_dir.is_absolute() || data_dir.starts_with(repository_root) {
        return false;
    }
    let Ok(canonical_root) = std::fs::canonicalize(repository_root) else {
        return false;
    };
    let mut existing = Some(data_dir);
    while existing.is_some_and(|candidate| !candidate.exists()) {
        existing = existing.and_then(Path::parent);
    }
    existing
        .and_then(|ancestor| std::fs::canonicalize(ancestor).ok())
        .is_some_and(|ancestor| !ancestor.starts_with(canonical_root))
}

/// The jdtls per-workspace data directory: under the OS temporary directory,
/// keyed by the workspace path, never inside the repository (ADR-0036).
pub fn workspace_data_dir(repository_root: &Path) -> PathBuf {
    std::env::temp_dir().join(format!(
        "chakra-jdtls-{:016x}",
        workspace_hash(repository_root)
    ))
}

/// Process and bounded-wait settings for the optional provider.
#[derive(Debug, Clone)]
pub struct JdtlsConfig {
    pub command: JdtlsCommand,
    pub startup_timeout: Duration,
    /// Bound for the post-synchronization readiness barrier. jdtls imports
    /// the project (Maven/Gradle model, JDK index) before call hierarchy
    /// answers honestly; the first import can take minutes on a cold data
    /// directory, so the default is generous and the value is configurable.
    pub readiness_timeout: Duration,
    pub request_timeout: Duration,
    pub barrier_timeout: Duration,
    pub query_wait_timeout: Duration,
    pub restart_base_delay: Duration,
    pub restart_max_delay: Duration,
    pub command_capacity: usize,
    pub max_message_bytes: usize,
}

impl Default for JdtlsConfig {
    fn default() -> Self {
        Self {
            command: JdtlsCommand::for_workspace(OsString::from("jdtls")),
            startup_timeout: Duration::from_secs(120),
            readiness_timeout: Duration::from_secs(180),
            request_timeout: Duration::from_secs(10),
            barrier_timeout: Duration::from_secs(2),
            query_wait_timeout: DEFAULT_QUERY_WAIT_TIMEOUT,
            restart_base_delay: Duration::from_millis(200),
            restart_max_delay: Duration::from_secs(2),
            command_capacity: DEFAULT_COMMAND_CAPACITY,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

impl JdtlsConfig {
    fn worker_config(&self) -> WorkerConfig {
        WorkerConfig {
            command: self.command.clone().into(),
            startup_timeout: self.startup_timeout,
            request_timeout: self.request_timeout,
            barrier_timeout: self.barrier_timeout,
            query_wait_timeout: self.query_wait_timeout,
            restart_base_delay: self.restart_base_delay,
            restart_max_delay: self.restart_max_delay,
            command_capacity: self.command_capacity,
            max_message_bytes: self.max_message_bytes,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("provider command capacity and message size bound must be non-zero")]
    InvalidCapacity,
    #[error("provider startup, readiness, request, and barrier timeouts must be non-zero")]
    InvalidTimeout,
    #[error("failed to spawn the jdtls owner thread: {0}")]
    ThreadSpawn(#[source] std::io::Error),
    #[error(
        "jdtls command must provide exactly one -data argument with an absolute directory outside the repository"
    )]
    UnsafeDataDirectory,
}

impl From<chakra_provider_worker::StartError> for StartError {
    fn from(error: chakra_provider_worker::StartError) -> Self {
        match error {
            chakra_provider_worker::StartError::InvalidCapacity => Self::InvalidCapacity,
            chakra_provider_worker::StartError::InvalidTimeout => Self::InvalidTimeout,
            chakra_provider_worker::StartError::ThreadSpawn(source) => Self::ThreadSpawn(source),
        }
    }
}

/// jdtls language hooks: Java documents synchronize through the session, the
/// precise surface is the LSP call-hierarchy trio, the barrier-proving
/// prepare round-trips get the generous cold-import readiness budget, and the
/// per-workspace data directory is prepared before the JVM starts.
#[derive(Debug)]
struct JdtlsHooks {
    readiness_timeout: Duration,
    data_dir: PathBuf,
}

impl ProviderHooks for JdtlsHooks {
    fn name(&self) -> &'static str {
        "jdtls"
    }

    fn provenance(&self) -> Provenance {
        Provenance::Jdtls
    }

    fn synchronizes(&self, language: Language) -> bool {
        language == Language::Java
    }

    fn language_id(&self, _path: &RepoRelativePath) -> &'static str {
        "java"
    }

    fn readiness_timeout(&self) -> Option<Duration> {
        Some(self.readiness_timeout)
    }

    fn cold_start_outlives_caller_wait(&self) -> bool {
        true
    }

    fn before_session_start(&self) -> Result<(), WorkerError> {
        // jdtls writes its project model under `-data`; create the
        // per-workspace data directory (outside the repository, ADR-0036) so
        // a missing parent never surfaces as an opaque server failure.
        std::fs::create_dir_all(&self.data_dir).map_err(|error| {
            WorkerError::transport(
                "jdtls",
                format!(
                    "failed to create the jdtls data directory {}: {error}",
                    self.data_dir.display()
                ),
            )
        })
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
        CallHierarchyDriver.query(channel, request, deadlines, Provenance::Jdtls)
    }
}

/// Owned jdtls process and worker lifecycle.
pub struct JdtlsProvider {
    inner: Arc<ProviderHandle<JdtlsHooks>>,
}

impl std::fmt::Debug for JdtlsProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, formatter)
    }
}

impl JdtlsProvider {
    /// Starts the owner thread. A missing or failing jdtls does not fail
    /// Chakra startup: the handle transitions to `Degraded` and later queries
    /// retain syntax results.
    pub fn start(
        initial_workspace: chakra_engine::ProviderWorkspace,
        mut config: JdtlsConfig,
    ) -> Result<Arc<Self>, StartError> {
        if config.readiness_timeout.is_zero() {
            return Err(StartError::InvalidTimeout);
        }
        config.command = config
            .command
            .bind_to_workspace(&initial_workspace.repository_root);
        let data_dir = config
            .command
            .data_dir()
            .ok_or(StartError::UnsafeDataDirectory)?;
        if !data_dir_is_safe(&initial_workspace.repository_root, data_dir) {
            return Err(StartError::UnsafeDataDirectory);
        }
        let hooks = JdtlsHooks {
            readiness_timeout: config.readiness_timeout,
            data_dir: data_dir.to_path_buf(),
        };
        let inner = ProviderHandle::start(initial_workspace, config.worker_config(), hooks)?;
        Ok(Arc::new(Self { inner }))
    }

    /// Idempotent cooperative shutdown followed by joining the owned worker.
    /// The owned process group is terminated, so no JVM child remains.
    pub fn shutdown(&self) -> Result<(), ShutdownError> {
        self.inner.shutdown()
    }
}

impl chakra_engine::PreciseProvider for JdtlsProvider {
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

impl Drop for JdtlsProvider {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Resolves the provider command: an explicit executable path first, then
/// side-effect-free `jdtls`/`jdt-language-server` discovery on `PATH` with
/// the per-workspace tempdir location (ADR-0036). Exposed for CLI wiring and
/// diagnostics.
pub fn resolve_command(explicit: Option<&OsStr>, repository_root: &Path) -> JdtlsCommand {
    resolve_command_with_context(explicit, repository_root, &OperationContext::unbounded())
        .unwrap_or_else(|_| JdtlsCommand::for_workspace(OsString::from("jdtls")))
}

pub fn resolve_command_with_context(
    explicit: Option<&OsStr>,
    repository_root: &Path,
    operation: &OperationContext,
) -> Result<JdtlsCommand, OperationAbort> {
    operation.check()?;
    let data_dir = workspace_data_dir(repository_root);
    let command = match explicit {
        Some(path) => JdtlsCommand::stdio(path.to_owned(), data_dir.into_os_string()),
        None => JdtlsCommand::discover(&data_dir).unwrap_or_else(|| {
            JdtlsCommand::stdio(OsString::from("jdtls"), data_dir.into_os_string())
        }),
    };
    operation.check()?;
    Ok(command)
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
    fn command_resolution_observes_the_caller_deadline() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let operation = OperationContext::with_timeout(Duration::ZERO);
        assert_eq!(
            resolve_command_with_context(None, root.path(), &operation),
            Err(OperationAbort::DeadlineExceeded)
        );
        Ok(())
    }

    #[test]
    fn missing_executable_degrades_without_failing_queries()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let workspace = chakra_engine::ProviderWorkspace::from_documents(
            root.path().to_path_buf(),
            Revision(1),
            vec![ProviderDocument {
                path: RepoRelativePath::new("src/Main.java")?,
                source: Arc::from("class Main {\n    void target() {}\n}\n"),
                language: Language::Java,
            }],
        );
        let provider = JdtlsProvider::start(
            workspace.clone(),
            JdtlsConfig {
                command: JdtlsCommand::for_workspace(OsString::from(
                    "chakra-definitely-missing-jdtls",
                )),
                request_timeout: Duration::from_millis(100),
                barrier_timeout: Duration::from_millis(50),
                ..JdtlsConfig::default()
            },
        )?;
        let result = provider.enrich(PreciseQueryRequest {
            workspace,
            symbol: ProviderSymbol {
                name: "target".to_owned(),
                declaration: SourceRange::new(
                    RepoRelativePath::new("src/Main.java")?,
                    TextPosition::new(2, 5)?,
                    TextPosition::new(2, 24)?,
                )?,
                language: Language::Java,
            },
            directions: CallHierarchyDirections {
                incoming: true,
                outgoing: false,
            },
            limit: 20,
            priority: chakra_engine::ProviderRequestPriority::Normal,
        });
        assert_eq!(result.state, ProviderState::Degraded);
        assert_eq!(provider.name(), "jdtls");
        assert!(provider.supports(Language::Java));
        assert!(!provider.supports(Language::Rust));
        provider.shutdown()?;
        assert!(!root.path().join("jdtls-data").exists());
        Ok(())
    }

    #[test]
    fn workspace_data_dir_stays_outside_the_repository() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let data_dir = workspace_data_dir(root.path());
        assert!(data_dir.starts_with(std::env::temp_dir()));
        assert!(!data_dir.starts_with(root.path()));
        assert_eq!(data_dir, workspace_data_dir(root.path()));
        Ok(())
    }

    #[test]
    fn default_command_binds_distinct_workspace_safe_data_directories()
    -> Result<(), Box<dyn std::error::Error>> {
        let first_root = tempfile::tempdir()?;
        let second_root = tempfile::tempdir()?;
        let first = JdtlsConfig::default()
            .command
            .bind_to_workspace(first_root.path());
        let second = JdtlsConfig::default()
            .command
            .bind_to_workspace(second_root.path());
        let first_data = first.data_dir().ok_or("first data directory missing")?;
        let second_data = second.data_dir().ok_or("second data directory missing")?;

        assert!(data_dir_is_safe(first_root.path(), first_data));
        assert!(data_dir_is_safe(second_root.path(), second_data));
        assert_ne!(first_data, second_data);
        Ok(())
    }

    #[test]
    fn provider_rejects_relative_or_repository_contained_data_directories()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let workspace = chakra_engine::ProviderWorkspace::from_documents(
            root.path().to_path_buf(),
            Revision(1),
            Vec::new(),
        );
        for data_dir in [
            PathBuf::from("relative-data"),
            root.path().join("provider-data"),
        ] {
            let result = JdtlsProvider::start(
                workspace.clone(),
                JdtlsConfig {
                    command: JdtlsCommand::stdio(
                        OsString::from("chakra-definitely-missing-jdtls"),
                        data_dir.into_os_string(),
                    ),
                    ..JdtlsConfig::default()
                },
            );
            assert!(matches!(result, Err(StartError::UnsafeDataDirectory)));
        }
        Ok(())
    }

    #[test]
    fn provider_rejects_missing_or_duplicate_data_arguments()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let safe = tempfile::tempdir()?;
        let workspace = chakra_engine::ProviderWorkspace::from_documents(
            root.path().to_path_buf(),
            Revision(1),
            Vec::new(),
        );

        let mut missing = JdtlsCommand::stdio(
            OsString::from("chakra-definitely-missing-jdtls"),
            safe.path().join("cache").into_os_string(),
        );
        missing.args = vec![OsString::from("-data")];

        let mut duplicate = JdtlsCommand::stdio(
            OsString::from("chakra-definitely-missing-jdtls"),
            safe.path().join("cache").into_os_string(),
        );
        duplicate.args.extend([
            OsString::from("-data"),
            root.path().join("provider-data").into_os_string(),
        ]);

        for command in [missing, duplicate] {
            let result = JdtlsProvider::start(
                workspace.clone(),
                JdtlsConfig {
                    command,
                    ..JdtlsConfig::default()
                },
            );
            assert!(matches!(result, Err(StartError::UnsafeDataDirectory)));
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn provider_rejects_an_outside_symlink_into_the_repository()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::os::unix::fs::symlink(root.path(), outside.path().join("into-repository"))?;
        let workspace = chakra_engine::ProviderWorkspace::from_documents(
            root.path().to_path_buf(),
            Revision(1),
            Vec::new(),
        );
        let result = JdtlsProvider::start(
            workspace,
            JdtlsConfig {
                command: JdtlsCommand::stdio(
                    OsString::from("chakra-definitely-missing-jdtls"),
                    outside
                        .path()
                        .join("into-repository/cache")
                        .into_os_string(),
                ),
                ..JdtlsConfig::default()
            },
        );
        assert!(matches!(result, Err(StartError::UnsafeDataDirectory)));
        Ok(())
    }
}
