//! Atomic published-revision ownership (SPEC §5, §35; ADR-0001).
//!
//! [`WorkspaceEngine`] is the single owner of the published revision.
//! Updates are constructed privately in an [`UpdateBuilder`] and published
//! with a compare-and-publish step: a builder whose base revision no longer
//! matches is rejected with [`PublishError::Conflict`], so a slow update can
//! never overwrite a newer revision.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use arc_swap::ArcSwap;
use chakra_domain::composition::WorkspaceGraphLayers;
use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::indexing::{IndexingDiagnostics, IndexingStatus};
use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;
use chakra_domain::revision::Revision;
use chakra_domain::state::{Freshness, ProviderState, WorkspaceStatus};
use chakra_domain::symbol::Language;
use thiserror::Error;

use crate::diff::WorkspaceDiffProvider;
use crate::graph::SymbolGraph;
use crate::precise::{PreciseProvider, ProviderInput, ProviderWorkspace};
use chakra_domain::project::ProjectModel;

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
    indexing: Arc<IndexingStatus>,
    graph: Arc<SymbolGraph>,
    /// Materialization-independent syntax facts for the exact base commit.
    commit_graph: Arc<SymbolGraph>,
    layers: Arc<WorkspaceGraphLayers>,
    provider_inputs: Arc<BTreeMap<chakra_domain::location::RepoRelativePath, ProviderInput>>,
    /// Typed Cargo/Composer project scope model published with the same
    /// atomic revision as the graph (issue #41).
    project_model: Arc<ProjectModel>,
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

    pub fn indexing(&self) -> &IndexingStatus {
        self.indexing.as_ref()
    }

    pub fn graph(&self) -> &SymbolGraph {
        self.graph.as_ref()
    }

    pub fn commit_graph(&self) -> &SymbolGraph {
        self.commit_graph.as_ref()
    }

    pub fn layers(&self) -> &WorkspaceGraphLayers {
        self.layers.as_ref()
    }

    /// The typed project scope model of this exact revision (issue #41).
    pub fn project_model(&self) -> &ProjectModel {
        self.project_model.as_ref()
    }

    pub(crate) fn graph_arc(&self) -> Arc<SymbolGraph> {
        self.graph.clone()
    }

    pub(crate) fn provider_inputs_arc(
        &self,
    ) -> Arc<BTreeMap<chakra_domain::location::RepoRelativePath, ProviderInput>> {
        self.provider_inputs.clone()
    }

    /// Whether the ordered provider-input catalog matches a privately built
    /// reconciliation candidate without allocating a second catalog.
    pub fn provider_inputs_match(&self, inputs: &[ProviderInput]) -> bool {
        self.provider_inputs.len() == inputs.len()
            && self.provider_inputs.values().eq(inputs.iter())
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
    fn require_fresh(&self) -> Result<(), FreshnessBarrierError> {
        self.require_fresh_with_context(&OperationContext::unbounded())
    }

    fn require_fresh_with_context(
        &self,
        operation: &OperationContext,
    ) -> Result<(), FreshnessBarrierError>;

    /// Optional bounded operational diagnostics from the same live owner.
    /// Keeping this on the freshness owner makes installation atomic: the
    /// engine cannot retain diagnostics for a stopped worker after a second
    /// owner-install step fails.
    fn index_diagnostics(&self) -> Option<IndexingDiagnostics> {
        None
    }
}

/// A workspace has exactly one owner for freshness reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a freshness barrier is already installed")]
pub struct BarrierAlreadyInstalled;

/// A provider cannot be installed unless routing stays unambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderInstallError {
    #[error("precise provider {provider} does not support a known language")]
    NoSupportedLanguages { provider: String },
    #[error(
        "language {language:?} is already owned by {installed}; cannot also install {candidate}"
    )]
    LanguageConflict {
        language: Language,
        installed: String,
        candidate: String,
    },
}

/// A workspace has at most one Git/workspace diff adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a workspace diff provider is already installed")]
pub struct DiffProviderAlreadyInstalled;

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
    indexing: Arc<IndexingStatus>,
    graph: Arc<SymbolGraph>,
    commit_graph: Arc<SymbolGraph>,
    layers: Arc<WorkspaceGraphLayers>,
    provider_inputs: Arc<BTreeMap<chakra_domain::location::RepoRelativePath, ProviderInput>>,
    project_model: Arc<ProjectModel>,
}

impl UpdateBuilder {
    /// Revision this update is based on.
    pub fn base_revision(&self) -> Revision {
        self.base_revision
    }

    /// Mutable access for low-level edits to an owned graph.
    ///
    /// Composed workspace graphs are immutable partitions and reject direct
    /// mutation. Live language reconciliation therefore builds or updates the
    /// owned language partitions privately and installs a newly composed graph
    /// with [`UpdateBuilder::replace_graph`].
    ///
    /// Mutating the graph changes the published relationship to the
    /// filesystem, so this revokes any inherited or previously claimed
    /// freshness: the publisher must re-confirm reconciliation and call
    /// [`UpdateBuilder::set_freshness`] *after* its graph edits.
    pub fn graph_mut(&mut self) -> &mut SymbolGraph {
        self.freshness = Freshness::Stale;
        Arc::make_mut(&mut self.graph)
    }

    /// Replaces the whole graph (used by the initial full index).
    ///
    /// Like [`UpdateBuilder::graph_mut`], this revokes freshness: a replaced
    /// graph is only `Fresh` once the publisher explicitly says the
    /// worktree was reconciled.
    pub fn replace_graph(&mut self, graph: SymbolGraph) {
        self.freshness = Freshness::Stale;
        self.graph = Arc::new(graph);
    }

    /// Replaces the immutable commit layer and its composition metadata in
    /// the same private update as the effective graph.
    pub fn set_graph_layers(&mut self, commit_graph: SymbolGraph, layers: WorkspaceGraphLayers) {
        self.commit_graph = Arc::new(commit_graph);
        self.layers = Arc::new(layers);
    }

    /// Updates the overlay description while retaining the current commit
    /// graph allocation.
    pub fn set_layers(&mut self, layers: WorkspaceGraphLayers) {
        self.layers = Arc::new(layers);
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

    /// Attaches indexing coverage to the same atomic revision as its graph.
    pub fn set_indexing(&mut self, indexing: IndexingStatus) {
        self.indexing = Arc::new(indexing);
    }

    /// Attaches non-source provider freshness inputs to the same atomic
    /// revision as the syntax graph and indexing metadata.
    pub fn set_provider_inputs(&mut self, inputs: impl IntoIterator<Item = ProviderInput>) {
        self.provider_inputs = Arc::new(
            inputs
                .into_iter()
                .map(|input| (input.path.clone(), input))
                .collect(),
        );
    }

    /// Attaches the typed project scope model to the same atomic revision as
    /// the syntax graph (issue #41). Like the graph, a replaced model is only
    /// `Fresh` once the publisher re-confirms reconciliation.
    pub fn set_project_model(&mut self, model: ProjectModel) {
        self.project_model = Arc::new(model);
    }
}

/// Owns and atomically publishes workspace revisions.
#[derive(Debug)]
pub struct WorkspaceEngine {
    current: ArcSwap<WorkspaceSnapshot>,
    freshness_barrier: OnceLock<Arc<dyn FreshnessBarrier>>,
    precise_providers: RwLock<Vec<Arc<dyn PreciseProvider>>>,
    diff_provider: OnceLock<Arc<dyn WorkspaceDiffProvider>>,
    cold_builds: AtomicU64,
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
            indexing: Arc::new(IndexingStatus::default()),
            graph: Arc::new(SymbolGraph::new()),
            commit_graph: Arc::new(SymbolGraph::new()),
            layers: Arc::new(WorkspaceGraphLayers::default()),
            provider_inputs: Arc::new(BTreeMap::new()),
            project_model: Arc::new(ProjectModel::default()),
        };
        Self {
            current: ArcSwap::from_pointee(snapshot),
            freshness_barrier: OnceLock::new(),
            precise_providers: RwLock::new(Vec::new()),
            diff_provider: OnceLock::new(),
            cold_builds: AtomicU64::new(0),
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

    /// Installs one language-neutral precise-provider adapter. Languages are
    /// exclusive so query routing cannot silently depend on install order.
    pub fn install_precise_provider(
        &self,
        provider: Arc<dyn PreciseProvider>,
    ) -> Result<(), ProviderInstallError> {
        let supported: Vec<_> = Language::ALL
            .into_iter()
            .filter(|language| provider.supports(*language))
            .collect();
        if supported.is_empty() {
            return Err(ProviderInstallError::NoSupportedLanguages {
                provider: provider.name().to_owned(),
            });
        }
        let mut providers = write_providers(&self.precise_providers);
        for language in supported {
            if let Some(installed) = providers
                .iter()
                .find(|installed| installed.supports(language))
            {
                return Err(ProviderInstallError::LanguageConflict {
                    language,
                    installed: installed.name().to_owned(),
                    candidate: provider.name().to_owned(),
                });
            }
        }
        providers.push(provider);
        Ok(())
    }

    /// Installs the Git/workspace adapter used by `diff_context`.
    pub fn install_diff_provider(
        &self,
        provider: Arc<dyn WorkspaceDiffProvider>,
    ) -> Result<(), DiffProviderAlreadyInstalled> {
        self.diff_provider
            .set(provider)
            .map_err(|_| DiffProviderAlreadyInstalled)
    }

    /// Published revisions whose graph was fully rebuilt instead of
    /// structurally reused (initial cold builds), observed at publication.
    pub fn cold_builds(&self) -> u64 {
        self.cold_builds.load(Ordering::Relaxed)
    }

    /// Snapshots the installed live diagnostics owner, if any, with the
    /// engine-observed cold-build counter merged in.
    pub(crate) fn index_diagnostics(&self) -> Option<IndexingDiagnostics> {
        let diagnostics = self
            .freshness_barrier
            .get()
            .and_then(|barrier| barrier.index_diagnostics());
        diagnostics.map(|mut diagnostics| {
            diagnostics.counters.cold_builds = self.cold_builds();
            diagnostics
        })
    }

    /// Captures the current immutable syntax input for provider startup.
    pub fn provider_workspace(&self) -> ProviderWorkspace {
        ProviderWorkspace::from_snapshot(&self.snapshot())
    }

    pub(crate) fn precise_providers(&self) -> Vec<Arc<dyn PreciseProvider>> {
        read_providers(&self.precise_providers).clone()
    }

    pub(crate) fn precise_provider_for_path(
        &self,
        language: Language,
        path: &RepoRelativePath,
    ) -> Option<Arc<dyn PreciseProvider>> {
        read_providers(&self.precise_providers)
            .iter()
            .find(|provider| provider.supports_path(language, path))
            .cloned()
    }

    pub(crate) fn diff_provider(&self) -> Option<&Arc<dyn WorkspaceDiffProvider>> {
        self.diff_provider.get()
    }

    /// Waits until the installed owner has reconciled and published the
    /// latest syntax state. Static engines without a live owner retain their
    /// already-published freshness semantics.
    pub fn require_fresh(&self) -> Result<(), FreshnessBarrierError> {
        self.freshness_barrier
            .get()
            .map_or(Ok(()), |barrier| barrier.require_fresh())
    }

    /// Context-aware form used by long-running queries so caller
    /// cancellation and the end-to-end deadline reach reconciliation.
    pub fn require_fresh_with_context(
        &self,
        operation: &OperationContext,
    ) -> Result<(), FreshnessBarrierError> {
        self.freshness_barrier.get().map_or_else(
            || {
                operation
                    .check()
                    .map_err(|error| FreshnessBarrierError::new(error.to_string()))
            },
            |barrier| barrier.require_fresh_with_context(operation),
        )
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
            indexing: base.indexing.clone(),
            graph: base.graph.clone(),
            commit_graph: base.commit_graph.clone(),
            layers: base.layers.clone(),
            provider_inputs: base.provider_inputs.clone(),
            project_model: base.project_model.clone(),
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
        let graph_replaced = !Arc::ptr_eq(&current.graph, &update.graph);
        let next = Arc::new(WorkspaceSnapshot {
            identity: update.identity,
            revision: update.base_revision.next(),
            status: update.status,
            freshness: update.freshness,
            provider_state: update.provider_state,
            indexing: update.indexing,
            graph: update.graph,
            commit_graph: update.commit_graph,
            layers: update.layers,
            provider_inputs: update.provider_inputs,
            project_model: update.project_model,
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
        // A publication that reports every retained file as rebuilt is an
        // initial/full (cold) build; structurally incremental revisions reuse
        // payloads instead (issue #43).
        let publication = &next.indexing.publication;
        if graph_replaced && !publication.structurally_incremental && publication.rebuilt_files > 0
        {
            self.cold_builds.fetch_add(1, Ordering::Relaxed);
        }
        Ok(next)
    }
}

fn read_providers(
    providers: &RwLock<Vec<Arc<dyn PreciseProvider>>>,
) -> std::sync::RwLockReadGuard<'_, Vec<Arc<dyn PreciseProvider>>> {
    match providers.read() {
        Ok(providers) => providers,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_providers(
    providers: &RwLock<Vec<Arc<dyn PreciseProvider>>>,
) -> std::sync::RwLockWriteGuard<'_, Vec<Arc<dyn PreciseProvider>>> {
    match providers.write() {
        Ok(providers) => providers,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct LanguageProvider {
        name: &'static str,
        language: Option<Language>,
    }

    impl PreciseProvider for LanguageProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supports(&self, language: Language) -> bool {
            self.language == Some(language)
        }

        fn state_for(&self, _revision: Revision) -> ProviderState {
            ProviderState::Dormant
        }

        fn enrich_with_context(
            &self,
            request: crate::PreciseQueryRequest,
            _operation: &OperationContext,
        ) -> crate::PreciseQueryResult {
            crate::PreciseQueryResult::unavailable(
                request.workspace.revision,
                ProviderState::Dormant,
            )
        }
    }

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
    fn precise_provider_installation_routes_disjoint_languages()
    -> Result<(), Box<dyn std::error::Error>> {
        let engine = engine()?;
        engine.install_precise_provider(Arc::new(LanguageProvider {
            name: "rust",
            language: Some(Language::Rust),
        }))?;
        engine.install_precise_provider(Arc::new(LanguageProvider {
            name: "python",
            language: Some(Language::Python),
        }))?;

        assert_eq!(
            engine
                .precise_provider_for_path(Language::Rust, &RepoRelativePath::new("src/lib.rs")?)
                .map(|provider| provider.name()),
            Some("rust")
        );
        assert_eq!(
            engine
                .precise_provider_for_path(Language::Python, &RepoRelativePath::new("src/app.py")?)
                .map(|provider| provider.name()),
            Some("python")
        );
        assert!(
            engine
                .precise_provider_for_path(Language::Php, &RepoRelativePath::new("src/app.php")?)
                .is_none()
        );
        assert_eq!(engine.precise_providers().len(), 2);
        Ok(())
    }

    #[test]
    fn precise_provider_installation_rejects_ambiguous_or_empty_routing()
    -> Result<(), Box<dyn std::error::Error>> {
        let engine = engine()?;
        engine.install_precise_provider(Arc::new(LanguageProvider {
            name: "first-rust",
            language: Some(Language::Rust),
        }))?;
        assert!(matches!(
            engine.install_precise_provider(Arc::new(LanguageProvider {
                name: "second-rust",
                language: Some(Language::Rust),
            })),
            Err(ProviderInstallError::LanguageConflict {
                language: Language::Rust,
                ..
            })
        ));
        assert!(matches!(
            engine.install_precise_provider(Arc::new(LanguageProvider {
                name: "empty",
                language: None,
            })),
            Err(ProviderInstallError::NoSupportedLanguages { .. })
        ));
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

    #[test]
    fn indexing_coverage_is_published_atomically_with_its_graph()
    -> Result<(), Box<dyn std::error::Error>> {
        let engine = engine()?;
        let before = engine.snapshot();
        let mut indexing = IndexingStatus::default();
        indexing.coverage.discovered_files = 3;
        indexing.coverage.indexed_files = 2;
        indexing.coverage.skipped_files = 1;

        let mut update = engine.begin_update();
        update.replace_graph(SymbolGraph::new());
        update.set_indexing(indexing.clone());
        let after = engine.publish(update)?;

        assert_eq!(before.indexing(), &IndexingStatus::default());
        assert_eq!(after.indexing(), &indexing);
        assert_eq!(engine.snapshot().indexing(), &indexing);
        Ok(())
    }

    #[test]
    fn freshness_only_publication_does_not_double_count_a_cold_build()
    -> Result<(), Box<dyn std::error::Error>> {
        let engine = engine()?;
        let mut indexing = IndexingStatus::default();
        indexing.publication.rebuilt_files = 1;

        let mut cold = engine.begin_update();
        cold.replace_graph(SymbolGraph::new());
        cold.set_indexing(indexing);
        engine.publish(cold)?;
        assert_eq!(engine.cold_builds(), 1);

        let mut freshness_only = engine.begin_update();
        freshness_only.set_freshness(Freshness::Fresh);
        engine.publish(freshness_only)?;
        assert_eq!(engine.cold_builds(), 1);
        Ok(())
    }
}
