//! In-memory workspace engine.
//!
//! Owns the published workspace state: immutable [`WorkspaceSnapshot`]
//! values are built privately and published atomically (SPEC §5, §35).
//! Implements the [`chakra_domain::query::QueryService`] contract against
//! the in-memory symbol graph.

mod engine;
mod graph;
mod query;

pub use engine::{PublishError, UpdateBuilder, WorkspaceEngine, WorkspaceSnapshot};
pub use graph::{ConsistencyError, GraphError, SymbolGraph};
