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
    DiffDocument, DiffWorkspace, WorkspaceDiff, WorkspaceDiffError, WorkspaceDiffProvider,
    WorkspaceFileChange,
};
pub use engine::{
    BarrierAlreadyInstalled, DiffProviderAlreadyInstalled, FreshnessBarrier, FreshnessBarrierError,
    ProviderAlreadyInstalled, PublishError, UpdateBuilder, WorkspaceEngine, WorkspaceSnapshot,
};
pub use graph::{
    BoundedGraphBuilder, CallSiteInput, ConsistencyAudit, ConsistencyError, GraphBuildLimits,
    GraphBuildReport, GraphError, SymbolGraph,
};
pub use precise::{
    CallHierarchyDirections, PreciseProvider, PreciseQueryRequest, PreciseQueryResult,
    PreciseRelation, ProviderDocument, ProviderSymbol, ProviderWorkspace, ProviderWorkspaceDelta,
};
