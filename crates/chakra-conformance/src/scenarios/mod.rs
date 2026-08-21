//! The shared conformance scenario catalog.
//!
//! Every scenario runs against a fresh live workspace seeded from the
//! language's fixture directory and asserts through the public
//! `QueryService` surface only. A scenario returns the provenance assertions
//! it performed; any expectation failure is an error the runner converts
//! into a `fail` report.

mod diff;
mod live;
mod providers;
mod queries;

use chakra_domain::envelope::QueryEnvelope;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{QueryService, SymbolSearchData, SymbolSearchRequest, SymbolView};
use chakra_domain::state::Freshness;

use crate::fixture::LiveFixture;
use crate::manifest::Manifest;
use crate::{Check, ensure, failure};

/// One implemented scenario.
pub(crate) struct ScenarioDef {
    pub id: &'static str,
    pub run: fn(&Manifest) -> Check<Vec<String>>,
}

/// The fixed catalog, in emitted-report order. Manifests must declare
/// exactly these ids (enforced by `validate_manifest`).
pub(crate) const SCENARIOS: &[ScenarioDef] = &[
    ScenarioDef {
        id: "declarations-containers",
        run: queries::declarations_containers,
    },
    ScenarioDef {
        id: "imports-aliases",
        run: queries::imports_aliases,
    },
    ScenarioDef {
        id: "source-roles",
        run: queries::source_roles,
    },
    ScenarioDef {
        id: "ambiguity",
        run: queries::ambiguity,
    },
    ScenarioDef {
        id: "syntax-callers",
        run: queries::syntax_callers,
    },
    ScenarioDef {
        id: "test-hints",
        run: queries::test_hints,
    },
    ScenarioDef {
        id: "text-search",
        run: queries::text_search,
    },
    ScenarioDef {
        id: "bounded-responses",
        run: queries::bounded_responses,
    },
    ScenarioDef {
        id: "syntax-error-recovery",
        run: live::syntax_error_recovery,
    },
    ScenarioDef {
        id: "file-lifecycle",
        run: live::file_lifecycle,
    },
    ScenarioDef {
        id: "diff-context-scopes",
        run: diff::diff_context_scopes,
    },
    ScenarioDef {
        id: "provider-absent-degradation",
        run: providers::provider_absent_degradation,
    },
    ScenarioDef {
        id: "provider-crash-recovery",
        run: providers::provider_crash_recovery,
    },
    ScenarioDef {
        id: "high-degree-callers",
        run: queries::high_degree_callers,
    },
];

/// Simple (last) segment of a `::`-separated qualified name.
fn simple_name(qualified_name: &str) -> &str {
    qualified_name.rsplit("::").next().unwrap_or(qualified_name)
}

/// Fresh symbol search; doubles as a live-index freshness barrier.
fn search_symbols(
    fixture: &LiveFixture,
    query: &str,
    limit: Option<u32>,
) -> Check<QueryEnvelope<SymbolSearchData>> {
    let response = fixture.engine.symbol_search(SymbolSearchRequest {
        query: query.to_owned(),
        limit,
        ..SymbolSearchRequest::default()
    })?;
    ensure(
        response.freshness == Freshness::Fresh,
        format!("symbol_search `{query}` did not observe a fresh revision"),
    )?;
    Ok(response)
}

/// Finds one candidate by exact qualified name.
fn candidate<'a>(data: &'a SymbolSearchData, qualified_name: &str) -> Check<&'a SymbolView> {
    data.candidates
        .iter()
        .find(|candidate| candidate.qualified_name == qualified_name)
        .ok_or_else(|| failure(format!("symbol `{qualified_name}` not found")).into())
}

/// Asserts the syntax-intelligence provenance contract (PROV-01).
fn ensure_syntax_symbol(symbol: &SymbolView) -> Check<()> {
    ensure(
        symbol.provenance == Provenance::TreeSitter && symbol.precision == Precision::Syntax,
        format!(
            "`{}`: expected tree_sitter/syntax, got {:?}/{:?}",
            symbol.qualified_name, symbol.provenance, symbol.precision
        ),
    )?;
    Ok(())
}
