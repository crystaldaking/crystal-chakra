use std::collections::{HashMap, VecDeque};
use std::mem::size_of;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::query::{
    ProviderCacheMetrics, ProviderDocumentSyncMetrics, ProviderMetrics, ProviderProgress,
    ProviderProgressSource, ProviderProgressStage,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_engine::{
    CallHierarchyDirections, PreciseQueryRequest, PreciseQueryResult, ProviderWorkspace,
    ProviderWorkspaceDelta,
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
    TextDocumentPositionParams, VersionedTextDocumentIdentifier, WindowClientCapabilities,
    WorkDoneProgressParams, WorkspaceFolder,
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

#[derive(Debug)]
struct CacheEntry {
    result: PreciseQueryResult,
    bytes: usize,
}

#[derive(Debug)]
struct ProviderCache {
    entries: HashMap<CacheKey, CacheEntry>,
    order: VecDeque<CacheKey>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl ProviderCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    fn get(&mut self, key: &CacheKey) -> Option<PreciseQueryResult> {
        let result = self.entries.get(key).map(|entry| entry.result.clone());
        if result.is_some() {
            self.hits = self.hits.saturating_add(1);
            self.order.retain(|candidate| candidate != key);
            self.order.push_back(key.clone());
        } else {
            self.misses = self.misses.saturating_add(1);
        }
        result
    }

    fn insert(&mut self, key: CacheKey, result: PreciseQueryResult) {
        let bytes = cache_entry_bytes(&key, &result);
        if bytes > self.max_bytes {
            return;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(previous.bytes);
            self.order.retain(|candidate| candidate != &key);
        }
        while self.entries.len() >= self.max_entries
            || self.bytes.saturating_add(bytes) > self.max_bytes
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(evicted.bytes);
                self.evictions = self.evictions.saturating_add(1);
            }
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.order.push_back(key.clone());
        self.entries.insert(key, CacheEntry { result, bytes });
    }

    fn retain_revision(&mut self, revision: Revision) {
        let removed: Vec<_> = self
            .entries
            .keys()
            .filter(|key| key.revision != revision)
            .cloned()
            .collect();
        for key in removed {
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
        self.order.retain(|key| self.entries.contains_key(key));
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }

    fn metrics(&self) -> ProviderCacheMetrics {
        ProviderCacheMetrics {
            entries: self.entries.len() as u64,
            bytes: self.bytes as u64,
            max_entries: self.max_entries as u64,
            max_bytes: self.max_bytes as u64,
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
        }
    }
}

fn cache_entry_bytes(key: &CacheKey, result: &PreciseQueryResult) -> usize {
    let range_bytes = |range: &SourceRange| range.file().as_str().len() + size_of::<SourceRange>();
    let relation_bytes = |relation: &chakra_engine::PreciseRelation| {
        size_of::<chakra_engine::PreciseRelation>()
            + relation.name.len()
            + range_bytes(&relation.declaration)
            + relation.call_sites.iter().map(range_bytes).sum::<usize>()
    };
    size_of::<CacheKey>()
        .saturating_add(key.name.len())
        .saturating_add(range_bytes(&key.declaration))
        .saturating_add(size_of::<PreciseQueryResult>())
        .saturating_add(result.incoming.iter().map(relation_bytes).sum::<usize>())
        .saturating_add(result.outgoing.iter().map(relation_bytes).sum::<usize>())
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
    known_workspace: ProviderWorkspace,
    opened_versions: HashMap<RepoRelativePath, i32>,
    sync_generation: u64,
    barrier_generation: Option<u64>,
    quiescent_generation: Option<u64>,
    cache: ProviderCache,
    sync_metrics: ProviderDocumentSyncMetrics,
    progress: Option<ProviderProgress>,
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
        let known_revision = initial_workspace.revision;
        let (workspace_documents, workspace_source_bytes) =
            initial_workspace.document_stats(chakra_domain::symbol::Language::Rust);
        let cache = ProviderCache::new(config.cache_capacity, config.cache_bytes);
        let root = initial_workspace.repository_root.clone();
        Self {
            commands,
            shared,
            force_stop,
            config,
            root,
            known_revision,
            session: None,
            server_status: None,
            next_request_id: 1,
            provider_epoch: 0,
            known_workspace: initial_workspace,
            opened_versions: HashMap::new(),
            sync_generation: 0,
            barrier_generation: None,
            quiescent_generation: None,
            cache,
            sync_metrics: ProviderDocumentSyncMetrics {
                revision: Some(known_revision),
                workspace_documents: workspace_documents as u64,
                workspace_source_bytes,
                ..ProviderDocumentSyncMetrics::default()
            },
            progress: None,
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
        if self.provider_is_quiescent().unwrap_or(false) {
            if let Some(cached) = self.cache.get(&key) {
                self.set_state(ProviderState::Ready, Some(revision), None);
                return cached;
            }
            self.publish_observability();
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
            let mut key = key;
            key.provider_epoch = self.provider_epoch;
            self.cache.insert(key, result.clone());
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
            self.confirm_sync_barrier();
            let Some(item) = self.select_hierarchy_item(items, request)? else {
                if self.provider_is_quiescent()? {
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
        session: &mut Session,
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
            self.set_progress(ProviderProgress {
                stage: ProviderProgressStage::Ready,
                source: ProviderProgressSource::Chakra,
                message: Some(
                    "provider quiescence and the post-synchronization request barrier are complete"
                        .to_owned(),
                ),
                percentage: Some(100),
            });
            self.set_state(ProviderState::Ready, Some(self.known_revision), None);
        }
    }

    fn documents_synchronized(&self) -> bool {
        self.known_revision == self.known_workspace.revision
    }

    fn synchronize_documents(
        &mut self,
        session: &mut Session,
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
            inputs_created,
            inputs_changed,
            inputs_deleted,
            inputs_examined: _,
        } = workspace
            .delta_since(
                &self.known_workspace,
                chakra_domain::symbol::Language::Rust,
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
            .filter(|document| document.language == chakra_domain::symbol::Language::Rust)
            .ok_or(ProviderError::InvalidPosition)?;
        let target_needs_open = !self.opened_versions.contains_key(target.file());
        if !deleted.is_empty()
            || !created.is_empty()
            || !changed.is_empty()
            || !inputs_created.is_empty()
            || !inputs_changed.is_empty()
            || !inputs_deleted.is_empty()
            || target_needs_open
        {
            self.sync_generation = self
                .sync_generation
                .checked_add(1)
                .ok_or(ProviderError::SyncGenerationOverflow)?;
            self.barrier_generation = None;
            self.quiescent_generation = None;
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
        for path in inputs_deleted {
            self.check_operation()?;
            events.push(FileEvent {
                uri: path_to_uri(&workspace.repository_root, &path)?,
                typ: FileChangeType::DELETED,
            });
        }
        for path in inputs_created {
            self.check_operation()?;
            events.push(FileEvent {
                uri: path_to_uri(&workspace.repository_root, &path)?,
                typ: FileChangeType::CREATED,
            });
        }
        for path in inputs_changed {
            self.check_operation()?;
            events.push(FileEvent {
                uri: path_to_uri(&workspace.repository_root, &path)?,
                typ: FileChangeType::CHANGED,
            });
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
                DidChangeWatchedFilesParams { changes: events },
                deadline,
            )?;
        }
        self.known_workspace = workspace.clone();
        self.known_revision = workspace.revision;
        self.cache.retain_revision(workspace.revision);
        let (workspace_documents, workspace_source_bytes) = workspace
            .document_stats_with_context(
                chakra_domain::symbol::Language::Rust,
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
            opened_documents: self.opened_versions.len() as u64,
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

    fn open_or_change(
        &mut self,
        session: &mut Session,
        root: &Path,
        path: &RepoRelativePath,
        source: &Arc<str>,
        deadline: Instant,
    ) -> Result<usize, ProviderError> {
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
            )?;
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
            )?;
        }
        Ok(source.len())
    }

    fn start_session(&mut self) -> Result<(), ProviderError> {
        self.set_progress(ProviderProgress {
            stage: ProviderProgressStage::ProcessStartup,
            source: ProviderProgressSource::Chakra,
            message: Some("starting rust-analyzer process".to_owned()),
            percentage: None,
        });
        self.set_state(ProviderState::Initializing, None, None);
        let mut session = Session::spawn(&self.config.executable, &self.root)?;
        self.set_progress(ProviderProgress {
            stage: ProviderProgressStage::Initialization,
            source: ProviderProgressSource::Chakra,
            message: Some("performing LSP initialization".to_owned()),
            percentage: None,
        });
        self.server_status = None;
        self.sync_generation = 0;
        self.barrier_generation = None;
        self.quiescent_generation = None;
        self.next_request_id = 1;
        let startup_deadline = self.operation_deadline(self.config.startup_timeout);
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
            self.set_progress(ProviderProgress {
                stage: ProviderProgressStage::Ready,
                source: ProviderProgressSource::Chakra,
                message: Some("provider is quiescent for the current syntax revision".to_owned()),
                percentage: Some(100),
            });
            self.set_state(ProviderState::Ready, Some(self.known_revision), None);
        } else {
            self.set_progress(ProviderProgress {
                stage: ProviderProgressStage::WorkspaceLoading,
                source: ProviderProgressSource::Chakra,
                message: Some("waiting for provider workspace loading signals".to_owned()),
                percentage: None,
            });
            self.set_state(ProviderState::CatchingUp, None, None);
        }
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
        let was_shutting_down = self.shutting_down;
        self.shutting_down = true;
        let deadline = self.operation_deadline(self.config.barrier_timeout);
        let shutdown =
            self.send_request::<_, Value>(&mut session, "shutdown", Value::Null, deadline);
        if shutdown.is_ok() {
            let _ = self.send_notification(&mut session, "exit", Value::Null, deadline);
        }
        session.terminate();
        self.shutting_down = was_shutting_down;
        self.opened_versions.clear();
        self.cache.clear();
        self.set_progress(ProviderProgress {
            stage: ProviderProgressStage::Stopped,
            source: ProviderProgressSource::Chakra,
            message: Some("provider process stopped".to_owned()),
            percentage: None,
        });
        self.publish_observability();
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
        if notification.method == "$/progress" {
            self.handle_work_done_progress(notification.params);
            return;
        }
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
                self.set_progress(ProviderProgress {
                    stage: ProviderProgressStage::Ready,
                    source: ProviderProgressSource::Chakra,
                    message: Some(
                        "provider reported quiescence and Chakra confirmed the synchronization barrier"
                            .to_owned(),
                    ),
                    percentage: Some(100),
                });
                self.set_state(ProviderState::Ready, Some(self.known_revision), None);
            }
            Some(ServerStatus {
                health: Health::Ok,
                quiescent: true,
                ..
            }) => {
                self.set_progress(ProviderProgress {
                    stage: ProviderProgressStage::DocumentSynchronization,
                    source: ProviderProgressSource::Chakra,
                    message: Some(
                        "provider is quiescent but the current revision barrier is incomplete"
                            .to_owned(),
                    ),
                    percentage: None,
                });
                self.set_state(ProviderState::CatchingUp, None, None);
            }
            Some(ServerStatus {
                health: Health::Ok,
                quiescent: false,
                ..
            }) => {
                self.quiescent_generation = None;
                if self
                    .progress
                    .as_ref()
                    .is_none_or(|progress| progress.source != ProviderProgressSource::Provider)
                {
                    self.set_progress(ProviderProgress {
                        stage: ProviderProgressStage::WorkspaceLoading,
                        source: ProviderProgressSource::Chakra,
                        message: Some("provider reports pending background work".to_owned()),
                        percentage: None,
                    });
                }
                self.set_state(ProviderState::CatchingUp, None, None);
            }
            Some(status) => {
                let message = status.message.clone();
                self.set_progress(ProviderProgress {
                    stage: ProviderProgressStage::Degraded,
                    source: ProviderProgressSource::Provider,
                    message: message.clone(),
                    percentage: None,
                });
                self.set_state(ProviderState::Degraded, None, message);
            }
            None => {}
        }
    }

    fn handle_work_done_progress(&mut self, params: Value) {
        let token = params
            .get("token")
            .and_then(|token| {
                token
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| Some(token.to_string()))
            })
            .unwrap_or_default();
        let Some(value) = params.get("value") else {
            return;
        };
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "end" {
            self.set_progress(ProviderProgress {
                stage: ProviderProgressStage::WorkspaceLoading,
                source: ProviderProgressSource::Chakra,
                message: Some(
                    "provider background stage completed; waiting for quiescence".to_owned(),
                ),
                percentage: None,
            });
            self.set_state(ProviderState::CatchingUp, None, None);
            return;
        }
        let title = value.get("title").and_then(Value::as_str);
        let message = value.get("message").and_then(Value::as_str);
        let stage = classify_progress_stage(&token, title, message);
        let display = match (title, message) {
            (Some(title), Some(message)) => Some(format!("{title}: {message}")),
            (Some(title), None) => Some(title.to_owned()),
            (None, Some(message)) => Some(message.to_owned()),
            (None, None) => (!token.is_empty()).then_some(token),
        };
        let percentage = value
            .get("percentage")
            .and_then(Value::as_u64)
            .and_then(|percentage| u32::try_from(percentage.min(100)).ok());
        self.set_progress(ProviderProgress {
            stage,
            source: ProviderProgressSource::Provider,
            message: display,
            percentage,
        });
        self.set_state(ProviderState::CatchingUp, None, None);
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
        let deadline = self.operation_deadline(self.config.barrier_timeout);
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
                cache: self.cache.metrics(),
                document_sync: self.sync_metrics.clone(),
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
                cache: self.cache.metrics(),
                document_sync: self.sync_metrics.clone(),
            };
        }
    }
}

fn classify_progress_stage(
    token: &str,
    title: Option<&str>,
    message: Option<&str>,
) -> ProviderProgressStage {
    let mut text = token.to_ascii_lowercase();
    if let Some(title) = title {
        text.push(' ');
        text.push_str(&title.to_ascii_lowercase());
    }
    if let Some(message) = message {
        text.push(' ');
        text.push_str(&message.to_ascii_lowercase());
    }
    if text.contains("metadata") || text.contains("fetch") || text.contains("cargo") {
        ProviderProgressStage::CargoMetadata
    } else if text.contains("index") || text.contains("roots scanned") {
        ProviderProgressStage::Indexing
    } else {
        ProviderProgressStage::WorkspaceLoading
    }
}

#[cfg(test)]
mod tests {
    use chakra_domain::location::TextPosition;
    use chakra_domain::provenance::Provenance;
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
            ProviderWorkspace::from_documents(root.path().to_path_buf(), Revision(7), Vec::new()),
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

    #[test]
    fn work_done_progress_is_exposed_as_a_direct_provider_fact()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let (_sender, commands) = bounded(1);
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let mut worker = Worker::new(
            commands,
            shared.clone(),
            Arc::new(AtomicBool::new(false)),
            RustAnalyzerConfig::default(),
            ProviderWorkspace::from_documents(root.path().to_path_buf(), Revision(3), Vec::new()),
        );
        worker.handle_notification(Notification {
            method: "$/progress".to_owned(),
            params: json!({
                "token": "rustAnalyzer/Loading",
                "value": {
                    "kind": "report",
                    "title": "Loading",
                    "message": "running cargo metadata",
                    "percentage": 25
                }
            }),
        });
        let state = shared.lock().map_err(|_| "shared state lock poisoned")?;
        let progress = state.progress.as_ref().ok_or("progress missing")?;
        assert_eq!(progress.stage, ProviderProgressStage::CargoMetadata);
        assert_eq!(progress.source, ProviderProgressSource::Provider);
        assert_eq!(progress.percentage, Some(25));
        assert_eq!(state.state, ProviderState::CatchingUp);
        Ok(())
    }

    #[test]
    fn precise_cache_evicts_to_its_byte_budget() -> Result<(), Box<dyn std::error::Error>> {
        let path = RepoRelativePath::new("src/lib.rs")?;
        let range = SourceRange::new(path, TextPosition::new(1, 1)?, TextPosition::new(1, 10)?)?;
        let key = |name: &str| CacheKey {
            revision: Revision(1),
            provider_epoch: 1,
            name: name.to_owned(),
            declaration: range.clone(),
            directions: CallHierarchyDirections {
                incoming: true,
                outgoing: false,
            },
            limit: 20,
        };
        let result = |name: &str| PreciseQueryResult {
            revision: Revision(1),
            state: ProviderState::Ready,
            fallback_cause: None,
            incoming: vec![chakra_engine::PreciseRelation {
                name: name.to_owned(),
                declaration: range.clone(),
                occurrence_count: 1,
                call_sites: Vec::new(),
                provenance: Provenance::RustAnalyzer,
            }],
            outgoing: Vec::new(),
            incoming_truncated: false,
            outgoing_truncated: false,
        };
        let first_key = key("first");
        let first_result = result("first");
        let one_entry_budget = cache_entry_bytes(&first_key, &first_result);
        let mut cache = ProviderCache::new(8, one_entry_budget);
        cache.insert(first_key.clone(), first_result);
        let second_key = key("other");
        cache.insert(second_key.clone(), result("other"));

        let metrics = cache.metrics();
        assert_eq!(metrics.entries, 1);
        assert!(metrics.bytes <= metrics.max_bytes);
        assert_eq!(metrics.evictions, 1);
        assert!(cache.get(&second_key).is_some());
        assert!(cache.get(&first_key).is_none());
        let metrics = cache.metrics();
        assert_eq!(metrics.hits, 1);
        assert_eq!(metrics.misses, 1);
        Ok(())
    }
}
