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
pub const SCHEMA_VERSION: u32 = 4;

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
    pub data: T,
}

impl<T> QueryEnvelope<T> {
    pub fn new(
        workspace_id: WorkspaceId,
        revision: Revision,
        freshness: Freshness,
        status: WorkspaceStatus,
        provider_state: ProviderState,
        truncated: bool,
        data: T,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            workspace_id,
            revision,
            freshness,
            status,
            provider_state,
            indexing: IndexingStatus::default(),
            truncated,
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
            false,
            serde_json::json!({ "answer": 42 }),
        ))
    }

    #[test]
    fn envelope_json_matches_spec_shape() -> Result<(), Box<dyn std::error::Error>> {
        let envelope = sample_envelope()?;
        let json = serde_json::to_value(&envelope)?;
        assert_eq!(json["schema_version"], 4);
        assert_eq!(json["revision"], 42);
        assert_eq!(json["freshness"], "fresh");
        assert_eq!(json["status"], "ready");
        assert_eq!(json["provider_state"], "catching_up");
        assert!(json["indexing"].is_object());
        assert_eq!(json["truncated"], false);
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
}
