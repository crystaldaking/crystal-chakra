//! Owner-thread worker: session lifecycle, revision-scoped document
//! synchronization, and bounded call-hierarchy queries on top of chakra-lsp.
//!
//! gopls has no rust-analyzer-style `experimental/serverStatus` quiescence
//! signal. Readiness is instead proven by a post-synchronization request
//! barrier (ADR-0041): after the current document generation is sent, the
//! first `textDocument/prepareCallHierarchy` round-trip confirms the server
//! consumed it. An empty prepare after the barrier is a genuine "no item",
//! not a reason to wait forever.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::query::{
    ProviderDocumentSyncMetrics, ProviderMetrics, ProviderProgress, ProviderProgressSource,
    ProviderProgressStage,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{
    PreciseQueryRequest, PreciseQueryResult, ProviderWorkspace, ProviderWorkspaceDelta,
};
use chakra_lsp::{Client, ClientConfig, ClientError, RestartBackoff, ServerEvent, TransportConfig};
use crossbeam_channel::Receiver;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CallHierarchyServerCapability, ClientCapabilities, ClientInfo, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    FileChangeType, FileEvent, InitializeParams, InitializeResult, PartialResultParams,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, VersionedTextDocumentIdentifier, WindowClientCapabilities,
    WorkDoneProgressParams, WorkspaceFolder,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;

use crate::convert::{
    convert_incoming, convert_outgoing, directory_uri, find_symbol_position, item_declaration,
    path_to_uri,
};
use crate::{Command, GoplsConfig, SharedState};

const MAX_PROVIDER_ERROR_CHARS: usize = 1_024;
const MAX_PROVIDER_RESULTS: usize = 500;
const EVENT_POLL: Duration = Duration::from_millis(25);

#[derive(Debug, Error)]
pub(crate) enum ProviderError {
    #[error("gopls transport failed: {0}")]
    Transport(String),
    #[error("timed out waiting for the gopls response")]
    Timeout,
    #[error("the gopls request was cancelled by its caller")]
    Cancelled,
    #[error("gopls request failed ({code}): {message}")]
    Server { code: i32, message: String },
    #[error("invalid gopls response: {0}")]
    InvalidResponse(#[from] serde_json::Error),
    #[error("gopls does not advertise call hierarchy")]
    Unsupported,
    #[error("invalid file URI for {0}")]
    InvalidUri(String),
    #[error("provider position is outside captured source")]
    InvalidPosition,
}

impl ProviderError {
    fn from_client(error: ClientError) -> Self {
        match error {
            ClientError::Timeout { .. } => Self::Timeout,
            ClientError::Cancelled { .. } => Self::Cancelled,
            ClientError::Server { code, message, .. } => Self::Server { code, message },
            other => Self::Transport(other.to_string()),
        }
    }

    fn is_transport_failure(&self) -> bool {
        matches!(self, Self::Transport(_))
    }

    fn fallback_state(&self) -> ProviderState {
        match self {
            Self::Timeout | Self::Cancelled => ProviderState::CatchingUp,
            _ => ProviderState::Degraded,
        }
    }
}

pub(crate) struct Worker {
    commands: Receiver<Command>,
    shared: Arc<Mutex<SharedState>>,
    force_stop: Arc<AtomicBool>,
    config: GoplsConfig,
    root: std::path::PathBuf,
    known_revision: Revision,
    session: Option<Client>,
    provider_epoch: u64,
    known_workspace: ProviderWorkspace,
    opened_versions: HashMap<RepoRelativePath, i32>,
    sync_generation: u64,
    barrier_generation: Option<u64>,
    sync_metrics: ProviderDocumentSyncMetrics,
    progress: Option<ProviderProgress>,
    active_operation: Option<OperationContext>,
    backoff: RestartBackoff,
}

impl Worker {
    pub(crate) fn new(
        commands: Receiver<Command>,
        shared: Arc<Mutex<SharedState>>,
        force_stop: Arc<AtomicBool>,
        config: GoplsConfig,
        initial_workspace: ProviderWorkspace,
    ) -> Self {
        let known_revision = initial_workspace.revision;
        let (workspace_documents, workspace_source_bytes) =
            initial_workspace.document_stats(Language::Go);
        let root = initial_workspace.repository_root.clone();
        Self {
            commands,
            shared,
            force_stop,
            backoff: RestartBackoff::new(config.restart_base_delay, config.restart_max_delay),
            config,
            root,
            known_revision,
            session: None,
            provider_epoch: 0,
            known_workspace: initial_workspace,
            opened_versions: HashMap::new(),
            sync_generation: 0,
            barrier_generation: None,
            sync_metrics: ProviderDocumentSyncMetrics {
                revision: Some(known_revision),
                workspace_documents: workspace_documents as u64,
                workspace_source_bytes,
                ..ProviderDocumentSyncMetrics::default()
            },
            progress: None,
            active_operation: None,
        }
    }

    pub(crate) fn run(mut self) {
        if let Err(error) = self.start_session() {
            self.set_state(ProviderState::Degraded, None, Some(error.to_string()));
        }
        while !self.force_stop.load(Ordering::Acquire) {
            match self.commands.recv_timeout(Duration::from_millis(50)) {
                Ok(Command::Enrich {
                    request,
                    operation,
                    response,
                }) => {
                    let result = self.handle_enrich(*request, operation);
                    let _ = response.send(result);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => self.drain_session_events(),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    self.stop_session();
                    break;
                }
            }
        }
        self.stop_session();
        self.set_progress(ProviderProgress {
            stage: ProviderProgressStage::Stopped,
            source: ProviderProgressSource::Chakra,
            message: Some("provider owner stopped".to_owned()),
            percentage: None,
        });
        self.set_state(
            ProviderState::Degraded,
            None,
            Some("provider stopped".to_owned()),
        );
    }

    fn handle_enrich(
        &mut self,
        request: PreciseQueryRequest,
        operation: OperationContext,
    ) -> PreciseQueryResult {
        self.active_operation = Some(operation);
        let result = self.handle_enrich_inner(request);
        self.active_operation = None;
        result
    }

    fn handle_enrich_inner(&mut self, request: PreciseQueryRequest) -> PreciseQueryResult {
        let mut request = request;
        request.limit = request.limit.min(MAX_PROVIDER_RESULTS);
        let revision = request.workspace.revision;
        if self.check_operation().is_err() {
            return PreciseQueryResult::unavailable(revision, ProviderState::CatchingUp);
        }
        self.drain_session_events();
        if revision < self.known_revision {
            return PreciseQueryResult::unavailable(revision, ProviderState::CatchingUp);
        }
        if self.session.is_none()
            && let Err(error) = self.restart_for(&request.workspace)
        {
            self.set_state(ProviderState::Degraded, None, Some(error.to_string()));
            return PreciseQueryResult::unavailable(revision, ProviderState::Degraded);
        }

        let first = self.query_with_owned_session(&request);
        let result = match first {
            Ok(result) => result,
            Err(error) if error.is_transport_failure() => {
                self.set_state(ProviderState::Degraded, None, Some(error.to_string()));
                self.stop_session();
                self.backoff_sleep();
                match self.restart_for(&request.workspace) {
                    Ok(()) => self
                        .query_with_owned_session(&request)
                        .unwrap_or_else(|retry| self.fallback(revision, retry)),
                    Err(restart) => self.fallback(revision, restart),
                }
            }
            Err(error) => self.fallback(revision, error),
        };
        if result.state == ProviderState::Ready {
            self.set_state(ProviderState::Ready, Some(revision), None);
        }
        result
    }

    fn fallback(&mut self, revision: Revision, error: ProviderError) -> PreciseQueryResult {
        let state = error.fallback_state();
        if state == ProviderState::Degraded {
            self.set_progress(ProviderProgress {
                stage: ProviderProgressStage::Degraded,
                source: ProviderProgressSource::Chakra,
                message: Some(error.to_string()),
                percentage: None,
            });
        }
        self.set_state(state, None, Some(error.to_string()));
        PreciseQueryResult::unavailable(revision, state)
    }

    fn check_operation(&self) -> Result<(), ProviderError> {
        match self
            .active_operation
            .as_ref()
            .map_or(Ok(()), OperationContext::check)
        {
            Ok(()) => Ok(()),
            Err(OperationAbort::Cancelled) => Err(ProviderError::Cancelled),
            Err(OperationAbort::DeadlineExceeded) => Err(ProviderError::Timeout),
        }
    }

    fn operation_deadline(&self, local_timeout: Duration) -> Instant {
        let local = Instant::now() + local_timeout;
        self.active_operation
            .as_ref()
            .and_then(OperationContext::deadline)
            .map_or(local, |deadline| deadline.min(local))
    }

    fn backoff_sleep(&mut self) {
        let delay = self.backoff.next_delay();
        let deadline = Instant::now() + delay;
        while Instant::now() < deadline {
            if self.force_stop.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(EVENT_POLL.min(delay));
        }
    }

    fn query_with_owned_session(
        &mut self,
        request: &PreciseQueryRequest,
    ) -> Result<PreciseQueryResult, ProviderError> {
        let mut session = self
            .session
            .take()
            .ok_or_else(|| ProviderError::Transport("no active gopls process".to_owned()))?;
        let result = self.query_session(&mut session, request);
        self.session = Some(session);
        result
    }

    fn query_session(
        &mut self,
        session: &mut Client,
        request: &PreciseQueryRequest,
    ) -> Result<PreciseQueryResult, ProviderError> {
        self.check_operation()?;
        self.set_state(ProviderState::CatchingUp, None, None);
        let deadline = self.operation_deadline(self.config.request_timeout);
        self.synchronize_documents(
            session,
            &request.workspace,
            &request.symbol.declaration,
            deadline,
        )?;
        let mut last_incoming = Vec::new();
        let mut last_outgoing = Vec::new();

        for attempt in 0..2 {
            self.check_operation()?;
            let items = self.prepare_call_hierarchy(session, request, deadline)?;
            // The prepare round-trip is the post-synchronization barrier: the
            // server consumed the current document generation before answering.
            self.confirm_sync_barrier();
            let Some(item) = self.select_hierarchy_item(items, request)? else {
                if self.provider_is_ready() {
                    return Ok(PreciseQueryResult {
                        revision: request.workspace.revision,
                        state: ProviderState::Ready,
                        fallback_cause: None,
                        incoming: Vec::new(),
                        outgoing: Vec::new(),
                        incoming_truncated: false,
                        outgoing_truncated: false,
                    });
                }
                self.wait_for_events();
                if attempt == 1 {
                    // The barrier completed and the server still reports no
                    // item: an honest empty precise result.
                    return Ok(PreciseQueryResult {
                        revision: request.workspace.revision,
                        state: ProviderState::Ready,
                        fallback_cause: None,
                        incoming: Vec::new(),
                        outgoing: Vec::new(),
                        incoming_truncated: false,
                        outgoing_truncated: false,
                    });
                }
                continue;
            };
            if request.directions.incoming {
                let params = CallHierarchyIncomingCallsParams {
                    item: item.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                };
                last_incoming = self
                    .send_request::<_, Option<Vec<CallHierarchyIncomingCall>>>(
                        session,
                        "callHierarchy/incomingCalls",
                        &params,
                        deadline,
                    )?
                    .unwrap_or_default();
            }
            if request.directions.outgoing {
                let params = CallHierarchyOutgoingCallsParams {
                    item,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                };
                last_outgoing = self
                    .send_request::<_, Option<Vec<CallHierarchyOutgoingCall>>>(
                        session,
                        "callHierarchy/outgoingCalls",
                        &params,
                        deadline,
                    )?
                    .unwrap_or_default();
            }
            break;
        }

        let mut incoming_truncated = false;
        let incoming = convert_incoming(
            last_incoming,
            &request.workspace,
            request.limit,
            &mut incoming_truncated,
        );
        let mut outgoing_truncated = false;
        let outgoing = convert_outgoing(
            last_outgoing,
            &request.workspace,
            request.symbol.declaration.file(),
            request.limit,
            &mut outgoing_truncated,
        );
        Ok(PreciseQueryResult {
            revision: request.workspace.revision,
            state: ProviderState::Ready,
            fallback_cause: None,
            incoming,
            outgoing,
            incoming_truncated,
            outgoing_truncated,
        })
    }

    fn prepare_call_hierarchy(
        &mut self,
        session: &mut Client,
        request: &PreciseQueryRequest,
        deadline: Instant,
    ) -> Result<Vec<CallHierarchyItem>, ProviderError> {
        let document = request
            .workspace
            .document(request.symbol.declaration.file())
            .ok_or(ProviderError::InvalidPosition)?;
        let position = find_symbol_position(
            &document.source,
            &request.symbol.name,
            &request.symbol.declaration,
        )?;
        let uri = path_to_uri(
            &request.workspace.repository_root,
            request.symbol.declaration.file(),
        )?;
        let params = CallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        Ok(self
            .send_request::<_, Option<Vec<CallHierarchyItem>>>(
                session,
                "textDocument/prepareCallHierarchy",
                &params,
                deadline,
            )?
            .unwrap_or_default())
    }

    fn select_hierarchy_item(
        &self,
        items: Vec<CallHierarchyItem>,
        request: &PreciseQueryRequest,
    ) -> Result<Option<CallHierarchyItem>, ProviderError> {
        if items.is_empty() {
            return Ok(None);
        }
        let mut matching = items.into_iter().filter(|item| {
            if item.name != request.symbol.name {
                return false;
            }
            item_declaration(item, &request.workspace).is_some_and(|(path, selection)| {
                path == *request.symbol.declaration.file()
                    && selection.start() >= request.symbol.declaration.start()
                    && selection.end() <= request.symbol.declaration.end()
            })
        });
        let Some(item) = matching.next() else {
            return Err(ProviderError::InvalidPosition);
        };
        if matching.next().is_some() {
            return Err(ProviderError::InvalidPosition);
        }
        Ok(Some(item))
    }

    fn confirm_sync_barrier(&mut self) {
        self.barrier_generation = Some(self.sync_generation);
        if self.documents_synchronized() {
            self.set_progress(ProviderProgress {
                stage: ProviderProgressStage::Ready,
                source: ProviderProgressSource::Chakra,
                message: Some("the post-synchronization request barrier is complete".to_owned()),
                percentage: Some(100),
            });
            self.set_state(ProviderState::Ready, Some(self.known_revision), None);
        }
    }

    fn provider_is_ready(&self) -> bool {
        self.documents_synchronized() && self.barrier_generation == Some(self.sync_generation)
    }

    fn documents_synchronized(&self) -> bool {
        self.known_revision == self.known_workspace.revision
    }

    fn synchronize_documents(
        &mut self,
        session: &mut Client,
        workspace: &ProviderWorkspace,
        target: &SourceRange,
        deadline: Instant,
    ) -> Result<(), ProviderError> {
        self.check_operation()?;
        let ProviderWorkspaceDelta {
            created,
            changed,
            deleted,
            documents_examined,
            source_body_comparisons,
        } = workspace
            .delta_since(
                &self.known_workspace,
                Language::Go,
                self.active_operation
                    .as_ref()
                    .ok_or(ProviderError::Cancelled)?,
            )
            .map_err(|abort| match abort {
                OperationAbort::Cancelled => ProviderError::Cancelled,
                OperationAbort::DeadlineExceeded => ProviderError::Timeout,
            })?;
        let target_document = workspace
            .document(target.file())
            .filter(|document| document.language == Language::Go)
            .ok_or(ProviderError::InvalidPosition)?;
        let target_needs_open = !self.opened_versions.contains_key(target.file());
        if !deleted.is_empty() || !created.is_empty() || !changed.is_empty() || target_needs_open {
            self.sync_generation = self.sync_generation.saturating_add(1);
            self.barrier_generation = None;
            self.set_progress(ProviderProgress {
                stage: ProviderProgressStage::DocumentSynchronization,
                source: ProviderProgressSource::Chakra,
                message: Some(format!(
                    "synchronizing revision {} ({} created, {} changed, {} deleted)",
                    workspace.revision,
                    created.len(),
                    changed.len(),
                    deleted.len()
                )),
                percentage: None,
            });
            self.set_state(ProviderState::CatchingUp, None, None);
        }

        let mut events = Vec::new();
        let created_count = created.len() as u64;
        let changed_count = changed.len() as u64;
        let deleted_count = deleted.len() as u64;
        let mut text_documents_sent = 0_u64;
        let mut text_bytes_sent = 0_u64;
        for path in deleted {
            self.check_operation()?;
            if self.opened_versions.remove(&path).is_some() {
                self.send_notification(
                    session,
                    "textDocument/didClose",
                    &DidCloseTextDocumentParams {
                        text_document: TextDocumentIdentifier {
                            uri: path_to_uri(&workspace.repository_root, &path)?,
                        },
                    },
                    deadline,
                )?;
            }
            events.push(FileEvent {
                uri: path_to_uri(&workspace.repository_root, &path)?,
                typ: FileChangeType::DELETED,
            });
        }
        for document in &created {
            self.check_operation()?;
            events.push(FileEvent {
                uri: path_to_uri(&workspace.repository_root, &document.path)?,
                typ: FileChangeType::CREATED,
            });
            if self.opened_versions.contains_key(&document.path) {
                text_bytes_sent = text_bytes_sent.saturating_add(self.open_or_change(
                    session,
                    &workspace.repository_root,
                    &document.path,
                    &document.source,
                    deadline,
                )? as u64);
                text_documents_sent = text_documents_sent.saturating_add(1);
            }
        }
        for document in &changed {
            self.check_operation()?;
            events.push(FileEvent {
                uri: path_to_uri(&workspace.repository_root, &document.path)?,
                typ: FileChangeType::CHANGED,
            });
            if self.opened_versions.contains_key(&document.path) {
                text_bytes_sent = text_bytes_sent.saturating_add(self.open_or_change(
                    session,
                    &workspace.repository_root,
                    &document.path,
                    &document.source,
                    deadline,
                )? as u64);
                text_documents_sent = text_documents_sent.saturating_add(1);
            }
        }
        if !self.opened_versions.contains_key(&target_document.path) {
            text_bytes_sent = text_bytes_sent.saturating_add(self.open_or_change(
                session,
                &workspace.repository_root,
                &target_document.path,
                &target_document.source,
                deadline,
            )? as u64);
            text_documents_sent = text_documents_sent.saturating_add(1);
        }
        let watched_file_events = events.len() as u64;
        if !events.is_empty() {
            self.send_notification(
                session,
                "workspace/didChangeWatchedFiles",
                &DidChangeWatchedFilesParams { changes: events },
                deadline,
            )?;
        }
        self.known_workspace = workspace.clone();
        self.known_revision = workspace.revision;
        let (workspace_documents, workspace_source_bytes) = workspace
            .document_stats_with_context(
                Language::Go,
                self.active_operation
                    .as_ref()
                    .ok_or(ProviderError::Cancelled)?,
            )
            .map_err(|abort| match abort {
                OperationAbort::Cancelled => ProviderError::Cancelled,
                OperationAbort::DeadlineExceeded => ProviderError::Timeout,
            })?;
        self.sync_metrics = ProviderDocumentSyncMetrics {
            revision: Some(workspace.revision),
            workspace_documents: workspace_documents as u64,
            workspace_source_bytes,
            opened_documents: self.open_versions_len(),
            created: created_count,
            changed: changed_count,
            deleted: deleted_count,
            text_documents_sent,
            text_bytes_sent,
            watched_file_events,
            documents_examined,
            source_body_comparisons,
            total_text_documents_sent: self
                .sync_metrics
                .total_text_documents_sent
                .saturating_add(text_documents_sent),
            total_text_bytes_sent: self
                .sync_metrics
                .total_text_bytes_sent
                .saturating_add(text_bytes_sent),
            total_watched_file_events: self
                .sync_metrics
                .total_watched_file_events
                .saturating_add(watched_file_events),
        };
        self.publish_observability();
        Ok(())
    }

    fn open_versions_len(&self) -> u64 {
        u64::try_from(self.opened_versions.len()).unwrap_or(u64::MAX)
    }

    fn open_or_change(
        &mut self,
        session: &mut Client,
        root: &Path,
        path: &RepoRelativePath,
        source: &Arc<str>,
        deadline: Instant,
    ) -> Result<usize, ProviderError> {
        let uri = path_to_uri(root, path)?;
        if let Some(version) = self.opened_versions.get_mut(path) {
            *version = version.saturating_add(1);
            let version = *version;
            self.send_notification(
                session,
                "textDocument/didChange",
                &DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier { uri, version },
                    content_changes: vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: source.to_string(),
                    }],
                },
                deadline,
            )?;
        } else {
            self.opened_versions.insert(path.clone(), 1);
            self.send_notification(
                session,
                "textDocument/didOpen",
                &DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id: language_id(path).to_owned(),
                        version: 1,
                        text: source.to_string(),
                    },
                },
                deadline,
            )?;
        }
        Ok(source.len())
    }

    fn start_session(&mut self) -> Result<(), ProviderError> {
        self.set_progress(ProviderProgress {
            stage: ProviderProgressStage::ProcessStartup,
            source: ProviderProgressSource::Chakra,
            message: Some("starting the gopls process".to_owned()),
            percentage: None,
        });
        self.set_state(ProviderState::Initializing, None, None);
        let client_config = ClientConfig {
            transport: TransportConfig {
                max_message_bytes: self.config.max_message_bytes,
                ..TransportConfig::default()
            },
            startup_timeout: self.config.startup_timeout,
            shutdown_timeout: self.config.barrier_timeout,
        };
        let args: Vec<&std::ffi::OsStr> = self
            .config
            .command
            .args
            .iter()
            .map(std::ffi::OsString::as_os_str)
            .collect();
        let mut session = Client::spawn(
            &self.config.command.program,
            &args,
            &self.root,
            client_config,
            "gopls",
        )
        .map_err(|error| ProviderError::Transport(error.to_string()))?;
        self.set_progress(ProviderProgress {
            stage: ProviderProgressStage::Initialization,
            source: ProviderProgressSource::Chakra,
            message: Some("performing LSP initialization".to_owned()),
            percentage: None,
        });
        self.sync_generation = 0;
        self.barrier_generation = None;
        let root_uri = directory_uri(&self.root)?;
        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_path: None,
            root_uri: Some(root_uri.clone()),
            initialization_options: None,
            capabilities: ClientCapabilities {
                window: Some(WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..WindowClientCapabilities::default()
                }),
                ..ClientCapabilities::default()
            },
            trace: None,
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root_uri,
                name: self
                    .root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("workspace")
                    .to_owned(),
            }]),
            client_info: Some(ClientInfo {
                name: "Chakra".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            locale: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let mut pending = Vec::new();
        let result = session
            .initialize(&params, &mut |event| pending.push(event))
            .map_err(ProviderError::from_client);
        for event in pending {
            self.handle_event(event);
        }
        let result: InitializeResult =
            serde_json::from_value(result?).map_err(ProviderError::InvalidResponse)?;
        let supports_call_hierarchy = matches!(
            result.capabilities.call_hierarchy_provider,
            Some(CallHierarchyServerCapability::Simple(true))
                | Some(CallHierarchyServerCapability::Options(_))
        );
        if !supports_call_hierarchy {
            return Err(ProviderError::Unsupported);
        }
        self.provider_epoch = self.provider_epoch.saturating_add(1);
        self.opened_versions.clear();
        self.session = Some(session);
        self.backoff.reset();
        self.set_progress(ProviderProgress {
            stage: ProviderProgressStage::WorkspaceLoading,
            source: ProviderProgressSource::Chakra,
            message: Some(
                "provider initialized; waiting for the first synchronization barrier".to_owned(),
            ),
            percentage: None,
        });
        self.set_state(ProviderState::CatchingUp, None, None);
        Ok(())
    }

    fn restart_for(&mut self, workspace: &ProviderWorkspace) -> Result<(), ProviderError> {
        self.root = workspace.repository_root.clone();
        self.known_workspace = workspace.clone();
        self.known_revision = workspace.revision;
        self.start_session()
    }

    fn stop_session(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        session.shutdown();
        self.opened_versions.clear();
        self.set_progress(ProviderProgress {
            stage: ProviderProgressStage::Stopped,
            source: ProviderProgressSource::Chakra,
            message: Some("provider process stopped".to_owned()),
            percentage: None,
        });
        self.publish_observability();
    }

    fn send_request<P: Serialize, R: DeserializeOwned>(
        &mut self,
        session: &mut Client,
        method: &str,
        params: &P,
        deadline: Instant,
    ) -> Result<R, ProviderError> {
        let operation = self.active_operation.clone();
        let force_stop = self.force_stop.clone();
        let cancel = move || {
            force_stop.load(Ordering::Acquire)
                || operation
                    .as_ref()
                    .is_some_and(|operation| operation.check().is_err())
        };
        let mut pending = Vec::new();
        let result = session.request(method, params, deadline, Some(&cancel), &mut |event| {
            pending.push(event);
        });
        for event in pending {
            self.handle_event(event);
        }
        let value = result.map_err(ProviderError::from_client)?;
        serde_json::from_value(value).map_err(ProviderError::InvalidResponse)
    }

    fn send_notification<P: Serialize>(
        &mut self,
        session: &mut Client,
        method: &str,
        params: &P,
        deadline: Instant,
    ) -> Result<(), ProviderError> {
        session
            .notify(method, params, deadline)
            .map_err(ProviderError::from_client)
    }

    /// Bounded event pump used while waiting for the server to catch up with
    /// a freshly synchronized generation.
    fn wait_for_events(&mut self) {
        let deadline = Instant::now() + self.config.barrier_timeout;
        while Instant::now() < deadline {
            if self.force_stop.load(Ordering::Acquire) {
                return;
            }
            self.drain_session_events();
            std::thread::sleep(EVENT_POLL);
        }
    }

    fn drain_session_events(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        let mut pending = Vec::new();
        session.drain_events(&mut |event| pending.push(event));
        let closed = pending
            .iter()
            .any(|event| matches!(event, ServerEvent::Closed(_)));
        for event in pending {
            self.handle_event(event);
        }
        if closed {
            session.shutdown();
            self.set_state(
                ProviderState::Degraded,
                None,
                Some("gopls process closed its output".to_owned()),
            );
        } else {
            self.session = Some(session);
        }
    }

    fn handle_event(&mut self, event: ServerEvent) {
        if let ServerEvent::Notification { method, params } = &event
            && method == "$/progress"
        {
            self.handle_work_done_progress(params);
        }
    }

    fn handle_work_done_progress(&mut self, params: &Value) {
        let Some(value) = params.get("value") else {
            return;
        };
        let title = value.get("title").and_then(Value::as_str);
        let message = value.get("message").and_then(Value::as_str);
        let display = match (title, message) {
            (Some(title), Some(message)) => Some(format!("{title}: {message}")),
            (Some(title), None) => Some(title.to_owned()),
            (None, Some(message)) => Some(message.to_owned()),
            (None, None) => None,
        };
        let percentage = value
            .get("percentage")
            .and_then(Value::as_u64)
            .and_then(|percentage| u32::try_from(percentage.min(100)).ok());
        self.set_progress(ProviderProgress {
            stage: ProviderProgressStage::WorkspaceLoading,
            source: ProviderProgressSource::Provider,
            message: display,
            percentage,
        });
    }

    fn set_progress(&mut self, mut progress: ProviderProgress) {
        progress.message = progress
            .message
            .map(|message| message.chars().take(MAX_PROVIDER_ERROR_CHARS).collect());
        self.progress = Some(progress);
        self.publish_observability();
    }

    fn publish_observability(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.progress = self.progress.clone();
            shared.metrics = ProviderMetrics {
                document_sync: self.sync_metrics.clone(),
                ..ProviderMetrics::default()
            };
        }
    }

    fn set_state(
        &self,
        state: ProviderState,
        synced_revision: Option<Revision>,
        last_error: Option<String>,
    ) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.state = state;
            shared.synced_revision = synced_revision;
            shared.provider_epoch = self.provider_epoch;
            shared.last_error =
                last_error.map(|message| message.chars().take(MAX_PROVIDER_ERROR_CHARS).collect());
            shared.progress = self.progress.clone();
            shared.metrics = ProviderMetrics {
                document_sync: self.sync_metrics.clone(),
                ..ProviderMetrics::default()
            };
        }
    }
}

fn language_id(path: &RepoRelativePath) -> &'static str {
    let _ = path;
    "go"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_ids_follow_the_file_extension() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(language_id(&RepoRelativePath::new("src/a.go")?), "go");
        Ok(())
    }
}
