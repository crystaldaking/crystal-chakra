//! Typed language hooks for the shared provider worker. Hooks carry exactly
//! the language-specific semantics — name, provenance, synchronized language
//! set, LSP language ids, capability verification, and the query strategy —
//! while the worker core owns every language-neutral mechanic.

use std::time::Instant;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::provenance::Provenance;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{PreciseQueryRequest, PreciseQueryResult};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CallHierarchyServerCapability, InitializeResult, PartialResultParams, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};
use serde_json::Value;

use crate::WorkerError;
use crate::convert;

/// Request/notification channel from a query hook into the worker-owned LSP
/// session. Implementations drain pending server events around every
/// round-trip and record round-trip completion for the synchronization
/// barrier.
pub trait QueryChannel {
    fn request(
        &mut self,
        method: &str,
        params: &Value,
        deadline: Instant,
    ) -> Result<Value, WorkerError>;
}

/// Outcome of one provider query attempt.
#[derive(Debug)]
pub struct QueryOutcome {
    pub result: PreciseQueryResult,
    /// True when the answer (typically an empty one) may improve once the
    /// provider finishes loading the workspace. The core then waits bounded
    /// and retries exactly once.
    pub may_improve_when_ready: bool,
}

impl QueryOutcome {
    pub fn ready(result: PreciseQueryResult) -> Self {
        Self {
            result,
            may_improve_when_ready: false,
        }
    }
}

/// Language-specific behavior of one worker-backed provider adapter.
pub trait ProviderHooks: Send + Sync + 'static {
    /// Stable operator-facing provider name, e.g. `vtsls`.
    fn name(&self) -> &'static str;

    /// Provenance attached to every precise fact this provider returns.
    fn provenance(&self) -> Provenance;

    /// Languages whose documents this provider's session synchronizes.
    fn synchronizes(&self, language: Language) -> bool;

    /// LSP language id for a synchronized document path.
    fn language_id(&self, path: &RepoRelativePath) -> &'static str;

    /// Verifies the initialized server actually serves this provider's
    /// precise operations.
    fn verify_capabilities(&self, result: &InitializeResult) -> Result<(), WorkerError>;

    /// Runs the provider-specific query after documents are synchronized.
    /// Every successful channel round-trip confirms the synchronization
    /// barrier; the core handles readiness waiting and the single retry.
    fn query(
        &self,
        channel: &mut dyn QueryChannel,
        request: &PreciseQueryRequest,
        deadline: Instant,
    ) -> Result<QueryOutcome, WorkerError>;
}

/// Stock query strategy for providers whose precise surface is the LSP
/// call-hierarchy trio. An empty prepare after the synchronization barrier is
/// a genuine "no item", not a reason to wait forever.
#[derive(Debug, Clone, Copy, Default)]
pub struct CallHierarchyDriver;

impl CallHierarchyDriver {
    pub fn query(
        &self,
        channel: &mut dyn QueryChannel,
        request: &PreciseQueryRequest,
        deadline: Instant,
        provenance: Provenance,
    ) -> Result<QueryOutcome, WorkerError> {
        let items = prepare_call_hierarchy(channel, request, deadline)?;
        let Some(item) = select_hierarchy_item(items, request)? else {
            // No hierarchy item: the answer may improve while the provider is
            // still loading; after readiness it is an honest empty result.
            return Ok(QueryOutcome {
                result: PreciseQueryResult {
                    revision: request.workspace.revision,
                    state: ProviderState::Ready,
                    fallback_cause: None,
                    incoming: Vec::new(),
                    outgoing: Vec::new(),
                    incoming_truncated: false,
                    outgoing_truncated: false,
                },
                may_improve_when_ready: true,
            });
        };
        let mut last_incoming = Vec::new();
        let mut last_outgoing = Vec::new();
        if request.directions.incoming {
            let params = CallHierarchyIncomingCallsParams {
                item: item.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            let value = channel.request(
                "callHierarchy/incomingCalls",
                &serde_json::to_value(params)?,
                deadline,
            )?;
            last_incoming =
                serde_json::from_value::<Option<Vec<CallHierarchyIncomingCall>>>(value)?
                    .unwrap_or_default();
        }
        if request.directions.outgoing {
            let params = CallHierarchyOutgoingCallsParams {
                item,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            let value = channel.request(
                "callHierarchy/outgoingCalls",
                &serde_json::to_value(params)?,
                deadline,
            )?;
            last_outgoing =
                serde_json::from_value::<Option<Vec<CallHierarchyOutgoingCall>>>(value)?
                    .unwrap_or_default();
        }
        let mut incoming_truncated = false;
        let incoming = convert::convert_incoming(
            last_incoming,
            &request.workspace,
            request.limit,
            provenance,
            &mut incoming_truncated,
        );
        let mut outgoing_truncated = false;
        let outgoing = convert::convert_outgoing(
            last_outgoing,
            &request.workspace,
            request.symbol.declaration.file(),
            request.limit,
            provenance,
            &mut outgoing_truncated,
        );
        Ok(QueryOutcome::ready(PreciseQueryResult {
            revision: request.workspace.revision,
            state: ProviderState::Ready,
            fallback_cause: None,
            incoming,
            outgoing,
            incoming_truncated,
            outgoing_truncated,
        }))
    }

    /// Capability gate shared by call-hierarchy providers.
    pub fn verify_call_hierarchy(result: &InitializeResult) -> Result<(), WorkerError> {
        let supported = matches!(
            result.capabilities.call_hierarchy_provider,
            Some(CallHierarchyServerCapability::Simple(true))
                | Some(CallHierarchyServerCapability::Options(_))
        );
        if supported {
            Ok(())
        } else {
            Err(WorkerError::Unsupported("call hierarchy".to_owned()))
        }
    }
}

fn prepare_call_hierarchy(
    channel: &mut dyn QueryChannel,
    request: &PreciseQueryRequest,
    deadline: Instant,
) -> Result<Vec<CallHierarchyItem>, WorkerError> {
    let document = request
        .workspace
        .document(request.symbol.declaration.file())
        .ok_or(WorkerError::InvalidPosition)?;
    let position = convert::find_symbol_position(
        &document.source,
        &request.symbol.name,
        &request.symbol.declaration,
    )?;
    let uri = convert::path_to_uri(
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
    let value = channel.request(
        "textDocument/prepareCallHierarchy",
        &serde_json::to_value(params)?,
        deadline,
    )?;
    Ok(serde_json::from_value::<Option<Vec<CallHierarchyItem>>>(value)?.unwrap_or_default())
}

fn select_hierarchy_item(
    items: Vec<CallHierarchyItem>,
    request: &PreciseQueryRequest,
) -> Result<Option<CallHierarchyItem>, WorkerError> {
    if items.is_empty() {
        return Ok(None);
    }
    let mut matching = items.into_iter().filter(|item| {
        if item.name != request.symbol.name {
            return false;
        }
        convert::item_declaration(item, &request.workspace).is_some_and(|(path, selection)| {
            path == *request.symbol.declaration.file()
                && selection.start() >= request.symbol.declaration.start()
                && selection.end() <= request.symbol.declaration.end()
        })
    });
    let Some(item) = matching.next() else {
        return Err(WorkerError::InvalidPosition);
    };
    if matching.next().is_some() {
        return Err(WorkerError::InvalidPosition);
    }
    Ok(Some(item))
}
