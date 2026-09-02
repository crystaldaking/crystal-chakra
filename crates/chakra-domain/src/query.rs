//! Query/application contracts (SPEC §23–§29).
//!
//! These types are the MCP-independent application interface: adapters map
//! transports onto [`QueryService`], and every response is wrapped in a
//! [`QueryEnvelope`]. v0.1 exposes exactly the seven queries listed in
//! `docs/roadmap/v0.1.md` §3. Optional adapters fail with typed errors rather
//! than returning placeholder data.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::composition::SourceLayer;
use crate::diagnostic::{DiagnosticTruncationCause, SyntaxDiagnostic};
use crate::envelope::QueryEnvelope;
use crate::identity::{WorkspaceId, WorkspaceIdentity};
use crate::location::{RepoRelativePath, SourceRange};
use crate::operation::{OperationAbort, OperationContext};
use crate::project::{
    ProjectDependency, ProjectManifestIssue, ProjectScopeSelector, ProjectSourceRoot,
    ProjectUnitId, ProjectUnitKind,
};
use crate::provenance::{Precision, Provenance};
use crate::revision::Revision;
use crate::source::{
    SourceClassification, SourceMetadata, SourceMetadataCoverage, SourcePackage, SourceRole,
};
use crate::state::{Freshness, FreshnessRequirement, ProviderState};
use crate::symbol::{
    CallForm, CallResolution, CallTargetKind, EdgeKind, EntityId, Language, SymbolKind,
};

/// Default result budget when a request does not specify one (SPEC §29).
pub const DEFAULT_QUERY_LIMIT: u32 = 20;
/// Hard upper bound for any requested limit; responses are never unbounded.
pub const MAX_QUERY_LIMIT: u32 = 500;
/// Fixed status budget for actionable syntax diagnostics.
pub const MAX_STATUS_DIAGNOSTICS: usize = 100;

/// Reference to a symbol for entity-based queries (SPEC §24).
///
/// Preferred flow: `symbol_search` → pick a candidate → address it by id.
/// `ById` carries the revision the id was taken from (the envelope revision
/// of the response that returned it); an [`EntityId`] is an opaque
/// revision-local key and is meaningless once a newer revision is published,
/// so resolution fails
/// with [`QueryError::StaleSymbolRef`] on a mismatch instead of silently
/// returning the wrong symbol. `ByName` resolves only when it is
/// unambiguous; ambiguity is returned as [`QueryError::AmbiguousSymbol`],
/// never guessed away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolRef {
    ById { id: EntityId, revision: Revision },
    ByName(String),
}

/// Flattened symbol view used inside query responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SymbolView {
    pub id: EntityId,
    pub language: Language,
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub location: SourceRange,
    pub signature: Option<String>,
    pub provenance: Provenance,
    pub precision: Precision,
    pub source_layer: SourceLayer,
    pub source_role: SourceRole,
    pub source_classification: SourceClassification,
    pub package: Option<SourcePackage>,
}

impl SymbolView {
    /// Builds a response view using metadata captured in the symbol's graph
    /// revision rather than reclassifying its path at query time.
    pub fn from_symbol_with_metadata(
        symbol: &crate::symbol::Symbol,
        metadata: &SourceMetadata,
    ) -> Self {
        Self {
            id: symbol.id,
            language: symbol.key.language,
            name: symbol.name().to_owned(),
            qualified_name: symbol.key.qualified_name.clone(),
            kind: symbol.key.kind,
            location: symbol.location.clone(),
            signature: symbol.signature.clone(),
            provenance: symbol.provenance,
            precision: symbol.precision,
            source_layer: SourceLayer::CommitSnapshot,
            source_role: metadata.role,
            source_classification: metadata.classification,
            package: metadata.package.clone(),
        }
    }
}

impl From<&crate::symbol::Symbol> for SymbolView {
    fn from(symbol: &crate::symbol::Symbol) -> Self {
        let metadata = SourceMetadata::path_fallback(&symbol.key.path);
        Self::from_symbol_with_metadata(symbol, &metadata)
    }
}

/// A typed relation to another symbol, as seen in query responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RelatedSymbol {
    pub symbol: SymbolView,
    pub edge_kind: EdgeKind,
    pub provenance: Provenance,
    pub precision: Precision,
    /// Total relations represented by this caller/target entry.
    pub occurrence_count: u64,
    /// Small deterministic sample of known relation ranges (e.g. call sites).
    pub representative_locations: Vec<SourceRange>,
    /// Known relation ranges omitted from `representative_locations`.
    pub locations_omitted: u64,
    /// Bounded syntax evidence for materialized heuristic call relations.
    /// Precise-provider and non-call relations do not fabricate this field.
    pub representative_call_sites: Vec<CallSiteEvidence>,
    /// Known call-site evidence omitted from `representative_call_sites`.
    pub call_site_evidence_omitted: u64,
}

/// Direction of a typed relation relative to the symbol whose context was
/// requested.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RelationDirection {
    Incoming,
    Outgoing,
}

/// A bounded non-call relationship whose direction cannot be inferred from
/// the response section name alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DirectedRelatedSymbol {
    pub direction: RelationDirection,
    pub relation: RelatedSymbol,
}

/// One representative syntax call expression retained inside an aggregated
/// caller/target entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallSiteEvidence {
    pub form: CallForm,
    pub target_kind: CallTargetKind,
    pub name: String,
    pub qualifier: Option<String>,
    pub receiver_type: Option<String>,
    pub receiver_type_source: Option<crate::symbol::ReceiverTypeSource>,
    pub receiver_hint: Option<String>,
    pub location: SourceRange,
    pub resolution: CallResolution,
}

/// Bounded syntax call-site evidence that was not materialized as a graph
/// edge because its target is ambiguous or unresolved. Repeated sites are
/// aggregated by caller and candidate target so they consume one result slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallSiteView {
    pub caller: SymbolView,
    /// One possible target for an ambiguous call, or `None` when no target can
    /// be justified from syntax alone.
    pub candidate_target: Option<SymbolView>,
    pub occurrence_count: u64,
    pub representative_evidence: Vec<CallSiteEvidence>,
    pub evidence_omitted: u64,
    pub provenance: Provenance,
    pub precision: Precision,
}

// --- status ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StatusRequest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexCounts {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    pub call_sites: u64,
    pub ambiguous_call_sites: u64,
    pub unresolved_call_sites: u64,
    /// Workspace call sites whose syntax candidate set was cut before query
    /// time. The lazy call-site model normally keeps this at zero; it remains
    /// separate from response-local truncation.
    pub call_sites_with_truncated_candidates: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    IncomingCalls,
    OutgoingCalls,
    SynchronizationState,
    ProgressReporting,
    RevisionDeltaSynchronization,
    CacheMetrics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProgressStage {
    ProcessStartup,
    Initialization,
    CargoMetadata,
    WorkspaceLoading,
    DocumentSynchronization,
    Indexing,
    Ready,
    Degraded,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProgressSource {
    /// Directly reported by the provider protocol.
    Provider,
    /// Inferred from a Chakra-owned lifecycle or synchronization step.
    Chakra,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderProgress {
    pub stage: ProviderProgressStage,
    pub source: ProviderProgressSource,
    pub message: Option<String>,
    pub percentage: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderCacheMetrics {
    pub entries: u64,
    pub bytes: u64,
    pub max_entries: u64,
    pub max_bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderDocumentSyncMetrics {
    pub revision: Option<Revision>,
    pub workspace_documents: u64,
    pub workspace_source_bytes: u64,
    pub opened_documents: u64,
    pub created: u64,
    pub changed: u64,
    pub deleted: u64,
    pub text_documents_sent: u64,
    pub text_bytes_sent: u64,
    pub watched_file_events: u64,
    pub documents_examined: u64,
    pub source_body_comparisons: u64,
    pub total_text_documents_sent: u64,
    pub total_text_bytes_sent: u64,
    pub total_watched_file_events: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderMetrics {
    pub cache: ProviderCacheMetrics,
    pub document_sync: ProviderDocumentSyncMetrics,
}

/// Provider admission latency with self-describing priority names on the
/// transport-neutral status contract.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct ProviderQueueLatencyByPriority {
    pub background: crate::scheduling::QueueLatencyStats,
    pub normal: crate::scheduling::QueueLatencyStats,
    pub interactive: crate::scheduling::QueueLatencyStats,
}

/// Provider-pool lifecycle and admission counters. Reservations are
/// deterministic configuration bounds, not process RSS measurements.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceProviderOrchestrationMetrics {
    pub active_providers: u64,
    pub max_active_providers: u64,
    pub reserved_memory_bytes: u64,
    pub max_reserved_memory_bytes: u64,
}

/// Process-global provider-pool counters plus the selected worktree's local
/// resource envelope when observed through a workspace-bound provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderOrchestrationMetrics {
    pub configured_providers: u64,
    pub configured_workspaces: u64,
    pub active_providers: u64,
    pub max_active_providers: u64,
    pub reserved_memory_bytes: u64,
    pub max_reserved_memory_bytes: u64,
    pub running_queries: u64,
    pub queued_queries: u64,
    pub max_concurrent_queries: u64,
    pub max_queued_queries: u64,
    pub activations: u64,
    pub activation_failures: u64,
    pub activation_timeouts: u64,
    pub idle_shutdowns: u64,
    pub resource_evictions: u64,
    pub shutdown_failures: u64,
    pub saturated_queries: u64,
    pub queue_timeouts: u64,
    pub cancelled_queries: u64,
    /// Admission queue wait per self-describing provider priority (issue #44).
    #[serde(default)]
    pub queue_latency_by_priority: ProviderQueueLatencyByPriority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceProviderOrchestrationMetrics>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderInfo {
    /// Provider name, e.g. `rust-analyzer`.
    pub name: String,
    /// Languages for which this provider may supply precise facts.
    pub languages: Vec<Language>,
    /// Chakra operations backed by this provider adapter.
    pub capabilities: Vec<ProviderCapability>,
    pub state: ProviderState,
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProviderProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<ProviderMetrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_wait_budget_millis: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFallbackCause {
    QueueSaturated,
    QueueTimedOut,
    ActivationCapacity,
    ActivationFailed,
    ActivationTimedOut,
    Cancelled,
    ProviderStopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderQueryInfo {
    pub name: String,
    pub state: ProviderState,
    pub fallback_used: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_cause: Option<ProviderFallbackCause>,
    pub fallback_reason: Option<String>,
    pub last_error: Option<String>,
    pub progress: Option<ProviderProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<ProviderMetrics>,
    pub wait_budget_millis: Option<u64>,
}

/// Transport-neutral operational counters for the bounded query executor.
/// Adapters that do not own such an executor leave this absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct QueryExecutionMetrics {
    pub queued: u64,
    pub running: u64,
    pub started: u64,
    pub cancelled: u64,
    pub queue_timed_out: u64,
    pub execution_timed_out: u64,
    pub completed: u64,
    pub failed: u64,
    pub permit_hold_micros_total: u64,
    pub permit_hold_micros_max: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StatusData {
    pub workspace: WorkspaceIdentity,
    pub counts: IndexCounts,
    pub providers: Vec<ProviderInfo>,
    /// Process-global provider-pool lifecycle/admission counters plus the
    /// selected worktree's local resource envelope, reported once instead of
    /// repeated per provider (issues #47 and #61).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_pool: Option<ProviderOrchestrationMetrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_execution: Option<QueryExecutionMetrics>,
    pub source_metadata: SourceMetadataCoverage,
    pub syntax_diagnostics: SyntaxDiagnosticSummary,
    /// Bounded machine-readable live indexing diagnostics (issue #43).
    /// Absent when no live indexing owner is installed on the engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_diagnostics: Option<crate::indexing::IndexingDiagnostics>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SyntaxDiagnosticSummary {
    pub files_with_diagnostics: u64,
    pub total_diagnostics: u64,
    pub diagnostics: Vec<SyntaxDiagnostic>,
    pub omitted_diagnostics: u64,
    pub truncated: bool,
    pub truncation_causes: Vec<DiagnosticTruncationCause>,
}

// --- repo_map ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepoMapRequest {
    /// Empty means every indexed language.
    #[serde(default)]
    pub include_languages: Vec<Language>,
    #[serde(default)]
    pub source: SourceFilter,
    /// Also emit the typed project-unit summary section on the first page
    /// (issue #41). The section respects the same revision and source scope
    /// as `files`.
    #[serde(default)]
    pub include_project_scope: bool,
    /// Self-contained continuation returned by an earlier `repo_map` page.
    /// Filters must be omitted when a cursor is supplied because the cursor
    /// already carries the normalized scope.
    #[serde(default)]
    pub cursor: Option<RepoMapCursor>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileSummary {
    pub path: RepoRelativePath,
    pub language: Language,
    pub symbol_count: u64,
    pub provenance: Provenance,
    pub precision: Precision,
    pub source_layer: SourceLayer,
    pub source_role: SourceRole,
    pub source_classification: SourceClassification,
    pub package: Option<SourcePackage>,
}

/// Scope captured inside a revision-scoped repository-map cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepoMapScope {
    pub include_languages: Vec<Language>,
    pub source: SourceFilter,
}

/// Stable continuation for the exact filtered path ordering of one revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepoMapCursor {
    pub workspace_id: WorkspaceId,
    pub revision: Revision,
    pub after: RepoRelativePath,
    pub scope: RepoMapScope,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RepoMapGroupKind {
    TopLevelDirectory,
    CargoPackage,
    ComposerPsr4,
    NpmPackage,
    PyprojectPackage,
    MavenModule,
    GradleProject,
    DotnetProject,
    ShellProject,
    CppProject,
    TerraformModule,
    GoModule,
}

/// Overlapping structural aggregation used by the first repository-map page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepoMapGroup {
    pub kind: RepoMapGroupKind,
    pub name: String,
    /// `None` denotes the repository root.
    pub root: Option<RepoRelativePath>,
    pub language: Language,
    pub file_count: u64,
    pub symbol_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepoMapData {
    /// Present only on the first page; ranked by structural usefulness and
    /// bounded by the same result limit as `files`.
    pub overview: Vec<RepoMapGroup>,
    pub overview_truncated: bool,
    pub files: Vec<FileSummary>,
    pub next_cursor: Option<RepoMapCursor>,
    pub source_metadata: SourceMetadataCoverage,
    /// Typed project-unit summary requested via
    /// [`RepoMapRequest::include_project_scope`]; present only on the first
    /// page (issue #41).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_scope: Option<ProjectScopeData>,
}

/// One project unit summarized against the files of the same filtered
/// `repo_map` page scope (issue #41).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectUnitSummary {
    pub id: ProjectUnitId,
    pub kind: ProjectUnitKind,
    pub name: String,
    /// Repository-relative unit directory. `None` denotes the repository
    /// root.
    pub root: Option<RepoRelativePath>,
    pub manifest: Option<RepoRelativePath>,
    pub source_roots: Vec<ProjectSourceRoot>,
    pub dependencies: Vec<ProjectDependency>,
    pub file_count: u64,
    pub symbol_count: u64,
}

/// Typed project-model summary for one filtered `repo_map` first page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectScopeData {
    /// Units owning at least one file in the filtered scope, ordered by id.
    pub units: Vec<ProjectUnitSummary>,
    /// Filtered files claimed by several units at the same deepest root.
    /// They are reported here instead of being silently assigned to one.
    pub ambiguous_files: u64,
    /// Filtered files no retained unit claims (for example because unit
    /// bounds were hit).
    pub unassigned_files: u64,
    /// Manifests whose evidence degraded to path fallback in this revision.
    pub issues: Vec<ProjectManifestIssue>,
}

/// Language-neutral source scope shared by repository and symbol queries.
/// Empty include/exclude lists preserve access to every indexed role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SourceFilter {
    /// Exact ecosystem package/project name when metadata is available.
    #[serde(default)]
    pub package: Option<String>,
    /// Repository-relative file or directory prefix.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Empty means every role. Otherwise only listed roles are eligible.
    #[serde(default)]
    pub include_roles: Vec<SourceRole>,
    /// Applied after `include_roles`.
    #[serde(default)]
    pub exclude_roles: Vec<SourceRole>,
    /// Typed project-unit scope (issue #41). Files whose ownership is
    /// ambiguous match no unit selector; path-fallback units are selectable
    /// like any other unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectScopeSelector>,
}

// --- search (text) ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
    pub query: String,
    /// Interpret `query` as a Rust `regex` expression instead of a literal
    /// substring.
    #[serde(default)]
    pub regex: bool,
    /// Match case exactly. Literal and regex modes are case-insensitive by
    /// default for agent-oriented discovery.
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TextMatch {
    pub file: RepoRelativePath,
    pub range: SourceRange,
    pub line: String,
    /// The matching source line exceeded the response snippet budget.
    pub line_truncated: bool,
    pub provenance: Provenance,
    pub precision: Precision,
    pub source_layer: SourceLayer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchData {
    pub matches: Vec<TextMatch>,
}

// --- symbol_search ---

/// Matching strategy for [`SymbolSearchRequest`] (issue #82).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SymbolMatchMode {
    /// Case-insensitive substring/prefix ranking across the symbol graph.
    #[default]
    Substring,
    /// Only symbols whose case-folded simple or qualified name equals the
    /// query. Reads the exact-name index without scanning unrelated substring
    /// candidates; truncation is reported only when the exact-name candidate
    /// set itself exceeds the response limit.
    Exact,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SymbolSearchRequest {
    pub query: String,
    #[serde(default)]
    pub match_mode: SymbolMatchMode,
    /// Empty means every indexed language.
    #[serde(default)]
    pub include_languages: Vec<Language>,
    /// Empty means every symbol kind.
    #[serde(default)]
    pub include_kinds: Vec<SymbolKind>,
    /// Applied after `include_kinds`; imports can be excluded explicitly.
    #[serde(default)]
    pub exclude_kinds: Vec<SymbolKind>,
    /// Exact, case-sensitive qualified-name segment prefix.
    #[serde(default)]
    pub namespace_prefix: Option<String>,
    #[serde(default)]
    pub source: SourceFilter,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SymbolSearchData {
    pub query: String,
    pub candidates: Vec<SymbolView>,
}

// --- context ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextRequest {
    pub symbol: Option<SymbolRef>,
    /// Restricts the related sections (callers, callees, implementations,
    /// tests, relations, call candidates) to a source scope (issue #41). The
    /// anchor symbol itself is never filtered out.
    #[serde(default)]
    pub source: SourceFilter,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

/// A declaration snippet captured from the same immutable revision as the
/// symbol graph. Both line and character budgets are enforced by the query
/// layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SourceSnippet {
    pub range: SourceRange,
    pub text: String,
    pub truncated: bool,
    pub provenance: Provenance,
    pub precision: Precision,
    pub source_layer: SourceLayer,
}

/// Bounded, provenance-tagged context around one symbol (SPEC §25).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextData {
    pub symbol: SymbolView,
    pub source: Option<SourceSnippet>,
    pub callers: Vec<RelatedSymbol>,
    pub callees: Vec<RelatedSymbol>,
    pub implementations: Vec<RelatedSymbol>,
    pub tests: Vec<RelatedSymbol>,
    /// Typed dependency/framework relations, including deterministic
    /// container, route, dispatch, listener, schedule, and policy facts.
    pub related_relations: Vec<DirectedRelatedSymbol>,
    pub syntax_call_candidates: Vec<CallSiteView>,
    /// Files referenced by the bounded response items above.
    pub related_files: Vec<RepoRelativePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderQueryInfo>,
}

// --- callers ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallersRequest {
    pub symbol: Option<SymbolRef>,
    /// Restricts callers and syntax candidates to a source scope (issue #41).
    /// The target symbol itself is never filtered out.
    #[serde(default)]
    pub source: SourceFilter,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallersData {
    pub target: SymbolView,
    pub callers: Vec<RelatedSymbol>,
    pub syntax_candidates: Vec<CallSiteView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderQueryInfo>,
}

// --- diff_context ---

/// Git baseline used by [`DiffContextRequest`]. Every scope compares its
/// resolved baseline commit with the final materialized worktree, so staged,
/// unstaged, and untracked changes remain visible alongside committed changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffScope {
    /// Compare `HEAD` with the materialized worktree. This is the v0.1
    /// behavior and remains the default for backward compatibility.
    #[default]
    Worktree,
    /// Compare the named Git commit-ish directly with the materialized
    /// worktree (two-dot-style feature review semantics).
    BaseRef { reference: String },
    /// Compare the unique merge-base of the named commit-ish and `HEAD` with
    /// the materialized worktree (three-dot-style feature review semantics).
    MergeBase { reference: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiffContextRequest {
    #[serde(default)]
    pub scope: DiffScope,
    /// Restricts the changed/symbol sections to a source scope (issue #41).
    /// Changed files are matched path-structurally: files the current graph
    /// does not index (deleted files, manifests) use deterministic
    /// path-fallback roles and model unit roots.
    #[serde(default)]
    pub source: SourceFilter,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub freshness: FreshnessRequirement,
}

/// Reproducible Git baseline selected for one completed diff query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ResolvedDiffScope {
    pub requested: DiffScope,
    /// Immutable commit object used as the baseline. This is `None` only for
    /// the default scope in an unborn repository.
    pub base_commit: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChangedFile {
    /// Current path, or the former path when the file was deleted.
    pub path: RepoRelativePath,
    /// Former path for a Git-detected rename.
    pub previous_path: Option<RepoRelativePath>,
    pub change: ChangeKind,
    pub provenance: Provenance,
    pub precision: Precision,
}

/// Why a current symbol appears in `diff_context`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangedSymbolBasis {
    /// The declaration belongs to a current file changed relative to the
    /// selected baseline. Chakra does not claim that the declaration or body
    /// overlaps a diff hunk.
    DeclaredInChangedFile,
}

/// A current syntax symbol selected by the documented diff heuristic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChangedSymbol {
    pub symbol: SymbolView,
    pub basis: ChangedSymbolBasis,
    pub provenance: Provenance,
    pub precision: Precision,
}

/// One caller/test relation anchored to a symbol returned in the same
/// `diff_context` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiffRelatedSymbol {
    pub changed_symbol_id: EntityId,
    pub relation: RelatedSymbol,
}

/// One ambiguous syntax call candidate anchored to a changed symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiffCallSite {
    pub changed_symbol_id: EntityId,
    pub call_site: CallSiteView,
}

/// A directed relation anchored to a changed symbol in `diff_context`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiffDirectedRelatedSymbol {
    pub changed_symbol_id: EntityId,
    pub relation: DirectedRelatedSymbol,
}

/// Bounded structured result of a diff walk (SPEC §26). Facts must be
/// distinguishable from heuristics through their precision fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiffContextData {
    pub scope: ResolvedDiffScope,
    pub changed_files: Vec<ChangedFile>,
    pub changed_symbols: Vec<ChangedSymbol>,
    pub related_callers: Vec<DiffRelatedSymbol>,
    pub related_tests: Vec<DiffRelatedSymbol>,
    pub related_relations: Vec<DiffDirectedRelatedSymbol>,
    pub related_call_candidates: Vec<DiffCallSite>,
}

// --- errors and the service contract ---

/// Typed query failures. Adapters map these onto transport errors; core
/// code never sees transport error types.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QueryError {
    #[error("query `{0}` is not implemented yet")]
    Unsupported(&'static str),
    #[error("symbol reference is required")]
    MissingSymbolRef,
    #[error(
        "symbol reference was taken from revision {reference_revision}, but the published revision is {current_revision}; re-resolve via symbol_search"
    )]
    StaleSymbolRef {
        reference_revision: Revision,
        current_revision: Revision,
    },
    #[error(
        "repository-map cursor was created for revision {cursor_revision}, but the published revision is {current_revision}; restart repo_map without a cursor"
    )]
    StaleCursor {
        cursor_revision: Revision,
        current_revision: Revision,
    },
    #[error(
        "repository-map cursor belongs to workspace {cursor_workspace}, but this query targets {current_workspace}; restart repo_map without a cursor"
    )]
    CursorWorkspaceMismatch {
        cursor_workspace: WorkspaceId,
        current_workspace: WorkspaceId,
    },
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
    #[error(
        "ambiguous symbol reference `{query}`: {candidates} candidates; resolve via symbol_search and EntityId"
    )]
    AmbiguousSymbol { query: String, candidates: usize },
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error(
        "freshness requirement {required:?} is not met: the published snapshot is {actual:?}; retry after reconciliation or allow stale results"
    )]
    FreshnessNotMet {
        required: FreshnessRequirement,
        actual: Freshness,
    },
    #[error("fresh syntax state is unavailable: {0}")]
    FreshnessUnavailable(String),
    #[error("Git diff state is unavailable: {0}")]
    DiffUnavailable(String),
    #[error("query execution was cancelled by the caller")]
    Cancelled,
    #[error("query exceeded its end-to-end execution deadline")]
    ExecutionDeadlineExceeded,
    #[error("query response construction failed: {0}")]
    ResponseConstruction(String),
    #[error("no workspaces are registered")]
    NoWorkspacesRegistered,
    #[error("workspace is not registered: {0}")]
    WorkspaceNotFound(WorkspaceId),
    #[error("multiple workspaces are registered; specify workspace_id (available: {available:?})")]
    WorkspaceSelectionRequired { available: Vec<WorkspaceId> },
}

impl From<OperationAbort> for QueryError {
    fn from(abort: OperationAbort) -> Self {
        match abort {
            OperationAbort::Cancelled => Self::Cancelled,
            OperationAbort::DeadlineExceeded => Self::ExecutionDeadlineExceeded,
        }
    }
}

/// The MCP-independent application interface (SPEC §23).
///
/// Implementations must guarantee that a call observes exactly one
/// published revision (SPEC §5), that large results respect budgets
/// (SPEC §29), and that a request's [`FreshnessRequirement`] is either met
/// or rejected with a typed freshness error — a `RequireFresh` request must
/// never be silently served from a stale snapshot. Methods are synchronous:
/// v0.1 queries run against an in-memory snapshot and adapters may use a
/// blocking worker when reconciliation is required.
pub trait QueryService: Send + Sync {
    fn status(&self, request: StatusRequest) -> Result<QueryEnvelope<StatusData>, QueryError>;
    fn repo_map(&self, request: RepoMapRequest) -> Result<QueryEnvelope<RepoMapData>, QueryError> {
        self.repo_map_with_context(request, &OperationContext::unbounded())
    }
    fn search(&self, request: SearchRequest) -> Result<QueryEnvelope<SearchData>, QueryError> {
        self.search_with_context(request, &OperationContext::unbounded())
    }
    fn symbol_search(
        &self,
        request: SymbolSearchRequest,
    ) -> Result<QueryEnvelope<SymbolSearchData>, QueryError> {
        self.symbol_search_with_context(request, &OperationContext::unbounded())
    }
    fn context(&self, request: ContextRequest) -> Result<QueryEnvelope<ContextData>, QueryError> {
        self.context_with_context(request, &OperationContext::unbounded())
    }
    fn callers(&self, request: CallersRequest) -> Result<QueryEnvelope<CallersData>, QueryError> {
        self.callers_with_context(request, &OperationContext::unbounded())
    }
    fn diff_context(
        &self,
        request: DiffContextRequest,
    ) -> Result<QueryEnvelope<DiffContextData>, QueryError> {
        self.diff_context_with_context(request, &OperationContext::unbounded())
    }

    /// Context-aware entry points used by bounded execution adapters. These
    /// are the required implementation contract; legacy/direct entry points
    /// above supply an unbounded context for non-MCP callers.
    fn repo_map_with_context(
        &self,
        request: RepoMapRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<RepoMapData>, QueryError>;

    fn search_with_context(
        &self,
        request: SearchRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<SearchData>, QueryError>;

    fn symbol_search_with_context(
        &self,
        request: SymbolSearchRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<SymbolSearchData>, QueryError>;

    fn context_with_context(
        &self,
        request: ContextRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<ContextData>, QueryError>;

    fn callers_with_context(
        &self,
        request: CallersRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<CallersData>, QueryError>;

    fn diff_context_with_context(
        &self,
        request: DiffContextRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<DiffContextData>, QueryError>;
}

/// Process-local catalog used by adapters to select one independently owned
/// materialized worktree before invoking the ordinary single-workspace query
/// contract. Routing never combines revisions or graphs.
pub trait WorkspaceQueryRouter: Send + Sync {
    fn workspaces(&self) -> Result<Vec<WorkspaceIdentity>, QueryError>;

    /// Resolves an explicit workspace, or the sole registered workspace when
    /// `requested` is absent. An omitted selector is rejected once several
    /// worktrees are ready so routing can never depend on registration order.
    fn route(&self, requested: Option<&WorkspaceId>) -> Result<Arc<dyn QueryService>, QueryError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceCatalogData {
    pub workspaces: Vec<WorkspaceIdentity>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_are_bounded() {
        let default = DEFAULT_QUERY_LIMIT;
        let max = MAX_QUERY_LIMIT;
        assert!(default > 0 && default <= max);
    }

    #[test]
    fn symbol_ref_serializes_as_snake_case_tagged() -> Result<(), Box<dyn std::error::Error>> {
        let by_name = serde_json::to_value(SymbolRef::ByName("refund".to_owned()))?;
        assert_eq!(by_name, serde_json::json!({ "by_name": "refund" }));
        let by_id = serde_json::to_value(SymbolRef::ById {
            id: EntityId(7),
            revision: Revision(42),
        })?;
        assert_eq!(
            by_id,
            serde_json::json!({ "by_id": { "id": 7, "revision": 42 } })
        );
        Ok(())
    }

    #[test]
    fn request_defaults_require_fresh() {
        let request = SymbolSearchRequest::default();
        assert_eq!(request.freshness, FreshnessRequirement::RequireFresh);
    }

    #[test]
    fn provider_queue_latency_serializes_with_named_priorities()
    -> Result<(), Box<dyn std::error::Error>> {
        let metrics = ProviderQueueLatencyByPriority {
            interactive: crate::scheduling::QueueLatencyStats {
                samples: 1,
                total_micros: 7,
                max_micros: 7,
            },
            ..ProviderQueueLatencyByPriority::default()
        };

        let json = serde_json::to_value(metrics)?;
        assert_eq!(json["interactive"]["samples"], 1);
        assert!(json.get("normal").is_some());
        assert!(json.get("background").is_some());
        Ok(())
    }

    #[test]
    fn provider_pool_metrics_serialize_global_and_workspace_envelopes()
    -> Result<(), Box<dyn std::error::Error>> {
        let metrics = ProviderOrchestrationMetrics {
            configured_providers: 2,
            configured_workspaces: 3,
            active_providers: 2,
            max_active_providers: 4,
            reserved_memory_bytes: 20,
            max_reserved_memory_bytes: 40,
            workspace: Some(WorkspaceProviderOrchestrationMetrics {
                active_providers: 1,
                max_active_providers: 2,
                reserved_memory_bytes: 10,
                max_reserved_memory_bytes: 20,
            }),
            ..ProviderOrchestrationMetrics::default()
        };

        let json = serde_json::to_value(metrics)?;
        assert_eq!(json["configured_providers"], 2);
        assert_eq!(json["configured_workspaces"], 3);
        assert_eq!(json["active_providers"], 2);
        assert_eq!(json["max_active_providers"], 4);
        assert_eq!(json["reserved_memory_bytes"], 20);
        assert_eq!(json["max_reserved_memory_bytes"], 40);
        assert_eq!(json["workspace"]["active_providers"], 1);
        assert_eq!(json["workspace"]["max_active_providers"], 2);
        assert_eq!(json["workspace"]["reserved_memory_bytes"], 10);
        assert_eq!(json["workspace"]["max_reserved_memory_bytes"], 20);

        let without_workspace = serde_json::to_value(ProviderOrchestrationMetrics::default())?;
        assert!(without_workspace.get("workspace").is_none());
        Ok(())
    }

    #[test]
    fn diff_scope_defaults_to_worktree_and_has_a_typed_wire_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let default: DiffContextRequest = serde_json::from_value(serde_json::json!({}))?;
        assert_eq!(default.scope, DiffScope::Worktree);

        let merge_base: DiffContextRequest = serde_json::from_value(serde_json::json!({
            "scope": {
                "kind": "merge_base",
                "reference": "origin/develop"
            }
        }))?;
        assert_eq!(
            merge_base.scope,
            DiffScope::MergeBase {
                reference: "origin/develop".to_owned()
            }
        );
        Ok(())
    }

    #[test]
    fn unsupported_is_a_typed_error() {
        let error = QueryError::Unsupported("search");
        assert!(error.to_string().contains("search"));
    }

    #[test]
    fn freshness_violation_is_a_typed_error() {
        let error = QueryError::FreshnessNotMet {
            required: FreshnessRequirement::RequireFresh,
            actual: Freshness::Stale,
        };
        let message = error.to_string();
        assert!(message.contains("RequireFresh"));
        assert!(message.contains("Stale"));
    }
}
