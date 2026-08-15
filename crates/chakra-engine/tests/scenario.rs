//! Query-service behavior over the Controller → Service → Provider scenario.

mod common;

use std::error::Error;

use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    CallersRequest, ContextRequest, DiffContextRequest, QueryError, QueryService, RepoMapRequest,
    SearchRequest, StatusRequest, SymbolRef, SymbolSearchRequest,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::{Freshness, ProviderState, WorkspaceStatus};
use chakra_domain::symbol::SymbolKind;
use chakra_engine::SymbolGraph;

use common::{scenario_engine, scenario_graph};

#[test]
fn status_reports_scenario_counts() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    let envelope = engine.status(StatusRequest)?;
    assert_eq!(envelope.schema_version, 1);
    assert_eq!(envelope.revision, Revision(1));
    assert_eq!(envelope.freshness, Freshness::Fresh);
    assert_eq!(envelope.status, WorkspaceStatus::Ready);
    assert_eq!(envelope.provider_state, ProviderState::NotConfigured);
    assert!(!envelope.truncated);
    assert_eq!(envelope.data.counts.symbols, 10);
    assert_eq!(envelope.data.counts.edges, 6);
    assert_eq!(envelope.data.counts.files, 3);
    assert_eq!(envelope.data.providers.len(), 1);
    assert_eq!(envelope.data.providers[0].name, "rust-analyzer");
    Ok(())
}

#[test]
fn repo_map_lists_files_sorted_with_counts() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    let envelope = engine.repo_map(RepoMapRequest::default())?;
    let files = &envelope.data.files;
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].path.as_str(), "src/api/controller.rs");
    assert_eq!(files[0].symbol_count, 2);
    assert_eq!(files[1].path.as_str(), "src/provider/mod.rs");
    assert_eq!(files[1].symbol_count, 4);
    assert_eq!(files[2].path.as_str(), "src/service/payment_service.rs");
    assert_eq!(files[2].symbol_count, 4);
    Ok(())
}

#[test]
fn symbol_search_matches_names_and_respects_budgets() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;

    let found = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    // 4 refund methods + 2 test functions whose names contain "refund".
    assert_eq!(found.data.candidates.len(), 6);
    assert!(!found.truncated);
    assert!(
        found
            .data
            .candidates
            .iter()
            .all(|c| c.precision == Precision::Syntax)
    );

    let limited = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        limit: Some(2),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(limited.data.candidates.len(), 2);
    assert!(limited.truncated);

    let empty = engine.symbol_search(SymbolSearchRequest {
        query: "   ".to_owned(),
        ..SymbolSearchRequest::default()
    });
    assert!(matches!(empty, Err(QueryError::Invalid(_))));
    Ok(())
}

#[test]
fn bare_name_refund_is_ambiguous_not_guessed() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    let result = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName("refund".to_owned())),
        ..CallersRequest::default()
    });
    let expected = QueryError::AmbiguousSymbol {
        query: "refund".to_owned(),
        candidates: 4,
    };
    assert_eq!(result.err(), Some(expected));
    Ok(())
}

#[test]
fn qualified_name_resolves_unambiguously() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let envelope = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName(
            "service::payment_service::PaymentService::refund".to_owned(),
        )),
        ..CallersRequest::default()
    })?;
    assert_eq!(envelope.data.target.id, ids.service_refund);
    assert_eq!(envelope.data.callers.len(), 1);
    assert_eq!(envelope.data.callers[0].symbol.id, ids.controller_refund);
    Ok(())
}

#[test]
fn callers_of_provider_trait_method_is_the_service() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let revision = engine.snapshot().revision();
    let envelope = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision,
        }),
        ..CallersRequest::default()
    })?;
    assert_eq!(envelope.data.callers.len(), 1);
    let caller = &envelope.data.callers[0];
    assert_eq!(caller.symbol.id, ids.service_refund);
    // Syntax call candidates must never masquerade as precise (SPEC §7).
    assert_eq!(caller.precision, Precision::Syntax);
    assert_eq!(caller.provenance, Provenance::TreeSitter);
    assert!(caller.location.is_some());
    Ok(())
}

#[test]
fn context_combines_bounded_relations() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let revision = engine.snapshot().revision();
    let envelope = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.service_refund,
            revision,
        }),
        ..ContextRequest::default()
    })?;
    let data = &envelope.data;
    assert_eq!(data.symbol.id, ids.service_refund);
    assert_eq!(data.symbol.kind, SymbolKind::Method);
    assert!(data.symbol.signature.is_some());
    assert_eq!(data.callers.len(), 1);
    assert_eq!(data.callers[0].symbol.id, ids.controller_refund);
    assert_eq!(data.callees.len(), 1);
    assert_eq!(data.callees[0].symbol.id, ids.provider_refund);
    assert_eq!(data.tests.len(), 2);
    let test_ids: Vec<_> = data.tests.iter().map(|t| t.symbol.id).collect();
    assert!(test_ids.contains(&ids.test_delegates));
    assert!(test_ids.contains(&ids.test_rejects_zero));
    assert!(data.implementations.is_empty());
    let files: Vec<&str> = data.related_files.iter().map(|f| f.as_str()).collect();
    assert_eq!(
        files,
        vec![
            "src/api/controller.rs",
            "src/provider/mod.rs",
            "src/service/payment_service.rs"
        ]
    );
    assert!(!envelope.truncated);
    Ok(())
}

#[test]
fn trait_method_context_shows_implementations() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let revision = engine.snapshot().revision();
    let envelope = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision,
        }),
        ..ContextRequest::default()
    })?;
    assert_eq!(envelope.data.implementations.len(), 1);
    assert_eq!(
        envelope.data.implementations[0].symbol.id,
        ids.stripe_refund
    );
    Ok(())
}

#[test]
fn struct_and_trait_kinds_are_exposed() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let kinds_of = |query: &str| -> Result<
        Vec<(chakra_domain::symbol::EntityId, SymbolKind)>,
        Box<dyn Error>,
    > {
        let found = engine.symbol_search(SymbolSearchRequest {
            query: query.to_owned(),
            ..SymbolSearchRequest::default()
        })?;
        Ok(found
            .data
            .candidates
            .iter()
            .map(|c| (c.id, c.kind))
            .collect())
    };
    let payments = kinds_of("payment")?;
    assert!(payments.contains(&(ids.controller_struct, SymbolKind::Struct)));
    assert!(payments.contains(&(ids.service_struct, SymbolKind::Struct)));
    let providers = kinds_of("provider")?;
    assert!(providers.contains(&(ids.provider_trait, SymbolKind::Trait)));
    assert!(providers.contains(&(ids.stripe_struct, SymbolKind::Struct)));
    Ok(())
}

#[test]
fn unimplemented_queries_fail_with_typed_errors() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    let search = engine.search(SearchRequest {
        query: "refund".to_owned(),
        ..SearchRequest::default()
    });
    assert!(matches!(search, Err(QueryError::Unsupported("search"))));
    let diff = engine.diff_context(DiffContextRequest::default());
    assert!(matches!(diff, Err(QueryError::Unsupported("diff_context"))));
    Ok(())
}

#[test]
fn missing_and_unknown_symbol_refs_are_typed_errors() -> Result<(), Box<dyn Error>> {
    let (engine, _) = scenario_engine()?;
    let revision = engine.snapshot().revision();
    let missing = engine.context(ContextRequest::default());
    assert!(matches!(missing, Err(QueryError::MissingSymbolRef)));

    let unknown = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: chakra_domain::symbol::EntityId(9999),
            revision,
        }),
        ..CallersRequest::default()
    });
    assert!(matches!(unknown, Err(QueryError::SymbolNotFound(_))));

    let absent = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName("does_not_exist".to_owned())),
        ..CallersRequest::default()
    });
    assert!(matches!(absent, Err(QueryError::SymbolNotFound(_))));
    Ok(())
}

#[test]
fn entity_ids_are_scoped_to_their_revision() -> Result<(), Box<dyn Error>> {
    let (engine, ids) = scenario_engine()?;
    let stale_revision = engine.snapshot().revision();

    // Any newer publication makes old ids unresolvable by value.
    engine.publish(engine.begin_update())?;

    let result = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: ids.provider_refund,
            revision: stale_revision,
        }),
        ..CallersRequest::default()
    });
    let expected = QueryError::StaleSymbolRef {
        reference_revision: stale_revision,
        current_revision: Revision(2),
    };
    assert_eq!(result.err(), Some(expected));

    // Publish a graph whose arena order differs, so the old numeric index now
    // denotes a DIFFERENT symbol — the exact hazard revision scoping exists
    // for. The scenario graph is republished in reverse declaration order.
    let (scenario, _) = scenario_graph()?;
    let count = scenario.symbol_count();
    let mut reversed = SymbolGraph::new();
    for symbol in scenario.symbols().iter().rev() {
        reversed.add_symbol(
            symbol.key.clone(),
            symbol.location.clone(),
            symbol.signature.clone(),
            symbol.provenance,
            symbol.precision,
        )?;
    }
    let remap =
        |id: chakra_domain::symbol::EntityId| chakra_domain::symbol::EntityId(count - 1 - id.0);
    for symbol in scenario.symbols() {
        for edge in scenario.outgoing_edges(symbol.id) {
            reversed.add_edge(
                edge.kind,
                remap(edge.from),
                remap(edge.to),
                edge.provenance,
                edge.precision,
                edge.location.clone(),
            )?;
        }
    }
    let mut update = engine.begin_update();
    update.replace_graph(reversed);
    // Replacing the graph revoked freshness; this update stands in for a
    // completed reconciliation, so it re-claims `Fresh` explicitly.
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    let snapshot = engine.snapshot();
    let current_revision = snapshot.revision();
    let graph = snapshot.graph();

    // The stale index now resolves to another symbol entirely…
    let hijacked = graph
        .symbol(ids.provider_refund)
        .ok_or("remapped graph lost the old index")?;
    assert_ne!(
        hijacked.key.qualified_name,
        "provider::PaymentProvider::refund"
    );

    // …so the client re-resolves by name against the current revision and
    // gets a fresh, correct id.
    let matches = graph.resolve_name("provider::PaymentProvider::refund");
    let fresh_id = *matches
        .first()
        .ok_or("provider::PaymentProvider::refund missing after remap")?;
    assert_ne!(fresh_id, ids.provider_refund);

    let resolved = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: fresh_id,
            revision: current_revision,
        }),
        ..CallersRequest::default()
    })?;
    assert_eq!(
        resolved.data.target.qualified_name,
        "provider::PaymentProvider::refund"
    );
    assert_eq!(resolved.data.callers.len(), 1);
    Ok(())
}

#[test]
fn unindexed_engine_reports_initializing_and_not_fresh() -> Result<(), Box<dyn Error>> {
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = chakra_engine::WorkspaceEngine::new(identity);
    let envelope = engine.status(StatusRequest)?;
    assert_eq!(envelope.status, WorkspaceStatus::Initializing);
    assert_eq!(envelope.freshness, Freshness::Stale);
    assert_eq!(envelope.data.counts.symbols, 0);
    Ok(())
}
