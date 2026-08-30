//! Query-layer regression coverage for revision-safe Git/graph joins.

mod common;

use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use chakra_domain::envelope::{TruncationCause, TruncationSection};
use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    ChangeKind, DiffContextRequest, DiffScope, QueryError, QueryService, ResolvedDiffScope,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::Freshness;
use chakra_engine::{
    DiffInventoryTruncation, DiffWorkspace, FreshnessBarrier, FreshnessBarrierError, WorkspaceDiff,
    WorkspaceDiffError, WorkspaceDiffProvider, WorkspaceEngine, WorkspaceFileChange,
};

const SERVICE_PATH: &str = "src/service/payment_service.rs";

#[derive(Debug, Default)]
struct StaticDiffProvider {
    calls: AtomicUsize,
    wrong_revision: bool,
    wrong_scope: bool,
    inventory_truncated: bool,
}

#[derive(Debug)]
struct EmptyDiffProvider;

#[derive(Debug)]
struct NoisyDiffProvider;

impl WorkspaceDiffProvider for EmptyDiffProvider {
    fn diff_with_context(
        &self,
        workspace: DiffWorkspace,
        _operation: &OperationContext,
    ) -> Result<WorkspaceDiff, WorkspaceDiffError> {
        Ok(WorkspaceDiff {
            revision: workspace.revision,
            scope: ResolvedDiffScope {
                requested: workspace.scope,
                base_commit: None,
            },
            files: Vec::new(),
            truncation: None,
        })
    }
}

impl WorkspaceDiffProvider for NoisyDiffProvider {
    fn diff_with_context(
        &self,
        workspace: DiffWorkspace,
        operation: &OperationContext,
    ) -> Result<WorkspaceDiff, WorkspaceDiffError> {
        operation
            .check()
            .map_err(|error| WorkspaceDiffError::new(error.to_string()))?;
        let long_component = "noise".repeat(100);
        let mut files = (0..200)
            .map(|index| {
                RepoRelativePath::new(format!("src/aaa/{index:03}-{long_component}.rs"))
                    .map(|path| WorkspaceFileChange {
                        path,
                        previous_path: None,
                        change: ChangeKind::Modified,
                        provenance: Provenance::Git,
                        precision: Precision::Precise,
                    })
                    .map_err(|error| WorkspaceDiffError::new(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        files.push(WorkspaceFileChange {
            path: RepoRelativePath::new(SERVICE_PATH)
                .map_err(|error| WorkspaceDiffError::new(error.to_string()))?,
            previous_path: None,
            change: ChangeKind::Modified,
            provenance: Provenance::Git,
            precision: Precision::Precise,
        });
        Ok(WorkspaceDiff {
            revision: workspace.revision,
            scope: ResolvedDiffScope {
                requested: workspace.scope,
                base_commit: None,
            },
            files,
            truncation: None,
        })
    }
}

impl StaticDiffProvider {
    fn wrong_revision() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            wrong_revision: true,
            wrong_scope: false,
            inventory_truncated: false,
        }
    }

    fn wrong_scope() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            wrong_revision: false,
            wrong_scope: true,
            inventory_truncated: false,
        }
    }
}

impl WorkspaceDiffProvider for StaticDiffProvider {
    fn diff_with_context(
        &self,
        workspace: DiffWorkspace,
        _operation: &OperationContext,
    ) -> Result<WorkspaceDiff, WorkspaceDiffError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(WorkspaceDiff {
            revision: if self.wrong_revision {
                Revision(workspace.revision.0 + 100)
            } else {
                workspace.revision
            },
            scope: ResolvedDiffScope {
                requested: if self.wrong_scope {
                    DiffScope::BaseRef {
                        reference: "wrong".to_owned(),
                    }
                } else {
                    workspace.scope
                },
                base_commit: Some("1111111111111111111111111111111111111111".to_owned()),
            },
            files: vec![WorkspaceFileChange {
                path: RepoRelativePath::new(SERVICE_PATH)
                    .map_err(|error| WorkspaceDiffError::new(error.to_string()))?,
                previous_path: None,
                change: ChangeKind::Modified,
                provenance: Provenance::Git,
                precision: Precision::Precise,
            }],
            truncation: self.inventory_truncated.then_some(DiffInventoryTruncation {
                limit: 10_000,
                omitted: None,
            }),
        })
    }
}

#[derive(Debug)]
struct OneRevisionChange {
    engine: Weak<WorkspaceEngine>,
    calls: AtomicUsize,
}

impl FreshnessBarrier for OneRevisionChange {
    fn require_fresh_with_context(
        &self,
        _operation: &OperationContext,
    ) -> Result<(), FreshnessBarrierError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 1 {
            let engine = self
                .engine
                .upgrade()
                .ok_or_else(|| FreshnessBarrierError::new("test engine was dropped"))?;
            engine
                .publish(engine.begin_update())
                .map_err(|error| FreshnessBarrierError::new(error.to_string()))?;
        }
        Ok(())
    }
}

#[test]
fn joins_changed_files_to_bounded_symbols_callers_and_tests() -> Result<(), Box<dyn Error>> {
    let (engine, _ids) = common::scenario_engine()?;
    engine.install_diff_provider(Arc::new(StaticDiffProvider::default()))?;

    let result = engine.diff_context(DiffContextRequest {
        source: Default::default(),
        limit: Some(20),
        ..DiffContextRequest::default()
    })?;
    assert_eq!(result.revision, Revision(1));
    assert!(!result.truncated);
    assert_eq!(result.data.scope.requested, DiffScope::Worktree);
    assert_eq!(
        result.data.scope.base_commit.as_deref(),
        Some("1111111111111111111111111111111111111111")
    );
    assert_eq!(result.data.changed_files.len(), 1);
    assert_eq!(result.data.changed_files[0].path.as_str(), SERVICE_PATH);
    assert_eq!(result.data.changed_files[0].precision, Precision::Precise);
    assert!(result.data.changed_symbols.iter().all(|changed| {
        changed.provenance == Provenance::Heuristic
            && changed.precision == Precision::Heuristic
            && changed.symbol.provenance == Provenance::TreeSitter
            && changed.symbol.precision == Precision::Syntax
    }));
    assert!(result.data.changed_symbols.iter().any(|changed| {
        changed.symbol.qualified_name == "service::payment_service::PaymentService::refund"
    }));
    assert!(result.data.related_callers.iter().any(|caller| {
        caller.relation.symbol.qualified_name == "api::controller::PaymentController::refund"
            && caller.relation.provenance == Provenance::TreeSitter
            && result
                .data
                .changed_symbols
                .iter()
                .any(|changed| changed.symbol.id == caller.changed_symbol_id)
    }));
    assert_eq!(result.data.related_tests.len(), 2);

    let bounded = engine.diff_context(DiffContextRequest {
        source: Default::default(),
        limit: Some(1),
        ..DiffContextRequest::default()
    })?;
    assert!(bounded.truncated);
    assert!(bounded.data.changed_files.len() <= 1);
    assert!(bounded.data.changed_symbols.len() <= 1);
    assert!(bounded.data.related_callers.len() <= 1);
    assert!(bounded.data.related_tests.len() <= 1);
    assert!(bounded.truncation.iter().all(|detail| {
        detail.cause == TruncationCause::ItemLimit
            || detail.cause == TruncationCause::UnresolvedCandidateFanout
    }));
    assert!(bounded.truncation.iter().any(|detail| {
        detail.section == TruncationSection::DiffContextChangedSymbols
            && detail.cause == TruncationCause::ItemLimit
    }));
    Ok(())
}

#[test]
fn changed_file_byte_budget_does_not_starve_changed_symbols() -> Result<(), Box<dyn Error>> {
    let (engine, _ids) = common::scenario_engine()?;
    engine.install_diff_provider(Arc::new(NoisyDiffProvider))?;

    let result = engine.diff_context(DiffContextRequest {
        source: Default::default(),
        limit: Some(500),
        ..DiffContextRequest::default()
    })?;

    assert!(result.truncation.iter().any(|detail| {
        detail.section == TruncationSection::DiffContextChangedFiles
            && detail.cause == TruncationCause::ResponseByteLimit
    }));
    assert!(
        !result
            .data
            .changed_files
            .iter()
            .any(|change| change.path.as_str() == SERVICE_PATH)
    );
    assert!(result.data.changed_symbols.iter().any(|changed| {
        changed.symbol.qualified_name == "service::payment_service::PaymentService::refund"
    }));
    Ok(())
}

#[test]
fn empty_diff_is_not_contaminated_by_workspace_call_candidate_truncation()
-> Result<(), Box<dyn Error>> {
    let (engine, _ids) = common::scenario_engine()?;
    let mut update = engine.begin_update();
    update.graph_mut().set_truncated_call_sites(66)?;
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    engine.install_diff_provider(Arc::new(EmptyDiffProvider))?;

    let status = engine.status(chakra_domain::query::StatusRequest)?;
    assert_eq!(status.data.counts.call_sites_with_truncated_candidates, 66);
    assert!(!status.truncated);

    let result = engine.diff_context(DiffContextRequest::default())?;
    assert!(result.data.changed_files.is_empty());
    assert!(!result.truncated);
    assert!(result.truncation.is_empty());
    Ok(())
}

#[test]
fn diff_inventory_truncation_is_reported_separately_from_query_item_limits()
-> Result<(), Box<dyn Error>> {
    let (engine, _ids) = common::scenario_engine()?;
    engine.install_diff_provider(Arc::new(StaticDiffProvider {
        inventory_truncated: true,
        ..StaticDiffProvider::default()
    }))?;

    let result = engine.diff_context(DiffContextRequest {
        source: Default::default(),
        limit: Some(20),
        ..DiffContextRequest::default()
    })?;
    assert!(result.truncated);
    assert_eq!(result.truncation.len(), 1);
    assert_eq!(
        result.truncation[0].section,
        TruncationSection::DiffContextChangedFiles
    );
    assert_eq!(
        result.truncation[0].cause,
        TruncationCause::DiffInventoryLimit
    );
    assert_eq!(result.truncation[0].limit, 10_000);
    assert_eq!(result.truncation[0].omitted, None);
    Ok(())
}

#[test]
fn refuses_a_diff_labeled_with_the_wrong_revision() -> Result<(), Box<dyn Error>> {
    let (engine, _ids) = common::scenario_engine()?;
    let provider = Arc::new(StaticDiffProvider::wrong_revision());
    engine.install_diff_provider(provider.clone())?;

    let result = engine.diff_context(DiffContextRequest::default());
    assert!(matches!(result, Err(QueryError::DiffUnavailable(_))));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    Ok(())
}

#[test]
fn refuses_a_diff_labeled_with_a_different_scope() -> Result<(), Box<dyn Error>> {
    let (engine, _ids) = common::scenario_engine()?;
    let provider = Arc::new(StaticDiffProvider::wrong_scope());
    engine.install_diff_provider(provider.clone())?;

    let result = engine.diff_context(DiffContextRequest::default());
    assert!(matches!(result, Err(QueryError::DiffUnavailable(_))));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    Ok(())
}

#[test]
fn retries_when_the_published_revision_changes_during_the_git_read() -> Result<(), Box<dyn Error>> {
    let (engine, _ids) = common::scenario_engine()?;
    let engine = Arc::new(engine);
    let provider = Arc::new(StaticDiffProvider::default());
    engine.install_diff_provider(provider.clone())?;
    engine.install_freshness_barrier(Arc::new(OneRevisionChange {
        engine: Arc::downgrade(&engine),
        calls: AtomicUsize::new(0),
    }))?;

    let result = engine.diff_context(DiffContextRequest::default())?;
    assert_eq!(result.revision, Revision(2));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    Ok(())
}
