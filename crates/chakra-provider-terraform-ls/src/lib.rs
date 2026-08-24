//! Live, optional terraform-ls adapter for HCL/Terraform precise reference
//! enrichment (ADR-0040).
//!
//! The language-neutral worker mechanics (session lifecycle, revision-scoped
//! document synchronization, the post-synchronization request barrier,
//! observability, restart, and shutdown) live in `chakra-provider-worker`
//! (issue #94); this crate keeps the Terraform-specific seams: command
//! discovery, defaults, terraform/terraform-vars/hcl language ids, the
//! definition/references/documentSymbol capability gate, and the
//! references-based query strategy with mid-query opening of caller
//! documents (terraform-ls has no callHierarchy; Chakra's bounded
//! Tree-sitter configuration-reference graph remains the outgoing
//! equivalent).
//!
//! An absent or failing terraform-ls never fails Chakra startup: the
//! provider transitions to `Degraded` and queries keep their syntax results
//! (ADR-0006/0013).

mod convert;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::provenance::Provenance;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{PreciseQueryRequest, PreciseQueryResult};
use chakra_provider_worker::{
    ProviderCommandSpec, ProviderHandle, ProviderHooks, QueryChannel, QueryDeadlines, QueryOutcome,
    WorkerConfig, WorkerError,
};
use lsp_types::{
    DocumentSymbolParams, DocumentSymbolResponse, InitializeResult, Location, OneOf,
    PartialResultParams, ReferenceContext, ReferenceParams, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

pub use chakra_provider_worker::{StartError, WorkerShutdownError as ShutdownError};

use convert::{
    CallerSymbol, convert_references, find_symbol_position, flat_caller_symbol,
    flatten_document_symbols,
};

const DEFAULT_COMMAND_CAPACITY: usize = 8;
const DEFAULT_MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_QUERY_WAIT_TIMEOUT: Duration = Duration::from_secs(1);
pub const COMMAND_DISCOVERY_TIMEOUT: Duration = Duration::ZERO;

/// Resolved `terraform-ls serve` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerraformLsCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl TerraformLsCommand {
    /// Explicit `terraform-ls serve` invocation.
    pub fn start(executable: impl Into<OsString>) -> Self {
        Self {
            program: executable.into(),
            args: vec![OsString::from("serve")],
        }
    }

    /// Side-effect-free discovery of `terraform-ls` on `PATH`.
    pub fn discover() -> Option<Self> {
        Self::discover_with_context(&OperationContext::unbounded())
            .ok()
            .flatten()
    }

    pub fn discover_with_context(
        operation: &OperationContext,
    ) -> Result<Option<Self>, OperationAbort> {
        operation.check()?;
        if let Some(executable) = find_on_path("terraform-ls") {
            return Ok(Some(Self::start(executable.into_os_string())));
        }
        Ok(None)
    }
}

impl From<TerraformLsCommand> for ProviderCommandSpec {
    fn from(command: TerraformLsCommand) -> Self {
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
pub struct TerraformLsConfig {
    pub command: TerraformLsCommand,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub barrier_timeout: Duration,
    pub query_wait_timeout: Duration,
    pub restart_base_delay: Duration,
    pub restart_max_delay: Duration,
    pub command_capacity: usize,
    pub max_message_bytes: usize,
}

impl Default for TerraformLsConfig {
    fn default() -> Self {
        Self {
            command: TerraformLsCommand::start(OsString::from("terraform-ls")),
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

impl From<TerraformLsConfig> for WorkerConfig {
    fn from(config: TerraformLsConfig) -> Self {
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

/// Terraform documents synchronized through terraform-ls.
fn terraform_ls_language(language: Language) -> bool {
    language == Language::Hcl
}

/// Terraform JSON variants are Chakra-side syntax facts only (issue #86).
fn is_terraform_json(path: &RepoRelativePath) -> bool {
    let path = path.as_str();
    path.ends_with(".tf.json") || path.ends_with(".tfvars.json") || path.ends_with(".tftest.json")
}

/// terraform-ls language hooks: HCL documents synchronize through the
/// session and the precise surface is references + document symbols.
#[derive(Debug, Clone, Copy, Default)]
struct TerraformLsHooks;

impl TerraformLsHooks {
    fn caller_symbols(
        channel: &mut dyn QueryChannel,
        request: &PreciseQueryRequest,
        references: &[Location],
        deadline: std::time::Instant,
    ) -> Result<Vec<CallerSymbol>, WorkerError> {
        let mut documents = BTreeMap::new();
        for reference in references {
            let Some(path) = chakra_provider_worker::convert::uri_to_path(
                &request.workspace.repository_root,
                &reference.uri,
            ) else {
                continue;
            };
            if request
                .workspace
                .document(&path)
                .is_none_or(|document| !terraform_ls_language(document.language))
            {
                continue;
            }
            documents
                .entry(path)
                .or_insert_with(|| reference.uri.clone());
        }
        let mut callers = Vec::new();
        for (path, uri) in documents {
            // Caller documents discovered through references must be opened
            // before their documentSymbol request; the channel bumps the
            // synchronization generation and records the metrics.
            channel.open_document(&path, deadline)?;
            let params = DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            let value = channel.request(
                "textDocument/documentSymbol",
                &serde_json::to_value(params)?,
                deadline,
            )?;
            let response = serde_json::from_value::<Option<DocumentSymbolResponse>>(value)?
                .unwrap_or(DocumentSymbolResponse::Flat(Vec::new()));
            match response {
                DocumentSymbolResponse::Flat(symbols) => {
                    callers.extend(symbols.into_iter().filter_map(|symbol| {
                        flat_caller_symbol(symbol, &path, &request.workspace)
                    }));
                }
                DocumentSymbolResponse::Nested(symbols) => {
                    flatten_document_symbols(&path, symbols, &mut callers);
                }
            }
        }
        Ok(callers)
    }
}

impl ProviderHooks for TerraformLsHooks {
    fn name(&self) -> &'static str {
        "terraform-ls"
    }

    fn provenance(&self) -> Provenance {
        Provenance::TerraformLs
    }

    fn synchronizes(&self, language: Language) -> bool {
        terraform_ls_language(language)
    }

    /// terraform-ls is not equipped to parse Terraform JSON variants
    /// (.tf.json/.tfvars.json/.tftest.json); they stay Chakra-side syntax
    /// facts and are never sent to the server (issue #86).
    fn synchronizes_path(&self, language: Language, path: &RepoRelativePath) -> bool {
        terraform_ls_language(language) && !is_terraform_json(path)
    }

    fn language_id(&self, path: &RepoRelativePath) -> &'static str {
        let path = path.as_str();
        if path.ends_with(".tfvars") {
            "terraform-vars"
        } else if path.ends_with(".tf") || path.ends_with(".tftest.hcl") {
            "terraform"
        } else {
            "hcl"
        }
    }

    fn verify_capabilities(&self, result: &InitializeResult) -> Result<(), WorkerError> {
        let supports_definition = matches!(
            result.capabilities.definition_provider,
            Some(OneOf::Left(true)) | Some(OneOf::Right(_))
        );
        let supports_references = matches!(
            result.capabilities.references_provider,
            Some(OneOf::Left(true)) | Some(OneOf::Right(_))
        );
        let supports_document_symbols = matches!(
            result.capabilities.document_symbol_provider,
            Some(OneOf::Left(true)) | Some(OneOf::Right(_))
        );
        if supports_definition && supports_references && supports_document_symbols {
            Ok(())
        } else {
            Err(WorkerError::Unsupported(
                "definition, references, and document symbols".to_owned(),
            ))
        }
    }

    fn query(
        &self,
        channel: &mut dyn QueryChannel,
        request: &PreciseQueryRequest,
        deadlines: QueryDeadlines,
    ) -> Result<QueryOutcome, WorkerError> {
        let document = request
            .workspace
            .document(request.symbol.declaration.file())
            .ok_or(WorkerError::InvalidPosition)?;
        let position = find_symbol_position(
            &document.source,
            &request.symbol.name,
            &request.symbol.declaration,
        )?;
        let uri = chakra_provider_worker::convert::path_to_uri(
            &request.workspace.repository_root,
            request.symbol.declaration.file(),
        )?;
        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: false,
            },
        };
        // The final references/document-symbol round trip is the
        // post-synchronization barrier (the core records round-trips).
        let value = channel.request(
            "textDocument/references",
            &serde_json::to_value(params)?,
            deadlines.readiness,
        )?;
        let references =
            serde_json::from_value::<Option<Vec<Location>>>(value)?.unwrap_or_default();
        let symbols = if request.directions.incoming {
            Self::caller_symbols(channel, request, &references, deadlines.request)?
        } else {
            Vec::new()
        };
        let mut incoming_truncated = false;
        let incoming = if request.directions.incoming {
            convert_references(
                references,
                &symbols,
                &request.workspace,
                request.limit,
                &mut incoming_truncated,
            )
        } else {
            Vec::new()
        };
        Ok(QueryOutcome::ready(PreciseQueryResult {
            revision: request.workspace.revision,
            state: ProviderState::Ready,
            fallback_cause: None,
            incoming,
            outgoing: Vec::new(),
            incoming_truncated,
            outgoing_truncated: false,
        }))
    }
}

/// Owned terraform-ls process and worker lifecycle.
pub struct TerraformLsProvider {
    inner: Arc<ProviderHandle<TerraformLsHooks>>,
}

impl std::fmt::Debug for TerraformLsProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, formatter)
    }
}

impl TerraformLsProvider {
    /// Starts the owner thread. A missing or failing terraform-ls does not
    /// fail Chakra startup: the handle transitions to `Degraded` and later
    /// queries retain syntax results.
    pub fn start(
        initial_workspace: chakra_engine::ProviderWorkspace,
        config: TerraformLsConfig,
    ) -> Result<Arc<Self>, StartError> {
        let inner = ProviderHandle::start(initial_workspace, config.into(), TerraformLsHooks)?;
        Ok(Arc::new(Self { inner }))
    }

    /// Idempotent cooperative shutdown followed by joining the owned worker.
    /// The owned process group is terminated, so no terraform-ls child
    /// remains.
    pub fn shutdown(&self) -> Result<(), ShutdownError> {
        self.inner.shutdown()
    }
}

impl chakra_engine::PreciseProvider for TerraformLsProvider {
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

impl Drop for TerraformLsProvider {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Resolves the provider command: an explicit executable path first, then
/// side-effect-free `terraform-ls` discovery on `PATH`.
pub fn resolve_command(explicit: Option<&OsStr>) -> TerraformLsCommand {
    resolve_command_with_context(explicit, &OperationContext::unbounded())
        .unwrap_or_else(|_| TerraformLsCommand::start(OsString::from("terraform-ls")))
}

pub fn resolve_command_with_context(
    explicit: Option<&OsStr>,
    operation: &OperationContext,
) -> Result<TerraformLsCommand, OperationAbort> {
    operation.check()?;
    match explicit {
        Some(path) => Ok(TerraformLsCommand::start(path.to_owned())),
        None => Ok(TerraformLsCommand::discover_with_context(operation)?
            .unwrap_or_else(|| TerraformLsCommand::start(OsString::from("terraform-ls")))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
    use chakra_domain::revision::Revision;
    use chakra_engine::{
        CallHierarchyDirections, PreciseProvider, ProviderDocument, ProviderSymbol,
    };

    #[test]
    fn default_config_uses_a_side_effect_free_command_fallback() {
        let config = TerraformLsConfig::default();
        assert_eq!(
            config.command,
            TerraformLsCommand::start(OsString::from("terraform-ls"))
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
    fn language_ids_follow_the_file_extension() -> Result<(), Box<dyn std::error::Error>> {
        let hooks = TerraformLsHooks;
        assert_eq!(
            hooks.language_id(&RepoRelativePath::new("main.tf")?),
            "terraform"
        );
        assert_eq!(
            hooks.language_id(&RepoRelativePath::new("tests/main.tftest.hcl")?),
            "terraform"
        );
        assert_eq!(
            hooks.language_id(&RepoRelativePath::new("vars.tfvars")?),
            "terraform-vars"
        );
        assert_eq!(hooks.language_id(&RepoRelativePath::new("mod.hcl")?), "hcl");
        Ok(())
    }

    #[test]
    fn terraform_json_documents_are_never_synchronized() -> Result<(), Box<dyn std::error::Error>> {
        let hooks = TerraformLsHooks;
        assert!(hooks.synchronizes_path(Language::Hcl, &RepoRelativePath::new("main.tf")?));
        assert!(!hooks.synchronizes_path(Language::Hcl, &RepoRelativePath::new("main.tf.json")?));
        assert!(
            !hooks.synchronizes_path(Language::Hcl, &RepoRelativePath::new("prod.tfvars.json")?)
        );
        assert!(!hooks.synchronizes_path(
            Language::Hcl,
            &RepoRelativePath::new("tests/x.tftest.json")?
        ));
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
                path: RepoRelativePath::new("main.tf")?,
                source: Arc::from("variable \"target\" { default = \"ok\" }\n"),
                language: Language::Hcl,
            }],
        );
        let provider = TerraformLsProvider::start(
            workspace.clone(),
            TerraformLsConfig {
                command: TerraformLsCommand::start(OsString::from(
                    "chakra-definitely-missing-terraform-ls",
                )),
                request_timeout: Duration::from_millis(100),
                barrier_timeout: Duration::from_millis(50),
                ..TerraformLsConfig::default()
            },
        )?;
        let result = provider.enrich(PreciseQueryRequest {
            workspace,
            symbol: ProviderSymbol {
                name: "target".to_owned(),
                declaration: SourceRange::new(
                    RepoRelativePath::new("main.tf")?,
                    TextPosition::new(1, 1)?,
                    TextPosition::new(1, 29)?,
                )?,
                language: Language::Hcl,
            },
            directions: CallHierarchyDirections {
                incoming: true,
                outgoing: false,
            },
            limit: 20,
            priority: chakra_engine::ProviderRequestPriority::Normal,
        });
        assert_eq!(result.state, ProviderState::Degraded);
        assert_eq!(provider.name(), "terraform-ls");
        assert!(provider.supports(Language::Hcl));
        assert!(!provider.supports(Language::TypeScript));
        assert!(!provider.supports(Language::Rust));
        provider.shutdown()?;
        Ok(())
    }
}
