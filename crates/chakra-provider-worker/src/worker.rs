//! Language-neutral owner-thread worker core: session lifecycle,
//! revision-scoped document synchronization, the post-synchronization request
//! barrier, bounded restart/backoff, observability, cancellation, and
//! cooperative shutdown. Provider-specific behavior enters only through
//! [`ProviderHooks`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::query::{
    ProviderDocumentSyncMetrics, ProviderMetrics, ProviderProgress, ProviderProgressSource,
    ProviderProgressStage,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_engine::{PreciseQueryRequest, PreciseQueryResult, ProviderWorkspaceDelta};
use chakra_lsp::{Client, ClientConfig, RestartBackoff, ServerEvent, TransportConfig};
use crossbeam_channel::Receiver;
use lsp_types::{
    ClientCapabilities, ClientInfo, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, FileChangeType, FileEvent,
    InitializeParams, InitializeResult, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, VersionedTextDocumentIdentifier, WindowClientCapabilities,
    WorkDoneProgressParams, WorkspaceFolder,
};
use serde::Serialize;
use serde_json::Value;

use crate::convert::{directory_uri, path_to_uri};
use crate::hooks::QueryChannel;
use crate::provider::WorkerConfig;
use crate::state::{ProviderCommand, SharedState};
use crate::{ProviderHooks, WorkerError};

pub(crate) const MAX_PROVIDER_ERROR_CHARS: usize = 1_024;
const MAX_PROVIDER_RESULTS: usize = 500;
const EVENT_POLL: Duration = Duration::from_millis(25);

pub(crate) struct WorkerCore<H: ProviderHooks> {
    commands: Receiver<ProviderCommand>,
    shared: Arc<Mutex<SharedState>>,
    force_stop: Arc<AtomicBool>,
    config: WorkerConfig,
    hooks: Arc<H>,
    root: PathBuf,
    known_revision: Revision,
    session: Option<Client>,
    provider_epoch: u64,
    known_workspace: chakra_engine::ProviderWorkspace,
    opened_versions: HashMap<RepoRelativePath, i32>,
    sync_generation: u64,
    barrier_generation: Option<u64>,
    sync_metrics: ProviderDocumentSyncMetrics,
    progress: Option<ProviderProgress>,
    active_operation: Option<OperationContext>,
    backoff: RestartBackoff,
}

impl<H: ProviderHooks> WorkerCore<H> {
    pub(crate) fn new(
        commands: Receiver<ProviderCommand>,
        shared: Arc<Mutex<SharedState>>,
        force_stop: Arc<AtomicBool>,
        config: WorkerConfig,
        hooks: Arc<H>,
        initial_workspace: chakra_engine::ProviderWorkspace,
    ) -> Self {
        let known_revision = initial_workspace.revision;
        let (workspace_documents, workspace_source_bytes) =
            initial_workspace.document_stats_matching(|language| hooks.synchronizes(language));
        let root = initial_workspace.repository_root.clone();
        Self {
            commands,
            shared,
            force_stop,
            backoff: RestartBackoff::new(config.restart_base_delay, config.restart_max_delay),
            config,
            hooks,
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
                Ok(ProviderCommand::Enrich {
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

    fn fallback(&mut self, revision: Revision, error: WorkerError) -> PreciseQueryResult {
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

    fn check_operation(&self) -> Result<(), WorkerError> {
        match self
            .active_operation
            .as_ref()
            .map_or(Ok(()), OperationContext::check)
        {
            Ok(()) => Ok(()),
            Err(OperationAbort::Cancelled) => Err(WorkerError::Cancelled),
            Err(OperationAbort::DeadlineExceeded) => Err(WorkerError::Timeout),
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
    ) -> Result<PreciseQueryResult, WorkerError> {
        let mut session = self
            .session
            .take()
            .ok_or_else(|| WorkerError::transport(self.hooks.name(), "no active process"))?;
        let result = self.query_session(&mut session, request);
        self.session = Some(session);
        result
    }

    fn query_session(
        &mut self,
        session: &mut Client,
        request: &PreciseQueryRequest,
    ) -> Result<PreciseQueryResult, WorkerError> {
        self.check_operation()?;
        self.set_state(ProviderState::CatchingUp, None, None);
        let request_deadline = self.operation_deadline(self.config.request_timeout);
        let readiness_deadline = self
            .hooks
            .readiness_timeout()
            .map(|timeout| self.operation_deadline(timeout))
            .unwrap_or(request_deadline);
        let deadlines = crate::hooks::QueryDeadlines {
            request: request_deadline,
            readiness: readiness_deadline,
        };
        self.synchronize_documents(
            session,
            &request.workspace,
            &request.symbol.declaration,
            request_deadline,
        )?;

        let mut last_result = None;
        for attempt in 0..2 {
            self.check_operation()?;
            let (outcome, roundtrip_completed) = {
                let hooks = self.hooks.clone();
                let mut channel = CoreChannel {
                    core: &mut *self,
                    session: &mut *session,
                    workspace: &request.workspace,
                    roundtrip_completed: false,
                };
                let outcome = hooks.query(&mut channel, request, deadlines)?;
                (outcome, channel.roundtrip_completed)
            };
            // The first completed round-trip after synchronization is the
            // barrier: the server consumed the current document generation
            // before answering.
            if roundtrip_completed {
                self.confirm_sync_barrier();
            }
            let may_improve = outcome.may_improve_when_ready && !self.provider_is_ready();
            last_result = Some(outcome.result);
            if !may_improve || attempt == 1 {
                break;
            }
            self.wait_for_events();
        }
        last_result.ok_or(WorkerError::Cancelled)
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
        workspace: &chakra_engine::ProviderWorkspace,
        target: &SourceRange,
        deadline: Instant,
    ) -> Result<(), WorkerError> {
        self.check_operation()?;
        let hooks = self.hooks.clone();
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
            .delta_since_matching(
                &self.known_workspace,
                |language| hooks.synchronizes(language),
                self.active_operation
                    .as_ref()
                    .ok_or(WorkerError::Cancelled)?,
            )
            .map_err(|abort| match abort {
                OperationAbort::Cancelled => WorkerError::Cancelled,
                OperationAbort::DeadlineExceeded => WorkerError::Timeout,
            })?;
        let target_document = workspace
            .document(target.file())
            .filter(|document| hooks.synchronizes(document.language))
            .ok_or(WorkerError::InvalidPosition)?;
        let target_needs_open = !self.opened_versions.contains_key(target.file());
        if !deleted.is_empty()
            || !created.is_empty()
            || !changed.is_empty()
            || !inputs_created.is_empty()
            || !inputs_changed.is_empty()
            || !inputs_deleted.is_empty()
            || target_needs_open
        {
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
                &DidChangeWatchedFilesParams { changes: events },
                deadline,
            )?;
        }
        self.known_workspace = workspace.clone();
        self.known_revision = workspace.revision;
        let (workspace_documents, workspace_source_bytes) = workspace
            .document_stats_with_context_matching(
                |language| hooks.synchronizes(language),
                self.active_operation
                    .as_ref()
                    .ok_or(WorkerError::Cancelled)?,
            )
            .map_err(|abort| match abort {
                OperationAbort::Cancelled => WorkerError::Cancelled,
                OperationAbort::DeadlineExceeded => WorkerError::Timeout,
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
        root: &std::path::Path,
        path: &RepoRelativePath,
        source: &Arc<str>,
        deadline: Instant,
    ) -> Result<usize, WorkerError> {
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
                        language_id: self.hooks.language_id(path).to_owned(),
                        version: 1,
                        text: source.to_string(),
                    },
                },
                deadline,
            )?;
        }
        Ok(source.len())
    }

    fn start_session(&mut self) -> Result<(), WorkerError> {
        self.hooks.before_session_start()?;
        self.set_progress(ProviderProgress {
            stage: ProviderProgressStage::ProcessStartup,
            source: ProviderProgressSource::Chakra,
            message: Some(format!("starting the {} process", self.hooks.name())),
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
            self.hooks.name(),
        )
        .map_err(|error| WorkerError::transport(self.hooks.name(), error))?;
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
            .map_err(|error| WorkerError::from_client(self.hooks.name(), error));
        for event in pending {
            self.handle_event(event);
        }
        let result: InitializeResult = serde_json::from_value(result?)?;
        self.hooks.verify_capabilities(&result)?;
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

    fn restart_for(
        &mut self,
        workspace: &chakra_engine::ProviderWorkspace,
    ) -> Result<(), WorkerError> {
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

    fn send_request_value(
        &mut self,
        session: &mut Client,
        method: &str,
        params: &Value,
        deadline: Instant,
    ) -> Result<Value, WorkerError> {
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
        result.map_err(|error| WorkerError::from_client(self.hooks.name(), error))
    }

    fn send_notification<P: Serialize>(
        &mut self,
        session: &mut Client,
        method: &str,
        params: &P,
        deadline: Instant,
    ) -> Result<(), WorkerError> {
        session
            .notify(method, params, deadline)
            .map_err(|error| WorkerError::from_client(self.hooks.name(), error))
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
            let message = format!("{} process closed its output", self.hooks.name());
            self.set_state(ProviderState::Degraded, None, Some(message));
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

/// Query channel handed to hook query drivers: routes requests through the
/// worker-owned session, draining pending server events and tracking
/// round-trip completion for the synchronization barrier.
struct CoreChannel<'a, H: ProviderHooks> {
    core: &'a mut WorkerCore<H>,
    session: &'a mut Client,
    workspace: &'a chakra_engine::ProviderWorkspace,
    roundtrip_completed: bool,
}

impl<H: ProviderHooks> QueryChannel for CoreChannel<'_, H> {
    fn request(
        &mut self,
        method: &str,
        params: &Value,
        deadline: Instant,
    ) -> Result<Value, WorkerError> {
        let value = self
            .core
            .send_request_value(self.session, method, params, deadline)?;
        self.roundtrip_completed = true;
        Ok(value)
    }

    fn open_document(
        &mut self,
        path: &RepoRelativePath,
        deadline: Instant,
    ) -> Result<(), WorkerError> {
        let document = self
            .workspace
            .document(path)
            .ok_or(WorkerError::InvalidPosition)?;
        if self.core.opened_versions.contains_key(path) {
            return Ok(());
        }
        self.core.sync_generation = self.core.sync_generation.saturating_add(1);
        self.core.barrier_generation = None;
        let bytes = self.core.open_or_change(
            self.session,
            &self.workspace.repository_root,
            path,
            &document.source,
            deadline,
        )? as u64;
        self.core.sync_metrics.opened_documents = self.core.open_versions_len();
        self.core.sync_metrics.text_documents_sent =
            self.core.sync_metrics.text_documents_sent.saturating_add(1);
        self.core.sync_metrics.text_bytes_sent =
            self.core.sync_metrics.text_bytes_sent.saturating_add(bytes);
        self.core.sync_metrics.total_text_documents_sent = self
            .core
            .sync_metrics
            .total_text_documents_sent
            .saturating_add(1);
        self.core.sync_metrics.total_text_bytes_sent = self
            .core
            .sync_metrics
            .total_text_bytes_sent
            .saturating_add(bytes);
        self.core.publish_observability();
        Ok(())
    }
}
