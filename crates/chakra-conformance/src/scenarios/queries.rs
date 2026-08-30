//! Static-query scenarios: declarations, imports, roles, ambiguity,
//! callers, test hints, text search, budgets, and high-degree relations.

use chakra_domain::envelope::{TruncationCause, TruncationSection};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    CallersRequest, ContextRequest, MAX_QUERY_LIMIT, QueryError, QueryService, RepoMapRequest,
    SearchRequest, SymbolRef, SymbolSearchRequest,
};
use chakra_domain::source::SourceRole;
use chakra_domain::state::Freshness;
use chakra_domain::symbol::SymbolKind;

use super::{candidate, ensure_syntax_symbol, search_symbols, simple_name};
use crate::fixture::with_live;
use crate::manifest::Manifest;
use crate::runner::fixtures_root;
use crate::{Check, ensure, failure};

pub(super) fn declarations_containers(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        for expected in expectations
            .nested_containers
            .iter()
            .chain(std::iter::once(&expectations.nested_symbol))
        {
            let response = search_symbols(fixture, simple_name(&expected.qualified_name), None)?;
            let symbol = candidate(&response.data, &expected.qualified_name)?;
            ensure(
                symbol.kind == expected.kind,
                format!(
                    "`{}`: expected kind {:?}, got {:?}",
                    expected.qualified_name, expected.kind, symbol.kind
                ),
            )?;
            ensure_syntax_symbol(symbol)?;
        }
        let filtered = fixture.engine.symbol_search(SymbolSearchRequest {
            query: simple_name(&expectations.nested_symbol.qualified_name).to_owned(),
            namespace_prefix: Some(expectations.nested_prefix.clone()),
            limit: None,
            ..SymbolSearchRequest::default()
        })?;
        candidate(&filtered.data, &expectations.nested_symbol.qualified_name)?;
        Ok(vec![
            "container/nested declarations: tree_sitter provenance, syntax precision".to_owned(),
            "nested symbol reachable through namespace_prefix filtering".to_owned(),
        ])
    })
}

pub(super) fn imports_aliases(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        let response = fixture.engine.symbol_search(SymbolSearchRequest {
            query: expectations.import_alias.clone(),
            include_kinds: vec![SymbolKind::Import],
            limit: None,
            ..SymbolSearchRequest::default()
        })?;
        ensure(
            !response.data.candidates.is_empty(),
            format!(
                "no import fact matches alias `{}`",
                expectations.import_alias
            ),
        )?;
        for import in &response.data.candidates {
            ensure(
                import.kind == SymbolKind::Import,
                format!("`{}` is not an import fact", import.qualified_name),
            )?;
            ensure_syntax_symbol(import)?;
        }
        ensure(
            response.data.candidates.iter().any(|import| {
                import.name.contains(&expectations.import_alias)
                    || import.qualified_name.contains(&expectations.import_alias)
            }),
            format!(
                "alias `{}` not recorded in any import fact",
                expectations.import_alias
            ),
        )?;
        Ok(vec![
            "import/alias facts: tree_sitter provenance, syntax precision".to_owned(),
        ])
    })
}

pub(super) fn source_roles(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        let map = fixture.engine.repo_map(RepoMapRequest {
            include_project_scope: false,
            limit: Some(MAX_QUERY_LIMIT),
            ..RepoMapRequest::default()
        })?;
        ensure(
            map.freshness == Freshness::Fresh,
            "repo_map did not observe a fresh revision",
        )?;
        let role_of = |path: &str| -> Check<SourceRole> {
            let file = map
                .data
                .files
                .iter()
                .find(|file| file.path.as_str() == path)
                .ok_or_else(|| failure(format!("file `{path}` missing from repo_map")))?;
            ensure(
                file.provenance == Provenance::Git,
                format!(
                    "`{path}` inventory provenance is {:?}, not git",
                    file.provenance
                ),
            )?;
            Ok(file.source_role)
        };
        ensure(
            role_of(&expectations.production_file)? == SourceRole::Production,
            format!(
                "`{}` is not classified production",
                expectations.production_file
            ),
        )?;
        ensure(
            role_of(&expectations.test_file)? == SourceRole::Test,
            format!("`{}` is not classified test", expectations.test_file),
        )?;
        Ok(vec![
            "file inventory: git provenance; roles from ecosystem path conventions".to_owned(),
        ])
    })
}

pub(super) fn ambiguity(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        let response = search_symbols(fixture, &expectations.ambiguous_name, None)?;
        let mut found: Vec<&str> = response
            .data
            .candidates
            .iter()
            .filter(|symbol| symbol.name == expectations.ambiguous_name)
            .map(|symbol| symbol.qualified_name.as_str())
            .collect();
        found.sort_unstable();
        let mut expected: Vec<&str> = expectations
            .ambiguous_qualified
            .iter()
            .map(String::as_str)
            .collect();
        expected.sort_unstable();
        ensure(
            found == expected,
            format!(
                "ambiguous name `{}`: expected candidates {expected:?}, found {found:?}",
                expectations.ambiguous_name
            ),
        )?;
        for symbol in &response.data.candidates {
            ensure_syntax_symbol(symbol)?;
        }
        for (query, result) in [
            (
                "context",
                fixture
                    .engine
                    .context(ContextRequest {
                        source: Default::default(),
                        symbol: Some(SymbolRef::ByName(expectations.ambiguous_name.clone())),
                        ..ContextRequest::default()
                    })
                    .map(|_| ()),
            ),
            (
                "callers",
                fixture
                    .engine
                    .callers(CallersRequest {
                        source: Default::default(),
                        symbol: Some(SymbolRef::ByName(expectations.ambiguous_name.clone())),
                        ..CallersRequest::default()
                    })
                    .map(|_| ()),
            ),
        ] {
            match result {
                Err(QueryError::AmbiguousSymbol { candidates, .. }) => ensure(
                    candidates == expected.len(),
                    format!(
                        "{query} reported {candidates} candidates, expected {}",
                        expected.len()
                    ),
                )?,
                Ok(()) => {
                    return Err(failure(format!(
                        "{query} silently resolved the ambiguous name `{}`",
                        expectations.ambiguous_name
                    ))
                    .into());
                }
                Err(other) => {
                    return Err(failure(format!(
                        "{query} failed with an unexpected error: {other}"
                    ))
                    .into());
                }
            }
        }
        Ok(vec![
            "duplicate names: every candidate listed; ambiguity is a typed error, never guessed"
                .to_owned(),
        ])
    })
}

pub(super) fn syntax_callers(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        let callers = fixture.engine.callers(CallersRequest {
            source: Default::default(),
            symbol: Some(SymbolRef::ByName(expectations.callee.clone())),
            limit: None,
            ..CallersRequest::default()
        })?;
        ensure(
            callers.freshness == Freshness::Fresh,
            "callers did not observe a fresh revision",
        )?;
        ensure(
            callers.data.callers.len() == 1,
            format!(
                "`{}`: expected exactly one caller, found {:?}",
                expectations.callee,
                callers
                    .data
                    .callers
                    .iter()
                    .map(|caller| caller.symbol.qualified_name.clone())
                    .collect::<Vec<_>>()
            ),
        )?;
        let caller = &callers.data.callers[0];
        ensure(
            caller.symbol.qualified_name == expectations.caller,
            format!(
                "expected caller `{}`, found `{}`",
                expectations.caller, caller.symbol.qualified_name
            ),
        )?;
        ensure(
            caller.precision == Precision::Heuristic && caller.provenance == Provenance::TreeSitter,
            format!(
                "caller edge: expected tree_sitter/heuristic, got {:?}/{:?}",
                caller.provenance, caller.precision
            ),
        )?;
        let context = fixture.engine.context(ContextRequest {
            source: Default::default(),
            symbol: Some(SymbolRef::ByName(expectations.caller.clone())),
            ..ContextRequest::default()
        })?;
        ensure(
            context.data.callees.iter().any(|callee| {
                callee.symbol.qualified_name == expectations.callee
                    && callee.precision == Precision::Heuristic
            }),
            format!(
                "`{}` not listed as a callee of `{}`",
                expectations.callee, expectations.caller
            ),
        )?;
        if let Some(json) = &expectations.json_variant {
            // Mixed-syntax workspace (issue #86): the JSON-declared resource
            // is found by exact name with honest syntax quality, and the
            // native-syntax caller resolves to it across the encoding
            // boundary.
            let response = search_symbols(fixture, simple_name(&json.target), None)?;
            let target = candidate(&response.data, &json.target)?;
            ensure(
                target.kind == SymbolKind::Configuration
                    && target.provenance == Provenance::TreeSitter
                    && target.precision == Precision::Syntax,
                format!(
                    "JSON variant `{}`: expected tree_sitter/syntax configuration, got {:?}/{:?}/{:?}",
                    json.target, target.kind, target.provenance, target.precision
                ),
            )?;
            ensure(
                target.location.file().as_str() == json.file,
                format!(
                    "JSON variant `{}`: expected file `{}`, found `{}`",
                    json.target,
                    json.file,
                    target.location.file()
                ),
            )?;
            let callers = fixture.engine.callers(CallersRequest {
                source: Default::default(),
                symbol: Some(SymbolRef::ByName(simple_name(&json.target).to_owned())),
                limit: None,
                ..CallersRequest::default()
            })?;
            ensure(
                callers
                    .data
                    .callers
                    .iter()
                    .any(|caller| caller.symbol.qualified_name == json.caller),
                format!(
                    "JSON variant `{}`: expected native caller `{}`, found {:?}",
                    json.target,
                    json.caller,
                    callers
                        .data
                        .callers
                        .iter()
                        .map(|caller| caller.symbol.qualified_name.clone())
                        .collect::<Vec<_>>()
                ),
            )?;
        }
        Ok(vec![
            "call edges: tree_sitter provenance, heuristic precision, no precise provider"
                .to_owned(),
        ])
    })
}

pub(super) fn test_hints(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        let response = search_symbols(fixture, simple_name(&expectations.test_symbol), None)?;
        let test = candidate(&response.data, &expectations.test_symbol)?;
        ensure(
            test.kind == SymbolKind::Test,
            format!(
                "`{}`: expected test kind, got {:?}",
                test.qualified_name, test.kind
            ),
        )?;
        ensure(
            test.source_role == SourceRole::Test,
            format!("`{}`: expected test source role", test.qualified_name),
        )?;
        ensure_syntax_symbol(test)?;
        Ok(vec![
            "test hint: tree_sitter provenance, syntax precision, test source role".to_owned(),
        ])
    })
}

pub(super) fn text_search(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        let response = fixture.engine.search(SearchRequest {
            query: expectations.text_needle.clone(),
            case_sensitive: true,
            limit: None,
            ..SearchRequest::default()
        })?;
        ensure(
            response.freshness == Freshness::Fresh,
            "search did not observe a fresh revision",
        )?;
        ensure(
            response.data.matches.len() == 1,
            format!(
                "needle `{}`: expected exactly one match, found {}",
                expectations.text_needle,
                response.data.matches.len()
            ),
        )?;
        let matched = &response.data.matches[0];
        ensure(
            matched.file.as_str() == expectations.text_needle_file,
            format!("needle matched `{}`", matched.file.as_str()),
        )?;
        ensure(
            matched.provenance == Provenance::TextSearch && matched.precision == Precision::Textual,
            format!(
                "text hit: expected text_search/textual, got {:?}/{:?}",
                matched.provenance, matched.precision
            ),
        )?;
        Ok(vec![
            "text hits: text_search provenance, textual precision".to_owned(),
        ])
    })
}

pub(super) fn bounded_responses(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let full = fixture.engine.search(SearchRequest {
            query: "conformance".to_owned(),
            limit: Some(MAX_QUERY_LIMIT),
            ..SearchRequest::default()
        })?;
        ensure(
            full.data.matches.len() > 1,
            "fixture must contain more than one `conformance` match",
        )?;
        let limited = fixture.engine.search(SearchRequest {
            query: "conformance".to_owned(),
            limit: Some(1),
            ..SearchRequest::default()
        })?;
        ensure(limited.data.matches.len() == 1, "limit was not applied")?;
        ensure(
            limited.truncated,
            "truncated flag missing on a limited query",
        )?;
        ensure(
            limited.truncation.iter().any(|detail| {
                detail.section == TruncationSection::SearchMatches
                    && detail.cause == TruncationCause::ItemLimit
                    && detail.limit == 1
            }),
            format!(
                "explicit truncation detail missing: {:?}",
                limited.truncation
            ),
        )?;
        Ok(vec![
            "limit exceedance: truncated flag plus section/cause detail".to_owned(),
        ])
    })
}

pub(super) fn high_degree_callers(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        let full = fixture.engine.callers(CallersRequest {
            source: Default::default(),
            symbol: Some(SymbolRef::ByName(expectations.fan_in_target.clone())),
            limit: Some(MAX_QUERY_LIMIT),
            ..CallersRequest::default()
        })?;
        ensure(
            full.data.callers.len() == expectations.fan_in_callers,
            format!(
                "`{}`: expected {} callers, found {}",
                expectations.fan_in_target,
                expectations.fan_in_callers,
                full.data.callers.len()
            ),
        )?;
        let occurrences: u64 = full
            .data
            .callers
            .iter()
            .map(|caller| caller.occurrence_count)
            .sum();
        ensure(
            occurrences == expectations.fan_in_callers as u64,
            format!(
                "expected {} call occurrences, found {occurrences}",
                expectations.fan_in_callers
            ),
        )?;
        for caller in &full.data.callers {
            ensure(
                caller.precision == Precision::Heuristic
                    && caller.provenance == Provenance::TreeSitter,
                format!(
                    "caller `{}`: expected tree_sitter/heuristic",
                    caller.symbol.qualified_name
                ),
            )?;
        }
        ensure(!full.truncated, "full callers response must not truncate")?;

        let paged = fixture.engine.callers(CallersRequest {
            source: Default::default(),
            symbol: Some(SymbolRef::ByName(expectations.fan_in_target.clone())),
            limit: None,
            ..CallersRequest::default()
        })?;
        ensure(
            paged.data.callers.len() < expectations.fan_in_callers,
            "default limit must bound the callers response",
        )?;
        ensure(
            paged.truncated,
            "default-limited callers must report truncation",
        )?;
        ensure(
            paged.truncation.iter().any(|detail| {
                detail.section == TruncationSection::CallersCallers
                    && detail.cause == TruncationCause::ItemLimit
            }),
            format!("callers truncation detail missing: {:?}", paged.truncation),
        )?;
        Ok(vec![format!(
            "{} call sites: complete heuristic callers at an explicit limit; default limit truncates explicitly",
            expectations.fan_in_callers
        )])
    })
}
