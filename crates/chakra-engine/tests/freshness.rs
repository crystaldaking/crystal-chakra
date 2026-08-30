//! Freshness is an axis of its own (SPEC §6): the publisher claims it, and
//! the query layer enforces each request's `FreshnessRequirement` — a
//! `RequireFresh` call must never be served from a stale snapshot.

mod common;

use std::error::Error;
use std::path::Path;

use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::query::{
    CallersRequest, ContextRequest, QueryError, QueryService, RepoMapRequest, SymbolRef,
    SymbolSearchRequest,
};
use chakra_domain::state::{Freshness, FreshnessRequirement, WorkspaceStatus};
use chakra_engine::WorkspaceEngine;

use common::{scenario_engine, scenario_graph};

fn unindexed_engine() -> Result<WorkspaceEngine, Box<dyn Error>> {
    let identity = WorkspaceIdentity::for_primary_worktree(Path::new("."))?;
    Ok(WorkspaceEngine::new(identity))
}

#[test]
fn require_fresh_is_rejected_until_first_reconciliation() -> Result<(), Box<dyn Error>> {
    let engine = unindexed_engine()?;
    let expected = QueryError::FreshnessNotMet {
        required: FreshnessRequirement::RequireFresh,
        actual: Freshness::Stale,
    };
    // Default is RequireFresh.
    assert_eq!(
        engine.repo_map(RepoMapRequest::default()).err(),
        Some(expected.clone())
    );
    assert_eq!(
        engine
            .symbol_search(SymbolSearchRequest {
                query: "refund".to_owned(),
                ..SymbolSearchRequest::default()
            })
            .err(),
        Some(expected.clone())
    );
    assert_eq!(
        engine
            .context(ContextRequest {
                source: Default::default(),
                symbol: Some(SymbolRef::ByName("refund".to_owned())),
                ..ContextRequest::default()
            })
            .err(),
        Some(expected.clone())
    );
    assert_eq!(
        engine
            .callers(CallersRequest {
                source: Default::default(),
                symbol: Some(SymbolRef::ByName("refund".to_owned())),
                ..CallersRequest::default()
            })
            .err(),
        Some(expected)
    );
    Ok(())
}

#[test]
fn allow_stale_is_served_with_a_stale_envelope() -> Result<(), Box<dyn Error>> {
    let engine = unindexed_engine()?;
    let envelope = engine.repo_map(RepoMapRequest {
        include_project_scope: false,
        freshness: FreshnessRequirement::AllowStale,
        ..RepoMapRequest::default()
    })?;
    assert_eq!(envelope.freshness, Freshness::Stale);
    assert_eq!(envelope.status, WorkspaceStatus::Initializing);
    Ok(())
}

#[test]
fn ready_alone_does_not_satisfy_require_fresh() -> Result<(), Box<dyn Error>> {
    let engine = unindexed_engine()?;
    let (graph, _) = scenario_graph()?;
    // Index published, but reconciliation is not confirmed: the publisher
    // claims Ready without claiming Fresh.
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Stale);
    engine.publish(update)?;

    let rejected = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        ..SymbolSearchRequest::default()
    });
    assert_eq!(
        rejected.err(),
        Some(QueryError::FreshnessNotMet {
            required: FreshnessRequirement::RequireFresh,
            actual: Freshness::Stale,
        })
    );

    let served = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        freshness: FreshnessRequirement::AllowStale,
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(served.freshness, Freshness::Stale);
    assert_eq!(served.status, WorkspaceStatus::Ready);
    assert!(!served.data.candidates.is_empty());
    Ok(())
}

#[test]
fn degraded_workspace_can_still_serve_fresh_syntax() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    // Precise provider failed, but the syntax snapshot is reconciled and
    // current: Degraded status with Fresh data must still serve RequireFresh.
    let mut update = engine.begin_update();
    update.set_status(WorkspaceStatus::Degraded);
    engine.publish(update)?;

    let envelope = engine.repo_map(RepoMapRequest::default())?;
    assert_eq!(envelope.status, WorkspaceStatus::Degraded);
    assert_eq!(envelope.freshness, Freshness::Fresh);
    assert_eq!(envelope.data.files.len(), 3);
    Ok(())
}

#[test]
fn freshness_is_revoked_when_the_worktree_changes() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    // A file changed; reconciliation is pending. Status stays Ready (the
    // service is up), but freshness is revoked until the next publish.
    let mut update = engine.begin_update();
    update.set_freshness(Freshness::Stale);
    engine.publish(update)?;

    let revision = engine.snapshot().revision();
    let rejected = engine.callers(CallersRequest {
        source: Default::default(),
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision,
        }),
        ..CallersRequest::default()
    });
    assert!(matches!(
        rejected,
        Err(QueryError::FreshnessNotMet {
            required: FreshnessRequirement::RequireFresh,
            actual: Freshness::Stale,
        })
    ));
    Ok(())
}
