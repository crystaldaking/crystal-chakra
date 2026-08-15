//! Atomic published-revision ownership (SPEC §5, §35; ADR-0001).
//!
//! [`WorkspaceEngine`] is the single owner of the published revision.
//! Updates are constructed privately in an [`UpdateBuilder`] and published
//! with a compare-and-publish step: a builder whose base revision no longer
//! matches is rejected with [`PublishError::Conflict`], so a slow update can
//! never overwrite a newer revision.

use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwap;
use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::revision::Revision;
use chakra_domain::state::{Freshness, ProviderState, WorkspaceStatus};
use thiserror::Error;

use crate::graph::SymbolGraph;
use crate::precise::{PreciseProvider, ProviderWorkspace};

/// One immutable, atomically published workspace state.
#[derive(Debug)]
pub struct WorkspaceSnapshot {
    identity: WorkspaceIdentity,
    revision: Revision,
    status: WorkspaceStatus,
    /// Independent from `status`: only a publisher that completed
    /// reconciliation against the filesystem may claim `Fresh` (SPEC §6).
    freshness: Freshness,
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

    pub fn freshness(&self) -> Freshness {
        self.freshness
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

/// Failure to prove that the published syntax state reflects the current
/// materialized worktree.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("freshness reconciliation failed: {message}")]
pub struct FreshnessBarrierError {
    message: String,
}

impl FreshnessBarrierError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Language-neutral synchronization point used by `RequireFresh` queries.
///
/// Implementations reconcile their canonical sources and atomically publish
/// the completed result before returning. The engine deliberately knows
/// nothing about filesystem watcher or language-provider types.
pub trait FreshnessBarrier: std::fmt::Debug + Send + Sync {
    fn require_fresh(&self) -> Result<(), FreshnessBarrierError>;
}

/// A workspace has exactly one owner for freshness reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a freshness barrier is already installed")]
pub struct BarrierAlreadyInstalled;

/// A workspace has at most one optional precise-provider adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a precise provider is already installed")]
pub struct ProviderAlreadyInstalled;

/// A private, in-progress update to the workspace state.
///
/// Obtained from [`WorkspaceEngine::begin_update`]; published via
/// [`WorkspaceEngine::publish`].
#[derive(Debug)]
pub struct UpdateBuilder {
    base_revision: Revision,
    identity: WorkspaceIdentity,
    status: WorkspaceStatus,
    freshness: Freshness,
    provider_state: ProviderState,
    graph: SymbolGraph,
}

impl UpdateBuilder {
    /// Revision this update is based on.
    pub fn base_revision(&self) -> Revision {
        self.base_revision
    }

    /// Mutable access for incremental edits relative to the base snapshot.
    ///
    /// Mutating the graph changes the published relationship to the
    /// filesystem, so this revokes any inherited or previously claimed
    /// freshness: the publisher must re-confirm reconciliation and call
    /// [`UpdateBuilder::set_freshness`] *after* its graph edits.
    pub fn graph_mut(&mut self) -> &mut SymbolGraph {
        self.freshness = Freshness::Stale;
        &mut self.graph
    }

    /// Replaces the whole graph (used by the initial full index).
    ///
    /// Like [`UpdateBuilder::graph_mut`], this revokes freshness: a replaced
    /// graph is only `Fresh` once the publisher explicitly says the
    /// worktree was reconciled.
    pub fn replace_graph(&mut self, graph: SymbolGraph) {
        self.freshness = Freshness::Stale;
        self.graph = graph;
    }

    pub fn set_status(&mut self, status: WorkspaceStatus) {
        self.status = status;
    }

    /// Claims (or revokes) filesystem freshness for the published snapshot.
    ///
    /// Freshness is inherited from the base snapshot only while the graph is
    /// untouched: [`UpdateBuilder::graph_mut`] and
    /// [`UpdateBuilder::replace_graph`] revoke it, so a publisher that
    /// changed the graph must call this *after* its edits, once
    /// reconciliation against the worktree is confirmed. `Ready` alone never
    /// implies `Fresh`.
    pub fn set_freshness(&mut self, freshness: Freshness) {
        self.freshness = freshness;
    }

    pub fn set_provider_state(&mut self, provider_state: ProviderState) {
        self.provider_state = provider_state;
    }
}

/// Owns and atomically publishes workspace revisions.
#[derive(Debug)]
pub struct WorkspaceEngine {
    current: ArcSwap<WorkspaceSnapshot>,
    freshness_barrier: OnceLock<Arc<dyn FreshnessBarrier>>,
    precise_provider: OnceLock<Arc<dyn PreciseProvider>>,
}

impl WorkspaceEngine {
    /// A fresh engine: revision 0, empty graph, no language provider.
    ///
    /// Status starts at `Initializing` and freshness at `Stale`: the engine
    /// has not yet observed or indexed the filesystem, so nothing may report
    /// ready/fresh data. The first real reconciliation/index publish sets
    /// both explicitly.
    pub fn new(identity: WorkspaceIdentity) -> Self {
        let snapshot = WorkspaceSnapshot {
            identity,
            revision: Revision::INITIAL,
            status: WorkspaceStatus::Initializing,
            freshness: Freshness::Stale,
            provider_state: ProviderState::NotConfigured,
            graph: SymbolGraph::new(),
        };
        Self {
            current: ArcSwap::from_pointee(snapshot),
            freshness_barrier: OnceLock::new(),
            precise_provider: OnceLock::new(),
        }
    }

    /// Installs the single reconciliation owner for this workspace.
    pub fn install_freshness_barrier(
        &self,
        barrier: Arc<dyn FreshnessBarrier>,
    ) -> Result<(), BarrierAlreadyInstalled> {
        self.freshness_barrier
            .set(barrier)
            .map_err(|_| BarrierAlreadyInstalled)
    }

    /// Installs the optional, language-neutral precise-provider adapter.
    pub fn install_precise_provider(
        &self,
        provider: Arc<dyn PreciseProvider>,
    ) -> Result<(), ProviderAlreadyInstalled> {
        self.precise_provider
            .set(provider)
            .map_err(|_| ProviderAlreadyInstalled)
    }

    /// Captures the current immutable syntax input for provider startup.
    pub fn provider_workspace(&self) -> ProviderWorkspace {
        ProviderWorkspace::from_snapshot(&self.snapshot())
    }

    pub(crate) fn precise_provider(&self) -> Option<&Arc<dyn PreciseProvider>> {
        self.precise_provider.get()
    }

    /// Waits until the installed owner has reconciled and published the
    /// latest syntax state. Static engines without a live owner retain their
    /// already-published freshness semantics.
    pub fn require_fresh(&self) -> Result<(), FreshnessBarrierError> {
        self.freshness_barrier
            .get()
            .map_or(Ok(()), |barrier| barrier.require_fresh())
    }

    /// The currently published revision, as a consistent immutable view.
    pub fn snapshot(&self) -> Arc<WorkspaceSnapshot> {
        self.current.load_full()
    }

    /// Starts a private update based on the current revision.
    ///
    /// Status, freshness, and provider state are inherited from the base
    /// snapshot. Freshness inheritance ends the moment the graph is touched:
    /// [`UpdateBuilder::graph_mut`] and [`UpdateBuilder::replace_graph`]
    /// revoke it, and the publisher must claim `Fresh` explicitly once
    /// reconciliation is confirmed.
    pub fn begin_update(&self) -> UpdateBuilder {
        let base = self.snapshot();
        UpdateBuilder {
            base_revision: base.revision,
            identity: base.identity.clone(),
            status: base.status,
            freshness: base.freshness,
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
            freshness: update.freshness,
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
    fn new_engine_starts_unindexed() -> Result<(), Box<dyn std::error::Error>> {
        let engine = engine()?;
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.revision(), Revision::INITIAL);
        assert_eq!(snapshot.graph().symbol_count(), 0);
        // Not Ready and not fresh: nothing has observed the filesystem yet.
        assert_eq!(snapshot.status(), WorkspaceStatus::Initializing);
        assert_eq!(snapshot.freshness(), Freshness::Stale);
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
    fn freshness_is_an_explicit_publication_claim() -> Result<(), Box<dyn std::error::Error>> {
        let engine = engine()?;
        // Inherited unchanged when the update says nothing.
        let published = engine.publish(engine.begin_update())?;
        assert_eq!(published.freshness(), Freshness::Stale);

        // A reconciling publisher claims Fresh explicitly.
        let mut update = engine.begin_update();
        update.set_status(WorkspaceStatus::Ready);
        update.set_freshness(Freshness::Fresh);
        let published = engine.publish(update)?;
        assert_eq!(published.status(), WorkspaceStatus::Ready);
        assert_eq!(published.freshness(), Freshness::Fresh);

        // A filesystem change can revoke freshness without touching status.
        let mut update = engine.begin_update();
        update.set_freshness(Freshness::Stale);
        let published = engine.publish(update)?;
        assert_eq!(published.status(), WorkspaceStatus::Ready);
        assert_eq!(published.freshness(), Freshness::Stale);
        Ok(())
    }

    #[test]
    fn graph_mutation_revokes_inherited_freshness() -> Result<(), Box<dyn std::error::Error>> {
        let engine = engine()?;
        let mut update = engine.begin_update();
        update.set_status(WorkspaceStatus::Ready);
        update.set_freshness(Freshness::Fresh);
        engine.publish(update)?;

        // Replacing the graph silently keeping `Fresh` would publish an
        // unverified freshness claim — mutation revokes it instead.
        let mut update = engine.begin_update();
        update.replace_graph(SymbolGraph::new());
        let published = engine.publish(update)?;
        assert_eq!(published.freshness(), Freshness::Stale);

        // Same for incremental mutation, even if `Fresh` was claimed before
        // the edit: the mutation has the last word.
        let mut update = engine.begin_update();
        update.set_freshness(Freshness::Fresh);
        assert_eq!(update.graph_mut().symbol_count(), 0);
        let published = engine.publish(update)?;
        assert_eq!(published.freshness(), Freshness::Stale);

        // The reconciling publisher claims freshness after its graph edits.
        let mut update = engine.begin_update();
        update.replace_graph(SymbolGraph::new());
        update.set_freshness(Freshness::Fresh);
        let published = engine.publish(update)?;
        assert_eq!(published.freshness(), Freshness::Fresh);

        // Metadata-only updates leave the freshness axis alone.
        let mut update = engine.begin_update();
        update.set_status(WorkspaceStatus::Degraded);
        let published = engine.publish(update)?;
        assert_eq!(published.status(), WorkspaceStatus::Degraded);
        assert_eq!(published.freshness(), Freshness::Fresh);
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
