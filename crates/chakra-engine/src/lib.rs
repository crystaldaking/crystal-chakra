//! In-memory workspace engine.
//!
//! Owns the published workspace state: immutable [`WorkspaceSnapshot`]
//! values are built privately and published atomically (SPEC §5, §35).
//! Implements the [`chakra_domain::query::QueryService`] contract against
//! the in-memory symbol graph.

mod diff;
mod engine;
mod graph;
mod precise;
mod query;

pub use diff::{
    DiffDocument, DiffInventoryTruncation, DiffWorkspace, WorkspaceDiff, WorkspaceDiffError,
    WorkspaceDiffProvider, WorkspaceFileChange,
};
pub use engine::{
    BarrierAlreadyInstalled, DiagnosticsAlreadyInstalled, DiffProviderAlreadyInstalled,
    FreshnessBarrier, FreshnessBarrierError, IndexDiagnosticsSource, ProviderInstallError,
    PublishError, UpdateBuilder, WorkspaceEngine, WorkspaceSnapshot,
};
pub use graph::{
    BoundedGraphBuilder, CallSiteInput, ConsistencyAudit, ConsistencyError, GraphBuildLimits,
    GraphBuildReport, GraphDiagnosticSummary, GraphError, GraphFileSummary, SymbolGraph,
};
pub use precise::{
    CallHierarchyDirections, PreciseProvider, PreciseQueryRequest, PreciseQueryResult,
    PreciseRelation, ProviderDocument, ProviderInput, ProviderRequestPriority,
    ProviderShutdownError, ProviderSymbol, ProviderWorkspace, ProviderWorkspaceDelta,
};
