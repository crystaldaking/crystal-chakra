//! [`QueryService`] implementation over the published snapshot.
//!
//! Each method pins one `Arc<WorkspaceSnapshot>` up front, so a query always
//! observes exactly one revision (SPEC §5). Freshness is the snapshot's own
//! axis, claimed by the publisher — never inferred from the lifecycle
//! status. A request's [`FreshnessRequirement`] is enforced: when the pinned
//! snapshot does not satisfy it, the call fails with
//! [`QueryError::FreshnessNotMet`] instead of silently serving stale data.

use std::sync::Arc;

use chakra_domain::envelope::QueryEnvelope;
use chakra_domain::location::{SourceRange, TextPosition};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    CallersData, CallersRequest, ContextData, ContextRequest, DEFAULT_QUERY_LIMIT, DiffContextData,
    DiffContextRequest, FileSummary, IndexCounts, MAX_QUERY_LIMIT, ProviderInfo, QueryError,
    QueryService, RelatedSymbol, RepoMapData, RepoMapRequest, SearchData, SearchRequest,
    SourceSnippet, StatusData, StatusRequest, SymbolRef, SymbolSearchData, SymbolSearchRequest,
    SymbolView, TextMatch,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::{Freshness, FreshnessRequirement, ProviderState};
use chakra_domain::symbol::{Edge, EdgeKind, Symbol};

use crate::engine::{WorkspaceEngine, WorkspaceSnapshot};
use crate::graph::SymbolGraph;
use crate::precise::{
    CallHierarchyDirections, PreciseQueryRequest, PreciseRelation, ProviderSymbol,
    ProviderWorkspace,
};

const MAX_QUERY_PATTERN_CHARS: usize = 1_024;
const MAX_MATCH_LINE_CHARS: usize = 512;
const MAX_SNIPPET_LINES: usize = 20;
const MAX_SNIPPET_CHARS: usize = 4_096;
const MAX_FRESH_SNAPSHOT_ATTEMPTS: usize = 3;

/// Applies the SPEC §29 budget: default when absent, hard cap always.
fn clamp_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(DEFAULT_QUERY_LIMIT).min(MAX_QUERY_LIMIT) as usize
}

/// Truncates to the budget; the bool is the envelope `truncated` flag.
fn bounded<T>(mut items: Vec<T>, limit: usize) -> (Vec<T>, bool) {
    let truncated = items.len() > limit;
    items.truncate(limit);
    (items, truncated)
}

/// SPEC §24 resolution: ids are exact and revision-scoped; names resolve
/// only when unambiguous.
fn resolve<'a>(
    graph: &'a SymbolGraph,
    reference: &SymbolRef,
    current_revision: Revision,
) -> Result<&'a Symbol, QueryError> {
    match reference {
        SymbolRef::ById { id, revision } => {
            // An EntityId is an arena index: it is only meaningful within
            // the revision it was taken from. Refuse to silently resolve a
            // stale reference against a newer graph.
            if *revision != current_revision {
                return Err(QueryError::StaleSymbolRef {
                    reference_revision: *revision,
                    current_revision,
                });
            }
            graph
                .symbol(*id)
                .ok_or_else(|| QueryError::SymbolNotFound(format!("{id:?}")))
        }
        SymbolRef::ByName(name) => {
            let matches = graph.resolve_name(name);
            match matches.len() {
                0 => Err(QueryError::SymbolNotFound(name.clone())),
                1 => matches
                    .first()
                    .and_then(|id| graph.symbol(*id))
                    .ok_or_else(|| QueryError::SymbolNotFound(name.clone())),
                n => Err(QueryError::AmbiguousSymbol {
                    query: name.clone(),
                    candidates: n,
                }),
            }
        }
    }
}

fn related(
    graph: &SymbolGraph,
    edge: &Edge,
    other: chakra_domain::symbol::EntityId,
) -> Option<RelatedSymbol> {
    graph.symbol(other).map(|symbol| RelatedSymbol {
        symbol: SymbolView::from(symbol),
        edge_kind: edge.kind,
        provenance: edge.provenance,
        precision: edge.precision,
        location: edge.location.clone(),
    })
}

fn sort_related(items: &mut [RelatedSymbol]) {
    items.sort_by(|a, b| {
        a.symbol
            .qualified_name
            .cmp(&b.symbol.qualified_name)
            .then(a.symbol.id.cmp(&b.symbol.id))
    });
}

fn provider_state_for(engine: &WorkspaceEngine, snapshot: &WorkspaceSnapshot) -> ProviderState {
    engine
        .precise_provider()
        .map_or(snapshot.provider_state(), |provider| {
            provider.state_for(snapshot.revision())
        })
}

fn precise_related(graph: &SymbolGraph, relation: PreciseRelation) -> Option<RelatedSymbol> {
    let position = relation.declaration.start();
    let symbol = graph
        .symbols()
        .iter()
        .filter(|symbol| {
            symbol.name() == relation.name
                && symbol.location.file() == relation.declaration.file()
                && symbol.location.start() <= position
                && position < symbol.location.end()
        })
        .min_by_key(|symbol| {
            (
                symbol.location.end().line() - symbol.location.start().line(),
                symbol
                    .location
                    .end()
                    .column()
                    .abs_diff(symbol.location.start().column()),
            )
        })?;
    Some(RelatedSymbol {
        symbol: SymbolView::from(symbol),
        edge_kind: EdgeKind::Calls,
        provenance: relation.provenance,
        precision: Precision::Precise,
        location: relation.call_site,
    })
}

/// Precise relations win for the same revision-scoped entity; unmatched
/// syntax candidates remain visible with their original lower precision.
fn merge_precise(
    graph: &SymbolGraph,
    syntax: Vec<RelatedSymbol>,
    precise: Vec<PreciseRelation>,
) -> Vec<RelatedSymbol> {
    let mut merged: Vec<_> = precise
        .into_iter()
        .filter_map(|relation| precise_related(graph, relation))
        .collect();
    sort_related(&mut merged);
    merged.dedup_by_key(|item| item.symbol.id);
    let precise_ids: std::collections::HashSet<_> =
        merged.iter().map(|item| item.symbol.id).collect();
    merged.extend(
        syntax
            .into_iter()
            .filter(|item| !precise_ids.contains(&item.symbol.id)),
    );
    sort_related(&mut merged);
    merged
}

/// Gate for every query with a freshness requirement: the pinned snapshot
/// either satisfies it or the call fails with a typed error. Cheaper request
/// validation (empty queries, missing refs) runs before this gate.
fn enforce_freshness(
    requirement: FreshnessRequirement,
    snapshot: &WorkspaceSnapshot,
) -> Result<(), QueryError> {
    if requirement.is_satisfied_by(snapshot.freshness()) {
        Ok(())
    } else {
        Err(QueryError::FreshnessNotMet {
            required: requirement,
            actual: snapshot.freshness(),
        })
    }
}

fn query_snapshot(
    engine: &WorkspaceEngine,
    requirement: FreshnessRequirement,
) -> Result<Arc<WorkspaceSnapshot>, QueryError> {
    if requirement == FreshnessRequirement::AllowStale {
        return Ok(engine.snapshot());
    }
    for _ in 0..MAX_FRESH_SNAPSHOT_ATTEMPTS {
        engine
            .require_fresh()
            .map_err(|error| QueryError::FreshnessUnavailable(error.to_string()))?;
        let snapshot = engine.snapshot();
        if requirement.is_satisfied_by(snapshot.freshness()) {
            return Ok(snapshot);
        }
    }
    let snapshot = engine.snapshot();
    enforce_freshness(requirement, &snapshot)?;
    Ok(snapshot)
}

fn envelope<T>(
    snapshot: &WorkspaceSnapshot,
    provider_state: ProviderState,
    truncated: bool,
    data: T,
) -> QueryEnvelope<T> {
    QueryEnvelope::new(
        snapshot.identity().workspace.clone(),
        snapshot.revision(),
        snapshot.freshness(),
        snapshot.status(),
        provider_state,
        truncated,
        data,
    )
}

fn bounded_match_line(line: &str, match_start: usize, match_end: usize) -> (String, bool) {
    if line.chars().count() <= MAX_MATCH_LINE_CHARS {
        return (line.to_owned(), false);
    }

    let match_start_char = line[..match_start].chars().count();
    let match_end_char = line[..match_end].chars().count();
    let match_chars = match_end_char.saturating_sub(match_start_char);
    let surrounding = MAX_MATCH_LINE_CHARS.saturating_sub(match_chars);
    let start_char = match_start_char.saturating_sub(surrounding / 2);
    let end_char = (start_char + MAX_MATCH_LINE_CHARS).max(match_end_char);
    let total_chars = line.chars().count();
    let end_char = end_char.min(total_chars);
    let start_char = end_char.saturating_sub(MAX_MATCH_LINE_CHARS);
    let snippet: String = line
        .chars()
        .skip(start_char)
        .take(end_char - start_char)
        .collect();
    (snippet, true)
}

fn position_byte_offset(source: &str, position: TextPosition) -> Option<usize> {
    let line_index = usize::try_from(position.line().checked_sub(1)?).ok()?;
    let column_index = usize::try_from(position.column().checked_sub(1)?).ok()?;
    let line_start = if line_index == 0 {
        0
    } else {
        source
            .match_indices('\n')
            .nth(line_index - 1)
            .map(|(offset, _)| offset + 1)?
    };
    let line = source.get(line_start..)?.split('\n').next()?;
    let column_byte = if column_index == line.chars().count() {
        line.len()
    } else {
        line.char_indices()
            .nth(column_index)
            .map(|(offset, _)| offset)?
    };
    Some(line_start + column_byte)
}

fn advance_position(start: TextPosition, text: &str) -> Option<TextPosition> {
    let mut line = start.line();
    let mut column = start.column();
    for character in text.chars() {
        if character == '\n' {
            line = line.checked_add(1)?;
            column = 1;
        } else {
            column = column.checked_add(1)?;
        }
    }
    TextPosition::new(line, column).ok()
}

fn source_snippet(graph: &SymbolGraph, symbol: &Symbol) -> Option<SourceSnippet> {
    let range = &symbol.location;
    let source = graph.file_source(range.file())?;
    let start_byte = position_byte_offset(source, range.start())?;
    let end_byte = position_byte_offset(source, range.end())?;
    let full = source.get(start_byte..end_byte)?;

    let mut end = full.len();
    let mut lines = 1_usize;
    for (chars, (offset, character)) in full.char_indices().enumerate() {
        if chars >= MAX_SNIPPET_CHARS || (character == '\n' && lines >= MAX_SNIPPET_LINES) {
            end = offset;
            break;
        }
        if character == '\n' {
            lines += 1;
        }
    }
    let text = full.get(..end)?.to_owned();
    let truncated = end < full.len();
    let snippet_end = advance_position(range.start(), &text)?;
    let snippet_range = SourceRange::new(range.file().clone(), range.start(), snippet_end).ok()?;
    Some(SourceSnippet {
        range: snippet_range,
        text,
        truncated,
        provenance: symbol.provenance,
        precision: symbol.precision,
    })
}

impl QueryService for WorkspaceEngine {
    fn status(&self, _request: StatusRequest) -> Result<QueryEnvelope<StatusData>, QueryError> {
        let snapshot = self.snapshot();
        let provider_state = provider_state_for(self, &snapshot);
        let counts = IndexCounts {
            files: snapshot.graph().file_count(),
            symbols: snapshot.graph().symbol_count(),
            edges: snapshot.graph().edge_count(),
        };
        let providers = vec![ProviderInfo {
            name: "rust-analyzer".to_owned(),
            state: provider_state,
        }];
        let data = StatusData {
            workspace: snapshot.identity().clone(),
            counts,
            providers,
        };
        Ok(envelope(&snapshot, provider_state, false, data))
    }

    fn repo_map(&self, request: RepoMapRequest) -> Result<QueryEnvelope<RepoMapData>, QueryError> {
        let snapshot = query_snapshot(self, request.freshness)?;
        let summaries = snapshot
            .graph()
            .file_summaries()
            .into_iter()
            .map(|(path, count, provenance, precision)| FileSummary {
                path,
                symbol_count: count,
                provenance,
                precision,
            })
            .collect();
        let (files, truncated) = bounded(summaries, clamp_limit(request.limit));
        Ok(envelope(
            &snapshot,
            provider_state_for(self, &snapshot),
            truncated,
            RepoMapData { files },
        ))
    }

    fn search(&self, request: SearchRequest) -> Result<QueryEnvelope<SearchData>, QueryError> {
        if request.query.is_empty() {
            return Err(QueryError::Invalid("query must be non-empty".to_owned()));
        }
        if request.query.chars().count() > MAX_QUERY_PATTERN_CHARS {
            return Err(QueryError::Invalid(format!(
                "query exceeds the {MAX_QUERY_PATTERN_CHARS}-character pattern budget"
            )));
        }
        let snapshot = query_snapshot(self, request.freshness)?;
        let pattern = if request.regex {
            request.query.clone()
        } else {
            regex::escape(&request.query)
        };
        let matcher = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!request.case_sensitive)
            .build()
            .map_err(|error| QueryError::Invalid(format!("invalid regex: {error}")))?;
        let limit = clamp_limit(request.limit);
        let mut matches = Vec::new();
        let mut truncated = false;

        'files: for (path, source) in snapshot.graph().source_files() {
            for (line_index, line) in source.lines().enumerate() {
                for found in matcher.find_iter(line) {
                    if matches.len() >= limit {
                        truncated = true;
                        break 'files;
                    }
                    let line_number = u32::try_from(line_index + 1)
                        .map_err(|_| QueryError::Invalid("source has too many lines".to_owned()))?;
                    let start_column = u32::try_from(line[..found.start()].chars().count() + 1)
                        .map_err(|_| QueryError::Invalid("source line is too long".to_owned()))?;
                    let end_column = u32::try_from(line[..found.end()].chars().count() + 1)
                        .map_err(|_| QueryError::Invalid("source line is too long".to_owned()))?;
                    let range = SourceRange::new(
                        path.clone(),
                        TextPosition::new(line_number, start_column)
                            .map_err(|error| QueryError::Invalid(error.to_string()))?,
                        TextPosition::new(line_number, end_column)
                            .map_err(|error| QueryError::Invalid(error.to_string()))?,
                    )
                    .map_err(|error| QueryError::Invalid(error.to_string()))?;
                    let (line, line_truncated) =
                        bounded_match_line(line, found.start(), found.end());
                    truncated |= line_truncated;
                    matches.push(TextMatch {
                        file: path.clone(),
                        range,
                        line,
                        line_truncated,
                        provenance: Provenance::TextSearch,
                        precision: Precision::Textual,
                    });
                }
            }
        }
        Ok(envelope(
            &snapshot,
            provider_state_for(self, &snapshot),
            truncated,
            SearchData { matches },
        ))
    }

    fn symbol_search(
        &self,
        request: SymbolSearchRequest,
    ) -> Result<QueryEnvelope<SymbolSearchData>, QueryError> {
        let query = request.query.trim();
        if query.is_empty() {
            return Err(QueryError::Invalid("query must be non-empty".to_owned()));
        }
        if query.chars().count() > MAX_QUERY_PATTERN_CHARS {
            return Err(QueryError::Invalid(format!(
                "query exceeds the {MAX_QUERY_PATTERN_CHARS}-character pattern budget"
            )));
        }
        let snapshot = query_snapshot(self, request.freshness)?;
        let limit = clamp_limit(request.limit);
        let (matches, truncated) = snapshot.graph().search_names(query, limit);
        let mut candidates: Vec<SymbolView> = matches
            .into_iter()
            .filter_map(|id| snapshot.graph().symbol(id))
            .map(SymbolView::from)
            .collect();
        candidates.sort_by(|a, b| {
            a.qualified_name
                .cmp(&b.qualified_name)
                .then(a.id.cmp(&b.id))
        });
        let data = SymbolSearchData {
            query: query.to_owned(),
            candidates,
        };
        Ok(envelope(
            &snapshot,
            provider_state_for(self, &snapshot),
            truncated,
            data,
        ))
    }

    fn context(&self, request: ContextRequest) -> Result<QueryEnvelope<ContextData>, QueryError> {
        let reference = request
            .symbol
            .as_ref()
            .ok_or(QueryError::MissingSymbolRef)?;
        let snapshot = query_snapshot(self, request.freshness)?;
        let graph = snapshot.graph();
        let symbol = resolve(graph, reference, snapshot.revision())?;
        let limit = clamp_limit(request.limit);

        let mut callers: Vec<RelatedSymbol> = graph
            .incoming_edges(symbol.id)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .filter_map(|edge| related(graph, edge, edge.from))
            .collect();

        let mut callees: Vec<RelatedSymbol> = graph
            .outgoing_edges(symbol.id)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .filter_map(|edge| related(graph, edge, edge.to))
            .collect();
        let mut provider_state = provider_state_for(self, &snapshot);
        let mut provider_truncated = false;
        if snapshot.freshness() == Freshness::Fresh
            && let Some(provider) = self.precise_provider()
        {
            let result = provider.enrich(PreciseQueryRequest {
                workspace: ProviderWorkspace::from_snapshot(&snapshot),
                symbol: ProviderSymbol {
                    name: symbol.name().to_owned(),
                    declaration: symbol.location.clone(),
                },
                directions: CallHierarchyDirections {
                    incoming: true,
                    outgoing: true,
                },
                limit,
            });
            provider_state = if result.revision == snapshot.revision() {
                result.state
            } else {
                ProviderState::CatchingUp
            };
            if provider_state == ProviderState::Ready {
                callers = merge_precise(graph, callers, result.incoming);
                callees = merge_precise(graph, callees, result.outgoing);
                provider_truncated = result.truncated;
            }
        }
        sort_related(&mut callers);
        sort_related(&mut callees);
        let (callers, callers_truncated) = bounded(callers, limit);
        let (callees, callees_truncated) = bounded(callees, limit);

        let mut implementations: Vec<RelatedSymbol> = graph
            .incoming_edges(symbol.id)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Implements)
            .filter_map(|edge| related(graph, edge, edge.from))
            .collect();
        sort_related(&mut implementations);
        let (implementations, implementations_truncated) = bounded(implementations, limit);

        let mut tests: Vec<RelatedSymbol> = graph
            .incoming_edges(symbol.id)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Tests)
            .filter_map(|edge| related(graph, edge, edge.from))
            .collect();
        sort_related(&mut tests);
        let (tests, tests_truncated) = bounded(tests, limit);

        let mut related_files: Vec<chakra_domain::location::RepoRelativePath> = callers
            .iter()
            .chain(callees.iter())
            .chain(implementations.iter())
            .chain(tests.iter())
            .map(|item| item.symbol.location.file().clone())
            .collect();
        related_files.sort();
        related_files.dedup();
        let (related_files, files_truncated) = bounded(related_files, limit);

        let truncated = callers_truncated
            || callees_truncated
            || implementations_truncated
            || tests_truncated
            || files_truncated
            || provider_truncated;
        let data = ContextData {
            symbol: SymbolView::from(symbol),
            source: source_snippet(graph, symbol),
            callers,
            callees,
            implementations,
            tests,
            related_files,
        };
        Ok(envelope(&snapshot, provider_state, truncated, data))
    }

    fn callers(&self, request: CallersRequest) -> Result<QueryEnvelope<CallersData>, QueryError> {
        let reference = request
            .symbol
            .as_ref()
            .ok_or(QueryError::MissingSymbolRef)?;
        let snapshot = query_snapshot(self, request.freshness)?;
        let graph = snapshot.graph();
        let target = resolve(graph, reference, snapshot.revision())?;
        let mut callers: Vec<RelatedSymbol> = graph
            .incoming_edges(target.id)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .filter_map(|edge| related(graph, edge, edge.from))
            .collect();
        let limit = clamp_limit(request.limit);
        let mut provider_state = provider_state_for(self, &snapshot);
        let mut provider_truncated = false;
        if snapshot.freshness() == Freshness::Fresh
            && let Some(provider) = self.precise_provider()
        {
            let result = provider.enrich(PreciseQueryRequest {
                workspace: ProviderWorkspace::from_snapshot(&snapshot),
                symbol: ProviderSymbol {
                    name: target.name().to_owned(),
                    declaration: target.location.clone(),
                },
                directions: CallHierarchyDirections {
                    incoming: true,
                    outgoing: false,
                },
                limit,
            });
            provider_state = if result.revision == snapshot.revision() {
                result.state
            } else {
                ProviderState::CatchingUp
            };
            if provider_state == ProviderState::Ready {
                callers = merge_precise(graph, callers, result.incoming);
                provider_truncated = result.truncated;
            }
        }
        sort_related(&mut callers);
        let (callers, truncated) = bounded(callers, limit);
        let data = CallersData {
            target: SymbolView::from(target),
            callers,
        };
        Ok(envelope(
            &snapshot,
            provider_state,
            truncated || provider_truncated,
            data,
        ))
    }

    fn diff_context(
        &self,
        _request: DiffContextRequest,
    ) -> Result<QueryEnvelope<DiffContextData>, QueryError> {
        // Needs the Git subsystem; arrives with diff awareness.
        Err(QueryError::Unsupported("diff_context"))
    }
}
