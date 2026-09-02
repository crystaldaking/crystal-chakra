//! Bounded ownership of materialized worktree runtimes (SPEC §§11, 17, 35–36).
//!
//! A [`WorkspaceRegistry`] is scoped to one Git repository identity and owns
//! one completely independent [`WorkspaceEngine`] plus live index per
//! registered materialized worktree. The registry never combines mutable
//! worktree state: callers select one engine by [`WorkspaceId`] and every
//! query continues to pin exactly one atomically published workspace
//! revision.

mod snapshot_cache;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use chakra_domain::identity::{RepositoryId, WorkspaceId, WorkspaceIdentity};
use chakra_domain::operation::OperationContext;
use chakra_domain::query::{DiffScope, QueryError, QueryService, WorkspaceQueryRouter};
use chakra_domain::revision::Revision;
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_engine::{
    DiffWorkspace, PublishError, WorkspaceDiffProvider, WorkspaceEngine, source_layers_for_graph,
    workspace_graph_layers,
};
use chakra_language::{
    IndexMetrics, IndexOptions, LiveIndex, LiveIndexError, LiveIndexOptions, WorkspaceIndexError,
    start_live_index_with_options_and_commit_provider,
};
use thiserror::Error;

pub use snapshot_cache::{
    CommitSnapshotCache, CommitSnapshotCacheConfig, CommitSnapshotCacheError,
};

const MAX_STALE_PUBLICATION_ATTEMPTS: usize = 3;
const MAX_LAYER_COMPOSITION_ATTEMPTS: usize = 3;

/// Hard process-local bounds for one repository registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceRegistryConfig {
    /// Maximum ready or transitioning worktrees owned by the registry.
    pub max_workspaces: usize,
}

impl Default for WorkspaceRegistryConfig {
    fn default() -> Self {
        Self { max_workspaces: 8 }
    }
}

impl WorkspaceRegistryConfig {
    pub fn validate(self) -> Result<Self, WorkspaceRegistryError> {
        if self.max_workspaces == 0 {
            return Err(WorkspaceRegistryError::InvalidMaxWorkspaces);
        }
        Ok(self)
    }
}

/// Per-worktree cold-index and live-watcher settings.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceStartOptions {
    pub index: IndexOptions,
    pub live: LiveIndexOptions,
}

/// Non-owning registration result. The registry retains watcher ownership;
/// keeping this value alive cannot keep a removed worktree fresh.
#[derive(Debug, Clone)]
pub struct RegisteredWorkspace {
    identity: WorkspaceIdentity,
    engine: Arc<WorkspaceEngine>,
    initial_metrics: IndexMetrics,
}

impl RegisteredWorkspace {
    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }

    pub fn engine(&self) -> Arc<WorkspaceEngine> {
        self.engine.clone()
    }

    pub fn initial_metrics(&self) -> &IndexMetrics {
        &self.initial_metrics
    }
}

#[derive(Debug, Error)]
pub enum WorkspaceRegistryError {
    #[error("workspace registry max_workspaces must be greater than zero")]
    InvalidMaxWorkspaces,
    #[error("workspace registry is shut down")]
    ShutDown,
    #[error(
        "repository {actual} cannot be registered in a registry scoped to repository {expected}"
    )]
    RepositoryMismatch {
        expected: RepositoryId,
        actual: RepositoryId,
    },
    #[error("workspace {workspace} is already registered")]
    AlreadyRegistered { workspace: WorkspaceId },
    #[error("workspace {workspace} is still starting")]
    RegistrationInProgress { workspace: WorkspaceId },
    #[error("workspace {workspace} is still stopping")]
    ShutdownInProgress { workspace: WorkspaceId },
    #[error("workspace registry reached its {limit}-worktree limit")]
    CapacityReached { limit: usize },
    #[error("workspace is not registered: {workspace}")]
    NotRegistered { workspace: WorkspaceId },
    #[error(
        "cannot shut down while workspace transitions are active (starting: {starting}, stopping: {stopping})"
    )]
    WorkspaceTransitionsInProgress { starting: usize, stopping: usize },
    #[error(transparent)]
    Git(#[from] chakra_git::DiscoveryError),
    #[error(transparent)]
    Index(#[from] WorkspaceIndexError),
    #[error(transparent)]
    SnapshotCache(#[from] CommitSnapshotCacheError),
    #[error("failed to install the worktree Git diff adapter")]
    DiffProviderInstall,
    #[error("failed to compose commit snapshot and worktree overlay: {0}")]
    LayerComposition(String),
    #[error("failed to publish the initial worktree revision: {0}")]
    InitialPublish(#[source] PublishError),
    #[error(transparent)]
    LiveIndex(#[from] LiveIndexError),
    #[error("failed to mark stopped workspace {workspace} stale: {source}")]
    StalePublication {
        workspace: WorkspaceId,
        #[source]
        source: PublishError,
    },
    #[error("one or more workspace shutdowns failed: {failures:?}")]
    ShutdownFailures { failures: Vec<String> },
}

#[derive(Debug)]
enum RegistryEntry {
    Starting,
    Ready(Box<WorkspaceRuntime>),
    Stopping,
}

#[derive(Debug, Default)]
struct RegistryState {
    repository: Option<RepositoryId>,
    entries: HashMap<WorkspaceId, RegistryEntry>,
    shut_down: bool,
}

/// Process-local owner for all materialized worktrees of one Git repository.
#[derive(Debug)]
pub struct WorkspaceRegistry {
    config: WorkspaceRegistryConfig,
    commit_snapshots: Arc<CommitSnapshotCache>,
    state: Mutex<RegistryState>,
}

impl WorkspaceRegistry {
    pub fn new(config: WorkspaceRegistryConfig) -> Result<Self, WorkspaceRegistryError> {
        Self::new_with_snapshot_cache(config, CommitSnapshotCacheConfig::default())
    }

    /// Creates a registry with explicit process-local/disk snapshot bounds.
    /// Disk reuse remains opt-in until issue #50's benchmark decision.
    pub fn new_with_snapshot_cache(
        config: WorkspaceRegistryConfig,
        snapshot_cache: CommitSnapshotCacheConfig,
    ) -> Result<Self, WorkspaceRegistryError> {
        Ok(Self {
            config: config.validate()?,
            commit_snapshots: Arc::new(CommitSnapshotCache::new(snapshot_cache)?),
            state: Mutex::new(RegistryState::default()),
        })
    }

    /// Starts and registers one materialized Git worktree.
    ///
    /// Identity resolution and all indexing work happen outside the registry
    /// lock. A bounded `Starting` reservation prevents duplicate owners and
    /// counts against `max_workspaces` until startup either succeeds or rolls
    /// back.
    pub fn register(
        &self,
        root: &Path,
        options: WorkspaceStartOptions,
    ) -> Result<RegisteredWorkspace, WorkspaceRegistryError> {
        let identity = chakra_git::resolve_workspace_identity(root)?;
        self.reserve(identity.clone())?;

        let started =
            WorkspaceRuntime::start(identity.clone(), options, self.commit_snapshots.clone());
        match started {
            Ok(runtime) => self.finish_registration(runtime),
            Err(error) => {
                self.rollback_reservation(&identity.workspace);
                Err(error)
            }
        }
    }

    /// Returns the independent query engine for one ready worktree.
    pub fn workspace(
        &self,
        workspace: &WorkspaceId,
    ) -> Result<Arc<WorkspaceEngine>, WorkspaceRegistryError> {
        let state = lock_state(&self.state);
        match state.entries.get(workspace) {
            Some(RegistryEntry::Ready(runtime)) => Ok(runtime.engine.clone()),
            Some(RegistryEntry::Starting) => Err(WorkspaceRegistryError::RegistrationInProgress {
                workspace: workspace.clone(),
            }),
            Some(RegistryEntry::Stopping) => Err(WorkspaceRegistryError::ShutdownInProgress {
                workspace: workspace.clone(),
            }),
            None => Err(WorkspaceRegistryError::NotRegistered {
                workspace: workspace.clone(),
            }),
        }
    }

    /// Stable identity snapshot of every ready worktree.
    pub fn workspaces(&self) -> Vec<WorkspaceIdentity> {
        let state = lock_state(&self.state);
        let mut identities: Vec<_> = state
            .entries
            .values()
            .filter_map(|entry| match entry {
                RegistryEntry::Ready(runtime) => Some(runtime.identity.clone()),
                RegistryEntry::Starting | RegistryEntry::Stopping => None,
            })
            .collect();
        identities.sort_by(|left, right| left.workspace.as_str().cmp(right.workspace.as_str()));
        identities
    }

    /// Stops and removes one worktree owner. Any previously cloned engine is
    /// atomically marked stale after its watcher joins, so it cannot continue
    /// claiming current filesystem facts after removal.
    pub fn unregister(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceRegistryError> {
        let runtime = {
            let mut state = lock_state(&self.state);
            match state.entries.get(workspace) {
                Some(RegistryEntry::Starting) => {
                    return Err(WorkspaceRegistryError::RegistrationInProgress {
                        workspace: workspace.clone(),
                    });
                }
                Some(RegistryEntry::Stopping) => {
                    return Err(WorkspaceRegistryError::ShutdownInProgress {
                        workspace: workspace.clone(),
                    });
                }
                Some(RegistryEntry::Ready(_)) => {}
                None => {
                    return Err(WorkspaceRegistryError::NotRegistered {
                        workspace: workspace.clone(),
                    });
                }
            }
            match state
                .entries
                .insert(workspace.clone(), RegistryEntry::Stopping)
            {
                Some(RegistryEntry::Ready(runtime)) => runtime,
                Some(RegistryEntry::Starting | RegistryEntry::Stopping) | None => {
                    return Err(WorkspaceRegistryError::NotRegistered {
                        workspace: workspace.clone(),
                    });
                }
            }
        };
        let result = runtime.shutdown();
        let mut state = lock_state(&self.state);
        if matches!(state.entries.get(workspace), Some(RegistryEntry::Stopping)) {
            state.entries.remove(workspace);
        }
        result
    }

    /// Stops every ready worktree and permanently closes this registry.
    pub fn shutdown(&self) -> Result<(), WorkspaceRegistryError> {
        let runtimes = {
            let mut state = lock_state(&self.state);
            if state.shut_down {
                return Ok(());
            }
            let starting = state
                .entries
                .values()
                .filter(|entry| matches!(entry, RegistryEntry::Starting))
                .count();
            let stopping = state
                .entries
                .values()
                .filter(|entry| matches!(entry, RegistryEntry::Stopping))
                .count();
            if starting > 0 || stopping > 0 {
                return Err(WorkspaceRegistryError::WorkspaceTransitionsInProgress {
                    starting,
                    stopping,
                });
            }
            state.shut_down = true;
            state
                .entries
                .drain()
                .filter_map(|(_, entry)| match entry {
                    RegistryEntry::Ready(runtime) => Some(runtime),
                    RegistryEntry::Starting | RegistryEntry::Stopping => None,
                })
                .collect::<Vec<_>>()
        };

        let mut failures = Vec::new();
        for runtime in runtimes {
            if let Err(error) = runtime.shutdown() {
                failures.push(error.to_string());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(WorkspaceRegistryError::ShutdownFailures { failures })
        }
    }

    fn reserve(&self, identity: WorkspaceIdentity) -> Result<(), WorkspaceRegistryError> {
        let mut state = lock_state(&self.state);
        if state.shut_down {
            return Err(WorkspaceRegistryError::ShutDown);
        }
        if let Some(repository) = &state.repository
            && repository != &identity.repository
        {
            return Err(WorkspaceRegistryError::RepositoryMismatch {
                expected: repository.clone(),
                actual: identity.repository,
            });
        }
        if state.entries.contains_key(&identity.workspace) {
            return Err(WorkspaceRegistryError::AlreadyRegistered {
                workspace: identity.workspace,
            });
        }
        if state.entries.len() >= self.config.max_workspaces {
            return Err(WorkspaceRegistryError::CapacityReached {
                limit: self.config.max_workspaces,
            });
        }
        if state.repository.is_none() {
            state.repository = Some(identity.repository.clone());
        }
        state
            .entries
            .insert(identity.workspace.clone(), RegistryEntry::Starting);
        Ok(())
    }

    fn finish_registration(
        &self,
        runtime: WorkspaceRuntime,
    ) -> Result<RegisteredWorkspace, WorkspaceRegistryError> {
        let workspace = runtime.identity.workspace.clone();
        let registered = runtime.registered();
        let mut rejected = None;
        {
            let mut state = lock_state(&self.state);
            if state.shut_down {
                state.entries.remove(&workspace);
                rejected = Some(runtime);
            } else {
                state
                    .entries
                    .insert(workspace, RegistryEntry::Ready(Box::new(runtime)));
            }
        }
        if let Some(runtime) = rejected {
            if let Err(error) = runtime.shutdown() {
                tracing::error!(%error, "failed to stop workspace rejected by registry shutdown");
            }
            Err(WorkspaceRegistryError::ShutDown)
        } else {
            Ok(registered)
        }
    }

    fn rollback_reservation(&self, workspace: &WorkspaceId) {
        let mut state = lock_state(&self.state);
        state.entries.remove(workspace);
        if state.entries.is_empty() {
            state.repository = None;
        }
    }
}

impl WorkspaceQueryRouter for WorkspaceRegistry {
    fn workspaces(&self) -> Result<Vec<WorkspaceIdentity>, QueryError> {
        Ok(WorkspaceRegistry::workspaces(self))
    }

    fn route(&self, requested: Option<&WorkspaceId>) -> Result<Arc<dyn QueryService>, QueryError> {
        let state = lock_state(&self.state);
        if let Some(workspace) = requested {
            return match state.entries.get(workspace) {
                Some(RegistryEntry::Ready(runtime)) => {
                    let service: Arc<dyn QueryService> = runtime.engine.clone();
                    Ok(service)
                }
                Some(RegistryEntry::Starting | RegistryEntry::Stopping) | None => {
                    Err(QueryError::WorkspaceNotFound(workspace.clone()))
                }
            };
        }

        let mut ready = state.entries.values().filter_map(|entry| match entry {
            RegistryEntry::Ready(runtime) => Some(runtime),
            RegistryEntry::Starting | RegistryEntry::Stopping => None,
        });
        let first = ready.next().ok_or(QueryError::NoWorkspacesRegistered)?;
        if ready.next().is_none() {
            let service: Arc<dyn QueryService> = first.engine.clone();
            return Ok(service);
        }
        let mut available: Vec<_> = state
            .entries
            .values()
            .filter_map(|entry| match entry {
                RegistryEntry::Ready(runtime) => Some(runtime.identity.workspace.clone()),
                RegistryEntry::Starting | RegistryEntry::Stopping => None,
            })
            .collect();
        available.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        Err(QueryError::WorkspaceSelectionRequired { available })
    }
}

impl Drop for WorkspaceRegistry {
    fn drop(&mut self) {
        let state = match self.state.get_mut() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        for (_, entry) in state.entries.drain() {
            if let RegistryEntry::Ready(runtime) = entry
                && let Err(error) = runtime.shutdown()
            {
                tracing::error!(%error, "failed to stop workspace while dropping registry");
            }
        }
    }
}

#[derive(Debug)]
struct WorkspaceRuntime {
    identity: WorkspaceIdentity,
    engine: Arc<WorkspaceEngine>,
    initial_metrics: IndexMetrics,
    live: Option<LiveIndex>,
    freshness_revoked: bool,
}

impl WorkspaceRuntime {
    fn start(
        identity: WorkspaceIdentity,
        options: WorkspaceStartOptions,
        commit_snapshots: Arc<CommitSnapshotCache>,
    ) -> Result<Self, WorkspaceRegistryError> {
        let mut last_instability = None;
        for _ in 0..MAX_LAYER_COMPOSITION_ATTEMPTS {
            match Self::start_once(identity.clone(), options.clone(), commit_snapshots.clone()) {
                Ok(runtime) => return Ok(runtime),
                Err(WorkspaceRegistryError::LayerComposition(message)) => {
                    last_instability = Some(message);
                }
                Err(error) => return Err(error),
            }
        }
        Err(WorkspaceRegistryError::LayerComposition(
            last_instability
                .unwrap_or_else(|| "worktree kept changing during startup composition".to_owned()),
        ))
    }

    fn start_once(
        identity: WorkspaceIdentity,
        options: WorkspaceStartOptions,
        commit_snapshots: Arc<CommitSnapshotCache>,
    ) -> Result<Self, WorkspaceRegistryError> {
        let operation = OperationContext::from_cancellation(options.index.cancellation.clone());
        let commit = chakra_git::resolve_head_commit_with_context(&identity.root, &operation)?;
        let layered = commit_snapshots
            .load_or_build(
                &identity.root,
                &identity.repository,
                commit.as_deref(),
                options.index.clone(),
            )?
            .compose_worktree(options.index)?;
        let mut report = layered.effective;
        let operation = OperationContext::unbounded();
        let diff_workspace = DiffWorkspace::from_graph_with_context(
            identity.root.clone(),
            Revision::INITIAL,
            DiffScope::Worktree,
            &report.graph,
            &operation,
        )
        .map_err(|error| WorkspaceRegistryError::LayerComposition(error.to_string()))?;
        let diff = chakra_git::GitWorkspaceDiff
            .diff_with_context(diff_workspace, &operation)
            .map_err(|error| WorkspaceRegistryError::LayerComposition(error.to_string()))?;
        if diff.scope.base_commit != layered.commit.commit {
            return Err(WorkspaceRegistryError::LayerComposition(
                "HEAD changed while the commit snapshot was being composed".to_owned(),
            ));
        }
        let mut layers = workspace_graph_layers(
            layered.commit.source_files,
            layered.commit.source_bytes,
            &diff,
        );
        layers.commit_snapshot.reuse = layered.commit.reuse.clone();
        let source_layers = source_layers_for_graph(&layered.commit.graph, &report.graph);
        report.graph = report.graph.with_source_layers(source_layers);
        let engine = Arc::new(WorkspaceEngine::new(identity.clone()));
        let diff_adapter: Arc<dyn WorkspaceDiffProvider> = Arc::new(chakra_git::GitWorkspaceDiff);
        engine
            .install_diff_provider(diff_adapter)
            .map_err(|_| WorkspaceRegistryError::DiffProviderInstall)?;

        let mut update = engine.begin_update();
        update.set_provider_inputs(report.provider_inputs.clone());
        update.set_project_model(report.project_model.clone());
        update.replace_graph(report.graph);
        update.set_graph_layers(layered.commit.graph, layers);
        update.set_indexing(report.metrics.indexing.clone());
        update.set_status(WorkspaceStatus::Indexing);
        // The watcher is not active yet. Its mandatory startup reconciliation
        // is the first operation allowed to claim filesystem freshness.
        update.set_freshness(Freshness::Stale);
        engine
            .publish(update)
            .map_err(WorkspaceRegistryError::InitialPublish)?;

        let live = start_live_index_with_options_and_commit_provider(
            report.repository_root,
            report.syntax_index,
            engine.clone(),
            options.live,
            Some(commit_snapshots),
        )?;
        Ok(Self {
            identity,
            engine,
            initial_metrics: report.metrics,
            live: Some(live),
            freshness_revoked: false,
        })
    }

    fn registered(&self) -> RegisteredWorkspace {
        RegisteredWorkspace {
            identity: self.identity.clone(),
            engine: self.engine.clone(),
            initial_metrics: self.initial_metrics.clone(),
        }
    }

    fn shutdown(mut self) -> Result<(), WorkspaceRegistryError> {
        let live_result = match self.live.take() {
            Some(live) => live.shutdown().map_err(WorkspaceRegistryError::from),
            None => Ok(()),
        };
        let stale_result = mark_engine_stale(&self.engine, &self.identity.workspace);
        if stale_result.is_ok() {
            self.freshness_revoked = true;
        }
        live_result.and(stale_result)
    }
}

impl Drop for WorkspaceRuntime {
    fn drop(&mut self) {
        if let Some(live) = self.live.take()
            && let Err(error) = live.shutdown()
        {
            tracing::error!(%error, workspace = %self.identity.workspace, "failed to stop live index");
        }
        if !self.freshness_revoked
            && let Err(error) = mark_engine_stale(&self.engine, &self.identity.workspace)
        {
            tracing::error!(%error, workspace = %self.identity.workspace, "failed to revoke workspace freshness");
        }
    }
}

fn mark_engine_stale(
    engine: &WorkspaceEngine,
    workspace: &WorkspaceId,
) -> Result<(), WorkspaceRegistryError> {
    let mut last_conflict = None;
    for _ in 0..MAX_STALE_PUBLICATION_ATTEMPTS {
        let mut update = engine.begin_update();
        update.set_freshness(Freshness::Stale);
        update.set_status(WorkspaceStatus::Stale);
        match engine.publish(update) {
            Ok(_) => return Ok(()),
            Err(error) => last_conflict = Some(error),
        }
    }
    let source = match last_conflict {
        Some(error) => error,
        None => {
            let revision = engine.snapshot().revision();
            PublishError::Conflict {
                base: revision,
                current: revision,
            }
        }
    };
    Err(WorkspaceRegistryError::StalePublication {
        workspace: workspace.clone(),
        source,
    })
}

fn lock_state(state: &Mutex<RegistryState>) -> MutexGuard<'_, RegistryState> {
    match state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_workspace_bound() {
        assert!(matches!(
            WorkspaceRegistry::new(WorkspaceRegistryConfig { max_workspaces: 0 }),
            Err(WorkspaceRegistryError::InvalidMaxWorkspaces)
        ));
    }

    #[test]
    fn stopping_workspace_retains_ownership_reservation() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempfile::tempdir()?;
        let identity = WorkspaceIdentity::for_primary_worktree(root.path())?;
        let registry = WorkspaceRegistry::new(WorkspaceRegistryConfig { max_workspaces: 1 })?;
        registry.reserve(identity.clone())?;
        {
            let mut state = lock_state(&registry.state);
            state
                .entries
                .insert(identity.workspace.clone(), RegistryEntry::Stopping);
        }

        assert!(matches!(
            registry.reserve(identity.clone()),
            Err(WorkspaceRegistryError::AlreadyRegistered { .. })
        ));
        assert!(matches!(
            registry.workspace(&identity.workspace),
            Err(WorkspaceRegistryError::ShutdownInProgress { .. })
        ));
        assert!(matches!(
            registry.shutdown(),
            Err(WorkspaceRegistryError::WorkspaceTransitionsInProgress {
                starting: 0,
                stopping: 1
            })
        ));
        Ok(())
    }
}
