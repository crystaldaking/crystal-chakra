//! Freshness and lifecycle states (SPEC §6, §37).

use serde::{Deserialize, Serialize};

/// Freshness of the data backing a response (SPEC §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// Reflects the latest reconciled filesystem state.
    Fresh,
    /// May lag the filesystem; only returned when the caller allowed it.
    Stale,
}

/// What a query requires from the underlying state.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessRequirement {
    /// The query must observe the latest reconciled state.
    #[default]
    RequireFresh,
    /// Slightly stale results are acceptable.
    AllowStale,
}

/// Synchronization state of the precise language provider (SPEC §6).
///
/// `CatchingUp` means the provider has not yet processed the current
/// revision; precise data derived from it must not be labeled current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    /// No provider is configured for this workspace.
    NotConfigured,
    /// Provider is starting up.
    Initializing,
    /// Provider has processed the current revision.
    Ready,
    /// Provider is processing; precise results are not current.
    CatchingUp,
    /// Provider failed or is unhealthy; precise results unavailable.
    Degraded,
}

/// Lifecycle state of the whole workspace service (SPEC §37).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    Initializing,
    Indexing,
    Ready,
    Degraded,
    Stale,
    Failed,
}
