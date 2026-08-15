//! Atomic published-revision ownership (SPEC §5, §35; ADR-0001).
//!
//! [`WorkspaceEngine`] is the single owner of the published revision.
//! Updates are constructed privately in an [`UpdateBuilder`] and published
//! with a compare-and-publish step: a builder whose base revision no longer
//! matches is rejected with [`PublishError::Conflict`], so a slow update can
//! never overwrite a newer revision.

use std::sync::Arc;

use arc_swap::ArcSwap;
use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::revision::Revision;
use chakra_domain::state::{ProviderState, WorkspaceStatus};
use thiserror::Error;

use crate::graph::SymbolGraph;

/// One immutable, atomically published workspace state.
#[derive(Debug)]
pub struct WorkspaceSnapshot {
    identity: WorkspaceIdentity,
    revision: Revision,
    status: WorkspaceStatus,
    provider_state: ProviderState,
    graph: SymbolGraph,
}

impl WorkspaceSnapshot {
    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn status(&self) -> WorkspaceStatus {
        self.status
    }

    pub fn provider_state(&self) -> ProviderState {
        self.provider_state
    }

    pub fn graph(&self) -> &SymbolGraph {
        &self.graph
    }
}

/// Why an update was not published.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PublishError {
    #[error("update is based on revision {base}, but the published revision is {current}")]
    Conflict { base: Revision, current: Revision },
}

/// A private, in-progress update to the workspace state.
///
/// Obtained from [`WorkspaceEngine::begin_update`]; published via
/// [`WorkspaceEngine::publish`].
#[derive(Debug)]
pub struct UpdateBuilder {
    base_revision: Revision,
    identity: WorkspaceIdentity,
    status: WorkspaceStatus,
    provider_state: ProviderState,
    graph: SymbolGraph,
}

impl UpdateBuilder {
    /// Revision this update is based on.
    pub fn base_revision(&self) -> Revision {
        self.base_revision
    }

    /// Mutable access for incremental edits relative to the base snapshot.
    pub fn graph_mut(&mut self) -> &mut SymbolGraph {
        &mut self.graph
    }

    /// Replaces the whole graph (used by the initial full index).
    pub fn replace_graph(&mut self, graph: SymbolGraph) {
        self.graph = graph;
    }

    pub fn set_status(&mut self, status: WorkspaceStatus) {
        self.status = status;
    }

    pub fn set_provider_state(&mut self, provider_state: ProviderState) {
        self.provider_state = provider_state;
    }
}

/// Owns and atomically publishes workspace revisions.
#[derive(Debug)]
pub struct WorkspaceEngine {
    current: ArcSwap<WorkspaceSnapshot>,
}

impl WorkspaceEngine {
    /// A fresh engine: revision 0, empty graph, no language provider.
    pub fn new(identity: WorkspaceIdentity) -> Self {
        let snapshot = WorkspaceSnapshot {
            identity,
            revision: Revision::INITIAL,
            status: WorkspaceStatus::Ready,
            provider_state: ProviderState::NotConfigured,
            graph: SymbolGraph::new(),
        };
        Self {
            current: ArcSwap::from_pointee(snapshot),
        }
    }

    /// The currently published revision, as a consistent immutable view.
    pub fn snapshot(&self) -> Arc<WorkspaceSnapshot> {
        self.current.load_full()
    }

    /// Starts a private update based on the current revision.
    pub fn begin_update(&self) -> UpdateBuilder {
        let base = self.snapshot();
        UpdateBuilder {
            base_revision: base.revision,
            identity: base.identity.clone(),
            status: base.status,
            provider_state: base.provider_state,
            graph: base.graph.clone(),
        }
    }

    /// Atomically publishes the update as the next revision.
    ///
    /// Fails with [`PublishError::Conflict`] if another update was published
    /// since `update` was started; the loser can rebase and retry.
    pub fn publish(&self, update: UpdateBuilder) -> Result<Arc<WorkspaceSnapshot>, PublishError> {
        let current = self.current.load_full();
        if current.revision != update.base_revision {
            return Err(PublishError::Conflict {
                base: update.base_revision,
                current: current.revision,
            });
        }
        let next = Arc::new(WorkspaceSnapshot {
            identity: update.identity,
            revision: update.base_revision.next(),
            status: update.status,
            provider_state: update.provider_state,
            graph: update.graph,
        });
        // Compare-and-publish: only swap if the slot still holds the
        // revision we validated. A concurrent winner makes this fail and the
        // loser reports a conflict instead of clobbering newer state.
        let previous = self.current.compare_and_swap(&current, next.clone());
        if !Arc::ptr_eq(&previous, &current) {
            return Err(PublishError::Conflict {
                base: update.base_revision,
                current: previous.revision,
            });
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Result<WorkspaceEngine, Box<dyn std::error::Error>> {
        let identity = WorkspaceIdentity::for_primary_worktree(std::path::Path::new("."))?;
        Ok(WorkspaceEngine::new(identity))
    }

    #[test]
    fn new_engine_starts_at_revision_zero() -> Result<(), Box<dyn std::error::Error>> {
        let engine = engine()?;
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.revision(), Revision::INITIAL);
        assert_eq!(snapshot.graph().symbol_count(), 0);
        assert_eq!(snapshot.status(), WorkspaceStatus::Ready);
        assert_eq!(snapshot.provider_state(), ProviderState::NotConfigured);
        Ok(())
    }

    #[test]
    fn publish_increments_revision() -> Result<(), Box<dyn std::error::Error>> {
        let engine = engine()?;
        let published = engine.publish(engine.begin_update())?;
        assert_eq!(published.revision(), Revision(1));
        assert_eq!(engine.snapshot().revision(), Revision(1));
        Ok(())
    }

    #[test]
    fn stale_update_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let engine = engine()?;
        let loser = engine.begin_update();
        engine.publish(engine.begin_update())?;
        let result = engine.publish(loser);
        assert!(matches!(result, Err(PublishError::Conflict { .. })));
        Ok(())
    }

    #[test]
    fn held_snapshot_is_immutable_after_publish() -> Result<(), Box<dyn std::error::Error>> {
        let engine = engine()?;
        let before = engine.snapshot();
        engine.publish(engine.begin_update())?;
        // The previously obtained Arc still observes the complete old state.
        assert_eq!(before.revision(), Revision::INITIAL);
        assert_eq!(before.graph().symbol_count(), 0);
        assert_eq!(engine.snapshot().revision(), Revision(1));
        Ok(())
    }
}
