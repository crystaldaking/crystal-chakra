//! Versioned query response envelope (SPEC §28).
//!
//! The envelope is a contract: its JSON shape is covered by roundtrip and
//! snapshot tests so adapters cannot silently change it.

use serde::{Deserialize, Serialize};

use crate::identity::WorkspaceId;
use crate::indexing::IndexingStatus;
use crate::revision::Revision;
use crate::state::{Freshness, ProviderState, WorkspaceStatus};

/// Current envelope schema version.
pub const SCHEMA_VERSION: u32 = 7;

/// Response section whose contents were cut by a bounded query operation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TruncationSection {
    StatusSyntaxDiagnostics,
    StatusProviders,
    RepoMapFiles,
    RepoMapOverview,
    SearchMatches,
    SearchMatchLine,
    SymbolSearchCandidates,
    ContextSource,
    ContextCallers,
    ContextCallees,
    ContextImplementations,
    ContextTests,
    ContextRelatedRelations,
    ContextSyntaxCallCandidates,
    ContextRelatedFiles,
    CallersCallers,
    CallersSyntaxCandidates,
    DiffContextChangedFiles,
    DiffContextChangedSymbols,
    DiffContextRelatedCallers,
    DiffContextRelatedTests,
    DiffContextRelatedRelations,
    DiffContextRelatedCallCandidates,
}

/// Budget that stopped one response section.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TruncationCause {
    ItemLimit,
    SyntaxDiagnosticPerFileLimit,
    /// Candidate/file/symbol inspection stopped at a deterministic work cap.
    ExaminedWorkLimit,
    /// Graph edge/call-site traversal stopped at a deterministic work cap.
    GraphTraversalLimit,
    /// Intermediate retained items reached the query construction cap.
    IntermediateAllocationLimit,
    /// Query construction stopped at the section wall-time safety cap.
    WallTimeLimit,
    SourceSnippetLineLimit,
    SourceSnippetCharacterLimit,
    ProviderLimit,
    ResponseByteLimit,
    UnresolvedCandidateFanout,
    DiffInventoryLimit,
}

/// One explicit explanation for an incomplete response section.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct TruncationDetail {
    pub section: TruncationSection,
    pub cause: TruncationCause,
    /// Configured bound in the unit implied by `cause`.
    pub limit: u64,
    /// Exact omitted amount when it is available without unbounded work.
    pub omitted: Option<u64>,
}

impl TruncationDetail {
    pub fn new(
        section: TruncationSection,
        cause: TruncationCause,
        limit: usize,
        omitted: Option<usize>,
    ) -> Self {
        Self {
            section,
            cause,
            limit: u64::try_from(limit).unwrap_or(u64::MAX),
            omitted: omitted.map(|value| u64::try_from(value).unwrap_or(u64::MAX)),
        }
    }
}

/// Metadata wrapper around every query response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct QueryEnvelope<T> {
    pub schema_version: u32,
    pub workspace_id: WorkspaceId,
    /// Revision the query actually observed.
    pub revision: Revision,
    pub freshness: Freshness,
    pub status: WorkspaceStatus,
    /// State of the precise provider relative to `revision` (SPEC §6).
    pub provider_state: ProviderState,
    /// Coverage and degradation of the syntax revision observed by this
    /// query. This is revision metadata, not a live mutable counter.
    pub indexing: IndexingStatus,
    /// True when a budget cut the result short (SPEC §29).
    pub truncated: bool,
    /// Deterministic per-section explanations for every truncation claim.
    pub truncation: Vec<TruncationDetail>,
    pub data: T,
}

impl<T> QueryEnvelope<T> {
    pub fn new(
        workspace_id: WorkspaceId,
        revision: Revision,
        freshness: Freshness,
        status: WorkspaceStatus,
        provider_state: ProviderState,
        mut truncation: Vec<TruncationDetail>,
        data: T,
    ) -> Self {
        truncation.sort();
        truncation.dedup();
        let truncated = !truncation.is_empty();
        Self {
            schema_version: SCHEMA_VERSION,
            workspace_id,
            revision,
            freshness,
            status,
            provider_state,
            indexing: IndexingStatus::default(),
            truncated,
            truncation,
            data,
        }
    }

    pub fn with_indexing(mut self, indexing: IndexingStatus) -> Self {
        self.indexing = indexing;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::WorkspaceIdentity;

    fn sample_envelope() -> Result<QueryEnvelope<serde_json::Value>, Box<dyn std::error::Error>> {
        let identity = WorkspaceIdentity::for_primary_worktree(std::path::Path::new("."))?;
        Ok(QueryEnvelope::new(
            identity.workspace,
            Revision(42),
            Freshness::Fresh,
            WorkspaceStatus::Ready,
            ProviderState::CatchingUp,
            Vec::new(),
            serde_json::json!({ "answer": 42 }),
        ))
    }

    #[test]
    fn envelope_json_matches_spec_shape() -> Result<(), Box<dyn std::error::Error>> {
        let envelope = sample_envelope()?;
        let json = serde_json::to_value(&envelope)?;
        assert_eq!(json["schema_version"], SCHEMA_VERSION);
        assert_eq!(json["revision"], 42);
        assert_eq!(json["freshness"], "fresh");
        assert_eq!(json["status"], "ready");
        assert_eq!(json["provider_state"], "catching_up");
        assert!(json["indexing"].is_object());
        assert_eq!(json["truncated"], false);
        assert_eq!(json["truncation"], serde_json::json!([]));
        let workspace_id = json["workspace_id"].as_str().unwrap_or("");
        assert!(workspace_id.starts_with("standalone-path:"));
        assert!(workspace_id.contains(":worktree:"));
        assert!(json["data"].is_object());
        Ok(())
    }

    #[test]
    fn envelope_roundtrips_through_json() -> Result<(), Box<dyn std::error::Error>> {
        let envelope = sample_envelope()?;
        let encoded = serde_json::to_string(&envelope)?;
        let decoded = serde_json::from_str::<QueryEnvelope<serde_json::Value>>(&encoded)?;
        assert_eq!(envelope, decoded);
        Ok(())
    }

    #[test]
    fn truncation_flag_is_derived_from_typed_details() -> Result<(), Box<dyn std::error::Error>> {
        let identity = WorkspaceIdentity::for_primary_worktree(std::path::Path::new("."))?;
        let envelope = QueryEnvelope::new(
            identity.workspace,
            Revision(42),
            Freshness::Fresh,
            WorkspaceStatus::Ready,
            ProviderState::NotConfigured,
            vec![TruncationDetail::new(
                TruncationSection::ContextSource,
                TruncationCause::SourceSnippetCharacterLimit,
                4_096,
                Some(3),
            )],
            serde_json::json!({ "answer": 42 }),
        );
        assert!(envelope.truncated);
        assert_eq!(envelope.truncation[0].omitted, Some(3));
        let json = serde_json::to_value(&envelope)?;
        assert_eq!(json["truncation"][0]["section"], "context_source");
        assert_eq!(
            json["truncation"][0]["cause"],
            "source_snippet_character_limit"
        );
        assert_eq!(json["truncation"][0]["limit"], 4_096);
        assert_eq!(json["truncation"][0]["omitted"], 3);
        Ok(())
    }
}
