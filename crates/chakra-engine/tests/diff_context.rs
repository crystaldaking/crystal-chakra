//! Query-layer regression coverage for revision-safe Git/graph joins.

mod common;

use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use chakra_domain::location::RepoRelativePath;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{ChangeKind, DiffContextRequest, QueryError, QueryService};
use chakra_domain::revision::Revision;
use chakra_engine::{
    DiffWorkspace, FreshnessBarrier, FreshnessBarrierError, WorkspaceDiff, WorkspaceDiffError,
    WorkspaceDiffProvider, WorkspaceEngine, WorkspaceFileChange,
};

const SERVICE_PATH: &str = "src/service/payment_service.rs";

#[derive(Debug, Default)]
struct StaticDiffProvider {
    calls: AtomicUsize,
    wrong_revision: bool,
}

impl StaticDiffProvider {
    fn wrong_revision() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            wrong_revision: true,
        }
    }
}

impl WorkspaceDiffProvider for StaticDiffProvider {
    fn diff(&self, workspace: DiffWorkspace) -> Result<WorkspaceDiff, WorkspaceDiffError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(WorkspaceDiff {
            revision: if self.wrong_revision {
                Revision(workspace.revision.0 + 100)
            } else {
                workspace.revision
            },
            files: vec![WorkspaceFileChange {
                path: RepoRelativePath::new(SERVICE_PATH)
                    .map_err(|error| WorkspaceDiffError::new(error.to_string()))?,
                previous_path: None,
                change: ChangeKind::Modified,
                provenance: Provenance::Git,
                precision: Precision::Precise,
            }],
            truncated: false,
        })
    }
}

#[derive(Debug)]
struct OneRevisionChange {
    engine: Weak<WorkspaceEngine>,
    calls: AtomicUsize,
}

impl FreshnessBarrier for OneRevisionChange {
    fn require_fresh(&self) -> Result<(), FreshnessBarrierError> {
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
        limit: Some(20),
        ..DiffContextRequest::default()
    })?;
    assert_eq!(result.revision, Revision(1));
    assert!(!result.truncated);
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
        limit: Some(1),
        ..DiffContextRequest::default()
    })?;
    assert!(bounded.truncated);
    assert!(bounded.data.changed_files.len() <= 1);
    assert!(bounded.data.changed_symbols.len() <= 1);
    assert!(bounded.data.related_callers.len() <= 1);
    assert!(bounded.data.related_tests.len() <= 1);
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
