use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_engine::{
    CallHierarchyDirections, PreciseQueryRequest, PreciseQueryResult, ProviderDocument,
    ProviderWorkspace,
};
use crossbeam_channel::Receiver;
use lsp_server::{ErrorCode, Message, Notification, Request, RequestId, Response, ResponseError};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CallHierarchyServerCapability, ClientCapabilities, ClientInfo, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    FileChangeType, FileEvent, InitializeParams, InitializeResult, InitializedParams,
    PartialResultParams, TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, VersionedTextDocumentIdentifier, WorkDoneProgressParams,
    WorkspaceFolder,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;

use crate::convert::{
    convert_incoming, convert_outgoing, directory_uri, find_symbol_position, item_declaration,
    path_to_uri,
};
use crate::protocol::{Session, TransportError, TransportEvent};
use crate::{Command, RustAnalyzerConfig, SharedState};

const MAX_PROVIDER_ERROR_CHARS: usize = 1_024;

const IDLE_POLL: Duration = Duration::from_millis(50);
const MAX_PROVIDER_RESULTS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Health {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
struct ServerStatus {
    health: Health,
    quiescent: bool,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerStatusWire {
    health: String,
    quiescent: bool,
    message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    revision: Revision,
    provider_epoch: u64,
    name: String,
    declaration: SourceRange,
    directions: CallHierarchyDirections,
    limit: usize,
}

#[derive(Debug, Error)]
pub(crate) enum ProviderError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("timed out waiting for rust-analyzer response")]
    Timeout,
    #[error("rust-analyzer request was cancelled by its caller")]
    Cancelled,
    #[error("rust-analyzer request failed ({code}): {message}")]
    Request { code: i32, message: String },
    #[error("invalid rust-analyzer response: {0}")]
    InvalidResponse(#[from] serde_json::Error),
    #[error("rust-analyzer does not advertise call hierarchy")]
    Unsupported,
    #[error("invalid file URI for {0}")]
    InvalidUri(String),
    #[error("provider position is outside captured source")]
    InvalidPosition,
    #[error("provider is not quiescent for the requested revision")]
    CatchingUp,
    #[error("provider health is {health:?}: {message}")]
    Unhealthy { health: Health, message: String },
    #[error("request id overflow")]
    RequestIdOverflow,
    #[error("document version overflow")]
    DocumentVersionOverflow,
    #[error("provider synchronization generation overflow")]
    SyncGenerationOverflow,
    #[error("rust-analyzer returned no call hierarchy item matching the selected symbol")]
    HierarchyItemMismatch,
    #[error("rust-analyzer returned multiple call hierarchy items matching the selected symbol")]
    AmbiguousHierarchyItem,
}

impl ProviderError {
    fn is_transport_failure(&self) -> bool {
        matches!(self, Self::Transport(_))
    }

    fn fallback_state(&self) -> ProviderState {
        match self {
            Self::Timeout | Self::Cancelled | Self::CatchingUp => ProviderState::CatchingUp,
            _ => ProviderState::Degraded,
        }
    }
}

pub(crate) struct Worker {
    commands: Receiver<Command>,
    shared: Arc<Mutex<SharedState>>,
    force_stop: Arc<AtomicBool>,
    config: RustAnalyzerConfig,
    root: std::path::PathBuf,
    known_revision: Revision,
    session: Option<Session>,
    server_status: Option<ServerStatus>,
    next_request_id: i32,
    provider_epoch: u64,
    known_documents: BTreeMap<RepoRelativePath, Arc<str>>,
    opened_versions: HashMap<RepoRelativePath, i32>,
    sync_generation: u64,
    barrier_generation: Option<u64>,
    quiescent_generation: Option<u64>,
    cache: HashMap<CacheKey, PreciseQueryResult>,
    shutting_down: bool,
    active_operation: Option<OperationContext>,
}

impl Worker {
    pub(crate) fn new(
        commands: Receiver<Command>,
        shared: Arc<Mutex<SharedState>>,
        force_stop: Arc<AtomicBool>,
        config: RustAnalyzerConfig,
        initial_workspace: ProviderWorkspace,
    ) -> Self {
        let known_documents = document_map(&initial_workspace.documents);
        Self {
            commands,
            shared,
            force_stop,
            config,
            root: initial_workspace.repository_root,
            known_revision: initial_workspace.revision,
            session: None,
            server_status: None,
            next_request_id: 1,
            provider_epoch: 0,
            known_documents,
            opened_versions: HashMap::new(),
            sync_generation: 0,
            barrier_generation: None,
            quiescent_generation: None,
            cache: HashMap::new(),
            shutting_down: false,
            active_operation: None,
        }
    }

    pub(crate) fn run(mut self) {
        if let Err(error) = self.start_session() {
            self.set_state(ProviderState::Degraded, None, Some(error.to_string()));
        }
        while !self.force_stop.load(Ordering::Acquire) {
            match self.commands.recv_timeout(IDLE_POLL) {
                Ok(Command::Enrich {
                    request,
                    operation,
                    response,
                }) => {
                    let result = self.handle_enrich(*request, operation);
                    let _ = response.send(result);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => self.drain_idle_messages(),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    self.stop_session();
                    break;
                }
            }
        }
        self.stop_session();
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
        self.drain_idle_messages();
        if revision < self.known_revision {
            return PreciseQueryResult::unavailable(revision, ProviderState::CatchingUp);
        }
        let key = CacheKey {
            revision,
            provider_epoch: self.provider_epoch,
            name: request.symbol.name.clone(),
            declaration: request.symbol.declaration.clone(),
            directions: request.directions,
            limit: request.limit,
        };
        if self.session.is_none()
            && let Err(error) = self.restart_for(&request.workspace)
        {
            self.set_state(ProviderState::Degraded, None, Some(error.to_string()));
            return PreciseQueryResult::unavailable(revision, ProviderState::Degraded);
        }
        if self.provider_is_quiescent().unwrap_or(false)
            && let Some(cached) = self.cache.get(&key).cloned()
        {
            self.set_state(ProviderState::Ready, Some(revision), None);
            return cached;
        }

        let first = self.query_with_owned_session(&request);
        let result = match first {
            Ok(result) => result,
            Err(error) if error.is_transport_failure() => {
                self.set_state(ProviderState::Degraded, None, Some(error.to_string()));
                self.stop_session();
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
            if self.cache.len() >= self.config.cache_capacity {
                self.cache.clear();
            }
            let mut key = key;
            key.provider_epoch = self.provider_epoch;
            self.cache.insert(key, result.clone());
            self.set_state(ProviderState::Ready, Some(revision), None);
        }
        result
    }

    fn fallback(&mut self, revision: Revision, error: ProviderError) -> PreciseQueryResult {
        let state = error.fallback_state();
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

    fn query_with_owned_session(
        &mut self,
        request: &PreciseQueryRequest,
    ) -> Result<PreciseQueryResult, ProviderError> {
        let mut session = self.session.take().ok_or_else(|| {
            ProviderError::Transport(TransportError::Closed("no active process".to_owned()))
        })?;
        let result = self.query_session(&mut session, request);
        self.session = Some(session);
        result
    }

    fn query_session(
        &mut self,
        session: &mut Session,
        request: &PreciseQueryRequest,
    ) -> Result<PreciseQueryResult, ProviderError> {
        self.check_operation()?;
        self.set_state(ProviderState::CatchingUp, None, None);
        let deadline = Instant::now() + self.config.request_timeout;
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
            self.confirm_sync_barrier();
            let Some(item) = self.select_hierarchy_item(items, request)? else {
                if self.provider_is_quiescent()? {
                    return Ok(PreciseQueryResult {
                        revision: request.workspace.revision,
                        state: ProviderState::Ready,
                        incoming: Vec::new(),
                        outgoing: Vec::new(),
                        truncated: false,
                    });
                }
                self.wait_for_quiescence(session)?;
                if attempt == 1 {
                    return Err(ProviderError::CatchingUp);
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
                        params,
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
                        params,
                        deadline,
                    )?
                    .unwrap_or_default();
            }
            if self.provider_is_quiescent()? {
                break;
            }
            self.wait_for_quiescence(session)?;
            if attempt == 1 {
                return Err(ProviderError::CatchingUp);
            }
        }

        let mut truncated = false;
        let incoming = convert_incoming(
            last_incoming,
            &request.workspace,
            request.limit,
            &mut truncated,
        );
        let outgoing = convert_outgoing(
            last_outgoing,
            &request.workspace,
            request.symbol.declaration.file(),
            request.limit,
            &mut truncated,
        );
        Ok(PreciseQueryResult {
            revision: request.workspace.revision,
            state: ProviderState::Ready,
            incoming,
            outgoing,
            truncated,
        })
    }

    fn prepare_call_hierarchy(
        &mut self,
        session: &mut Session,
        request: &PreciseQueryRequest,
        deadline: Instant,
    ) -> Result<Vec<CallHierarchyItem>, ProviderError> {
        let document = request
            .workspace
            .documents
            .iter()
            .find(|document| document.path == *request.symbol.declaration.file())
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
                params,
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
        let item = matching
            .next()
            .ok_or(ProviderError::HierarchyItemMismatch)?;
        if matching.next().is_some() {
            return Err(ProviderError::AmbiguousHierarchyItem);
        }
        Ok(Some(item))
    }

    fn confirm_sync_barrier(&mut self) {
        self.barrier_generation = Some(self.sync_generation);
        if self
            .server_status
            .as_ref()
            .is_some_and(|status| status.health == Health::Ok && status.quiescent)
        {
            self.quiescent_generation = Some(self.sync_generation);
            self.set_state(ProviderState::Ready, Some(self.known_revision), None);
        }
    }

    fn documents_synchronized(&self) -> bool {
        self.opened_versions.len() == self.known_documents.len()
            && self
                .known_documents
                .keys()
                .all(|path| self.opened_versions.contains_key(path))
    }

    fn synchronize_documents(
        &mut self,
        session: &mut Session,
        workspace: &ProviderWorkspace,
        target: &SourceRange,
        deadline: Instant,
    ) -> Result<(), ProviderError> {
        self.check_operation()?;
        let current = document_map(&workspace.documents);
        if !current.contains_key(target.file()) {
            return Err(ProviderError::InvalidPosition);
        }
        let deleted: Vec<_> = self
            .known_documents
            .keys()
            .filter(|path| !current.contains_key(*path))
            .cloned()
            .collect();
        let upserts: Vec<_> = current
            .iter()
            .filter_map(|(path, source)| {
                let content_changed = self.known_documents.get(path).is_none_or(|known| {
                    !Arc::ptr_eq(known, source) && known.as_ref() != source.as_ref()
                });
                let needs_open = !self.opened_versions.contains_key(path);
                (content_changed || needs_open)
                    .then(|| (path.clone(), source.clone(), content_changed))
            })
            .collect();
        if !deleted.is_empty() || !upserts.is_empty() {
            self.sync_generation = self
                .sync_generation
                .checked_add(1)
                .ok_or(ProviderError::SyncGenerationOverflow)?;
            self.barrier_generation = None;
            self.quiescent_generation = None;
            self.set_state(ProviderState::CatchingUp, None, None);
        }

        let mut events = Vec::new();
        for path in deleted {
            self.check_operation()?;
            if self.opened_versions.remove(&path).is_some() {
                self.send_notification(
                    session,
                    "textDocument/didClose",
                    DidCloseTextDocumentParams {
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
        for (path, source, content_changed) in upserts {
            self.check_operation()?;
            if content_changed {
                let change_type = if self.known_documents.contains_key(&path) {
                    FileChangeType::CHANGED
                } else {
                    FileChangeType::CREATED
                };
                events.push(FileEvent {
                    uri: path_to_uri(&workspace.repository_root, &path)?,
                    typ: change_type,
                });
            }
            self.open_or_change(
                session,
                &workspace.repository_root,
                &path,
                &source,
                deadline,
            )?;
        }
        if !events.is_empty() {
            self.send_notification(
                session,
                "workspace/didChangeWatchedFiles",
                DidChangeWatchedFilesParams { changes: events },
                deadline,
            )?;
        }
        self.known_documents = current;
        self.known_revision = workspace.revision;
        self.cache
            .retain(|key, _| key.revision == workspace.revision);
        Ok(())
    }

    fn open_or_change(
        &mut self,
        session: &mut Session,
        root: &Path,
        path: &RepoRelativePath,
        source: &Arc<str>,
        deadline: Instant,
    ) -> Result<(), ProviderError> {
        let uri = path_to_uri(root, path)?;
        if let Some(version) = self.opened_versions.get_mut(path) {
            *version = version
                .checked_add(1)
                .ok_or(ProviderError::DocumentVersionOverflow)?;
            let version = *version;
            self.send_notification(
                session,
                "textDocument/didChange",
                DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier { uri, version },
                    content_changes: vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: source.to_string(),
                    }],
                },
                deadline,
            )
        } else {
            self.opened_versions.insert(path.clone(), 1);
            self.send_notification(
                session,
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id: "rust".to_owned(),
                        version: 1,
                        text: source.to_string(),
                    },
                },
                deadline,
            )
        }
    }

    fn start_session(&mut self) -> Result<(), ProviderError> {
        self.set_state(ProviderState::Initializing, None, None);
        let mut session = Session::spawn(&self.config.executable, &self.root)?;
        self.server_status = None;
        self.sync_generation = 0;
        self.barrier_generation = None;
        self.quiescent_generation = None;
        self.next_request_id = 1;
        let startup_deadline = Instant::now() + self.config.startup_timeout;
        let root_uri = directory_uri(&self.root)?;
        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_path: None,
            root_uri: Some(root_uri.clone()),
            initialization_options: None,
            capabilities: ClientCapabilities {
                experimental: Some(json!({ "serverStatusNotification": true })),
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
        let result: InitializeResult =
            self.send_request(&mut session, "initialize", params, startup_deadline)?;
        let supports_call_hierarchy = matches!(
            result.capabilities.call_hierarchy_provider,
            Some(CallHierarchyServerCapability::Simple(true))
                | Some(CallHierarchyServerCapability::Options(_))
        );
        if !supports_call_hierarchy {
            return Err(ProviderError::Unsupported);
        }
        self.send_notification(
            &mut session,
            "initialized",
            InitializedParams {},
            startup_deadline,
        )?;
        self.provider_epoch = self.provider_epoch.saturating_add(1);
        self.cache.clear();
        self.opened_versions.clear();
        self.session = Some(session);
        if self.provider_is_quiescent().unwrap_or(false) {
            self.set_state(ProviderState::Ready, Some(self.known_revision), None);
        } else {
            self.set_state(ProviderState::CatchingUp, None, None);
        }
        Ok(())
    }

    fn restart_for(&mut self, workspace: &ProviderWorkspace) -> Result<(), ProviderError> {
        self.root = workspace.repository_root.clone();
        self.known_documents = document_map(&workspace.documents);
        self.known_revision = workspace.revision;
        self.start_session()
    }

    fn stop_session(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        let was_shutting_down = self.shutting_down;
        self.shutting_down = true;
        let deadline = Instant::now() + self.config.barrier_timeout;
        let shutdown =
            self.send_request::<_, Value>(&mut session, "shutdown", Value::Null, deadline);
        if shutdown.is_ok() {
            let _ = self.send_notification(&mut session, "exit", Value::Null, deadline);
        }
        session.terminate();
        self.shutting_down = was_shutting_down;
        self.opened_versions.clear();
        self.cache.clear();
    }

    fn send_request<P: serde::Serialize, R: DeserializeOwned>(
        &mut self,
        session: &mut Session,
        method: &str,
        params: P,
        deadline: Instant,
    ) -> Result<R, ProviderError> {
        let id = RequestId::from(self.next_request_id);
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(ProviderError::RequestIdOverflow)?;
        let request = Request {
            id: id.clone(),
            method: method.to_owned(),
            params: serde_json::to_value(params)?,
        };
        session.send(&Message::Request(request), deadline)?;
        let value = match self.wait_for_response(session, &id, deadline) {
            Ok(value) => value,
            Err(error @ (ProviderError::Timeout | ProviderError::Cancelled)) => {
                let _ = self.send_notification(
                    session,
                    "$/cancelRequest",
                    json!({ "id": id }),
                    Instant::now() + self.config.barrier_timeout,
                );
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        serde_json::from_value(value).map_err(ProviderError::InvalidResponse)
    }

    fn send_notification<P: serde::Serialize>(
        &mut self,
        session: &mut Session,
        method: &str,
        params: P,
        deadline: Instant,
    ) -> Result<(), ProviderError> {
        session
            .send(
                &Message::Notification(Notification {
                    method: method.to_owned(),
                    params: serde_json::to_value(params)?,
                }),
                deadline,
            )
            .map_err(ProviderError::Transport)
    }

    fn wait_for_response(
        &mut self,
        session: &mut Session,
        id: &RequestId,
        deadline: Instant,
    ) -> Result<Value, ProviderError> {
        loop {
            self.check_operation()?;
            if self.force_stop.load(Ordering::Acquire) && !self.shutting_down {
                let _ = self.send_notification(
                    session,
                    "$/cancelRequest",
                    json!({ "id": id }),
                    Instant::now() + self.config.barrier_timeout,
                );
                return Err(ProviderError::CatchingUp);
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(ProviderError::Timeout)?;
            let poll = remaining.min(IDLE_POLL);
            let event = match session.incoming().recv_timeout(poll) {
                Ok(event) => event,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) if poll < remaining => continue,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    return Err(ProviderError::Timeout);
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    return Err(ProviderError::Transport(TransportError::Closed(
                        "reader disconnected".to_owned(),
                    )));
                }
            };
            match event {
                TransportEvent::Message(Message::Response(response)) if &response.id == id => {
                    return response
                        .response_result
                        .map_err(|error| ProviderError::Request {
                            code: error.code,
                            message: error.message,
                        });
                }
                TransportEvent::Message(Message::Request(request)) => {
                    self.respond_to_server(session, request, deadline)?;
                }
                TransportEvent::Message(Message::Notification(notification)) => {
                    self.handle_notification(notification);
                }
                TransportEvent::Message(Message::Response(_)) => {}
                TransportEvent::Closed(message) => {
                    return Err(ProviderError::Transport(TransportError::Closed(message)));
                }
            }
        }
    }

    fn respond_to_server(
        &mut self,
        session: &mut Session,
        request: Request,
        deadline: Instant,
    ) -> Result<(), ProviderError> {
        let result = match request.method.as_str() {
            "workspace/configuration" => {
                let count = request
                    .params
                    .get("items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Ok(Value::Array(vec![Value::Null; count]))
            }
            "window/workDoneProgress/create"
            | "client/registerCapability"
            | "client/unregisterCapability" => Ok(Value::Null),
            _ => Err(ResponseError {
                code: ErrorCode::MethodNotFound as i32,
                message: format!("unsupported client request: {}", request.method),
                data: None,
            }),
        };
        session
            .send(
                &Message::Response(Response {
                    id: request.id,
                    response_result: result,
                }),
                deadline,
            )
            .map_err(ProviderError::Transport)
    }

    pub(crate) fn handle_notification(&mut self, notification: Notification) {
        if notification.method != "experimental/serverStatus" {
            return;
        }
        match serde_json::from_value::<ServerStatusWire>(notification.params) {
            Ok(status) => {
                let health = match status.health.as_str() {
                    "ok" => Health::Ok,
                    "warning" => Health::Warning,
                    _ => Health::Error,
                };
                self.server_status = Some(ServerStatus {
                    health,
                    quiescent: status.quiescent,
                    message: status.message,
                });
            }
            Err(error) => {
                self.server_status = Some(ServerStatus {
                    health: Health::Error,
                    quiescent: false,
                    message: Some(error.to_string()),
                });
            }
        }
        match &self.server_status {
            Some(ServerStatus {
                health: Health::Ok,
                quiescent: true,
                ..
            }) if self.documents_synchronized()
                && (self.sync_generation == 0
                    || self.barrier_generation == Some(self.sync_generation)) =>
            {
                self.quiescent_generation = Some(self.sync_generation);
                self.set_state(ProviderState::Ready, Some(self.known_revision), None);
            }
            Some(ServerStatus {
                health: Health::Ok,
                quiescent: true,
                ..
            }) => self.set_state(ProviderState::CatchingUp, None, None),
            Some(ServerStatus {
                health: Health::Ok,
                quiescent: false,
                ..
            }) => {
                self.quiescent_generation = None;
                self.set_state(ProviderState::CatchingUp, None, None);
            }
            Some(status) => self.set_state(ProviderState::Degraded, None, status.message.clone()),
            None => {}
        }
    }

    fn provider_is_quiescent(&self) -> Result<bool, ProviderError> {
        let Some(status) = &self.server_status else {
            return Ok(false);
        };
        if status.health != Health::Ok {
            return Err(ProviderError::Unhealthy {
                health: status.health,
                message: status
                    .message
                    .clone()
                    .unwrap_or_else(|| "no status message".to_owned()),
            });
        }
        Ok(self.documents_synchronized()
            && status.quiescent
            && (self.sync_generation == 0
                || self.quiescent_generation == Some(self.sync_generation)))
    }

    fn wait_for_quiescence(&mut self, session: &mut Session) -> Result<(), ProviderError> {
        let deadline = Instant::now() + self.config.barrier_timeout;
        loop {
            self.check_operation()?;
            if self.force_stop.load(Ordering::Acquire) {
                return Err(ProviderError::CatchingUp);
            }
            if self.provider_is_quiescent()? {
                return Ok(());
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(ProviderError::CatchingUp)?;
            let poll = remaining.min(IDLE_POLL);
            let event = match session.incoming().recv_timeout(poll) {
                Ok(event) => event,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) if poll < remaining => continue,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    return Err(ProviderError::CatchingUp);
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    return Err(ProviderError::Transport(TransportError::Closed(
                        "reader disconnected".to_owned(),
                    )));
                }
            };
            match event {
                TransportEvent::Message(Message::Notification(notification)) => {
                    self.handle_notification(notification);
                }
                TransportEvent::Message(Message::Request(request)) => {
                    self.respond_to_server(session, request, deadline)?;
                }
                TransportEvent::Message(Message::Response(_)) => {}
                TransportEvent::Closed(message) => {
                    return Err(ProviderError::Transport(TransportError::Closed(message)));
                }
            }
        }
    }

    fn drain_idle_messages(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        while let Ok(event) = session.incoming().try_recv() {
            match event {
                TransportEvent::Message(Message::Notification(notification)) => {
                    self.handle_notification(notification);
                }
                TransportEvent::Message(Message::Request(request)) => {
                    if let Err(error) = self.respond_to_server(
                        &mut session,
                        request,
                        Instant::now() + self.config.barrier_timeout,
                    ) {
                        self.set_state(ProviderState::Degraded, None, Some(error.to_string()));
                    }
                }
                TransportEvent::Message(Message::Response(_)) => {}
                TransportEvent::Closed(message) => {
                    self.set_state(ProviderState::Degraded, None, Some(message));
                    session.terminate();
                    return;
                }
            }
        }
        self.session = Some(session);
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
        }
    }
}

fn document_map(documents: &[ProviderDocument]) -> BTreeMap<RepoRelativePath, Arc<str>> {
    documents
        .iter()
        .filter(|document| document.language == chakra_domain::symbol::Language::Rust)
        .map(|document| (document.path.clone(), document.source.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use crossbeam_channel::bounded;
    use serde_json::json;

    use super::*;
    use crate::SharedState;

    #[test]
    fn quiescent_status_needs_a_post_sync_request_barrier() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempfile::tempdir()?;
        let (_sender, commands) = bounded(1);
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let mut worker = Worker::new(
            commands,
            shared.clone(),
            Arc::new(AtomicBool::new(false)),
            RustAnalyzerConfig::default(),
            ProviderWorkspace {
                repository_root: root.path().to_path_buf(),
                revision: Revision(7),
                documents: Vec::new(),
            },
        );
        worker.sync_generation = 1;
        worker.known_revision = Revision(8);
        worker.handle_notification(Notification {
            method: "experimental/serverStatus".to_owned(),
            params: json!({ "health": "ok", "quiescent": true, "message": null }),
        });
        {
            let state = shared.lock().map_err(|_| "shared state lock poisoned")?;
            assert_eq!(state.state, ProviderState::CatchingUp);
            assert_eq!(state.synced_revision, None);
        }

        worker.confirm_sync_barrier();
        let state = shared.lock().map_err(|_| "shared state lock poisoned")?;
        assert_eq!(state.state, ProviderState::Ready);
        assert_eq!(state.synced_revision, Some(Revision(8)));
        Ok(())
    }
}
