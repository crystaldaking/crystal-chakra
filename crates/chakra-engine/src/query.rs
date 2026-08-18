//! [`QueryService`] implementation over the published snapshot.
//!
//! Each method pins one `Arc<WorkspaceSnapshot>` up front, so a query always
//! observes exactly one revision (SPEC §5). Freshness is the snapshot's own
//! axis, claimed by the publisher — never inferred from the lifecycle
//! status. A request's [`FreshnessRequirement`] is enforced: when the pinned
//! snapshot does not satisfy it, the call fails with
//! [`QueryError::FreshnessNotMet`] instead of silently serving stale data.

use std::collections::BTreeMap;
use std::sync::Arc;

use chakra_domain::envelope::QueryEnvelope;
use chakra_domain::location::{SourceRange, TextPosition};
use chakra_domain::operation::OperationContext;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    CallSiteView, CallersData, CallersRequest, ChangedFile, ChangedSymbol, ChangedSymbolBasis,
    ContextData, ContextRequest, DEFAULT_QUERY_LIMIT, DiffCallSite, DiffContextData,
    DiffContextRequest, DiffRelatedSymbol, FileSummary, IndexCounts, MAX_QUERY_LIMIT,
    ProviderCapability, ProviderInfo, QueryError, QueryService, RelatedSymbol, RepoMapData,
    RepoMapRequest, SearchData, SearchRequest, SourceSnippet, StatusData, StatusRequest, SymbolRef,
    SymbolSearchData, SymbolSearchRequest, SymbolView, TextMatch,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::{Freshness, FreshnessRequirement, ProviderState};
use chakra_domain::symbol::{CallResolution, CallSite, Edge, EdgeKind, EntityId, Symbol};

use crate::engine::{WorkspaceEngine, WorkspaceSnapshot};
use crate::graph::SymbolGraph;
use crate::precise::{
    CallHierarchyDirections, PreciseQueryRequest, PreciseRelation, ProviderSymbol,
    ProviderWorkspace,
};
use crate::{DiffWorkspace, WorkspaceDiff};

const MAX_QUERY_PATTERN_CHARS: usize = 1_024;
const MAX_MATCH_LINE_CHARS: usize = 512;
const MAX_SNIPPET_LINES: usize = 20;
const MAX_SNIPPET_CHARS: usize = 4_096;
const MAX_FRESH_SNAPSHOT_ATTEMPTS: usize = 3;
const CANCELLATION_POLL_WORK_ITEMS: usize = 256;

struct CancellationPoll {
    remaining: usize,
}

impl CancellationPoll {
    fn new() -> Self {
        Self { remaining: 0 }
    }

    fn observe(&mut self, operation: &OperationContext) -> Result<(), QueryError> {
        if self.remaining == 0 {
            operation.check()?;
            self.remaining = CANCELLATION_POLL_WORK_ITEMS;
        }
        self.remaining -= 1;
        Ok(())
    }
}

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
    operation: &OperationContext,
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
            let matches = graph.resolve_name_with_context(name, operation)?;
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

fn collect_related(
    graph: &SymbolGraph,
    edges: &[Edge],
    kind: EdgeKind,
    incoming: bool,
    operation: &OperationContext,
) -> Result<Vec<RelatedSymbol>, QueryError> {
    let mut items = Vec::new();
    let mut poll = CancellationPoll::new();
    for edge in edges {
        poll.observe(operation)?;
        if edge.kind != kind {
            continue;
        }
        let other = if incoming { edge.from } else { edge.to };
        if let Some(item) = related(graph, edge, other) {
            items.push(item);
        }
    }
    Ok(items)
}

fn sort_related(items: &mut [RelatedSymbol]) {
    items.sort_by(|a, b| {
        a.symbol
            .qualified_name
            .cmp(&b.symbol.qualified_name)
            .then(a.symbol.id.cmp(&b.symbol.id))
    });
}

fn sort_diff_related(items: &mut [DiffRelatedSymbol]) {
    items.sort_by(|a, b| {
        a.relation
            .symbol
            .qualified_name
            .cmp(&b.relation.symbol.qualified_name)
            .then(a.changed_symbol_id.cmp(&b.changed_symbol_id))
            .then(a.relation.symbol.id.cmp(&b.relation.symbol.id))
    });
}

fn call_site_view(
    graph: &SymbolGraph,
    call_site: &CallSite,
    candidate_target: Option<&Symbol>,
) -> Option<CallSiteView> {
    let caller = graph.symbol(call_site.caller)?;
    Some(CallSiteView {
        caller: SymbolView::from(caller),
        candidate_target: candidate_target.map(SymbolView::from),
        form: call_site.form,
        target_kind: call_site.target_kind,
        name: call_site.name.clone(),
        qualifier: call_site.qualifier.clone(),
        receiver_hint: call_site.receiver_hint.clone(),
        location: call_site.location.clone(),
        resolution: call_site.resolution.clone(),
        provenance: call_site.provenance,
        precision: if candidate_target.is_some() {
            Precision::Heuristic
        } else {
            call_site.precision
        },
    })
}

fn sort_call_sites(items: &mut [CallSiteView]) {
    items.sort_by(|a, b| {
        a.caller
            .qualified_name
            .cmp(&b.caller.qualified_name)
            .then_with(|| {
                a.candidate_target
                    .as_ref()
                    .map(|target| (&target.qualified_name, target.id))
                    .cmp(
                        &b.candidate_target
                            .as_ref()
                            .map(|target| (&target.qualified_name, target.id)),
                    )
            })
            .then(a.location.file().cmp(b.location.file()))
            .then(a.location.start().line().cmp(&b.location.start().line()))
            .then(
                a.location
                    .start()
                    .column()
                    .cmp(&b.location.start().column()),
            )
    });
}

fn outgoing_call_candidates(
    graph: &SymbolGraph,
    caller: EntityId,
    limit: usize,
    operation: &OperationContext,
) -> Result<(Vec<CallSiteView>, bool), QueryError> {
    let capacity = limit.saturating_add(1);
    let mut items = Vec::with_capacity(capacity);
    let mut truncated = false;
    let mut poll = CancellationPoll::new();
    for call_site in graph.call_sites_from(caller) {
        poll.observe(operation)?;
        match call_site.resolution {
            CallResolution::Resolved { .. } => continue,
            CallResolution::Unresolved => {
                if let Some(view) = call_site_view(graph, call_site, None) {
                    items.push(view);
                }
            }
            CallResolution::Ambiguous { .. } => {
                let remaining = capacity.saturating_sub(items.len());
                if remaining == 0 {
                    truncated = true;
                    break;
                }
                let (candidates, candidate_truncated) = graph.call_candidates(call_site, remaining);
                truncated |= candidate_truncated;
                items.extend(
                    candidates
                        .into_iter()
                        .filter_map(|target| call_site_view(graph, call_site, Some(target))),
                );
            }
        }
        if items.len() >= capacity {
            truncated = true;
            break;
        }
    }
    sort_call_sites(&mut items);
    truncated |= items.len() > limit;
    items.truncate(limit);
    Ok((items, truncated))
}

fn incoming_call_candidates(
    graph: &SymbolGraph,
    target: EntityId,
    limit: usize,
    operation: &OperationContext,
) -> Result<(Vec<CallSiteView>, bool), QueryError> {
    operation.check()?;
    let (call_sites, mut truncated) = graph.call_sites_for_target(target, limit.saturating_add(1));
    let target = graph.symbol(target);
    let mut items: Vec<_> = call_sites
        .into_iter()
        .filter_map(|call_site| call_site_view(graph, call_site, target))
        .collect();
    sort_call_sites(&mut items);
    truncated |= items.len() > limit;
    items.truncate(limit);
    operation.check()?;
    Ok((items, truncated))
}

fn provider_state_for(engine: &WorkspaceEngine, snapshot: &WorkspaceSnapshot) -> ProviderState {
    engine
        .precise_provider()
        .map_or(snapshot.provider_state(), |provider| {
            provider.state_for(snapshot.revision())
        })
}

fn provider_state_for_language(
    engine: &WorkspaceEngine,
    snapshot: &WorkspaceSnapshot,
    language: chakra_domain::symbol::Language,
) -> ProviderState {
    engine
        .precise_provider()
        .filter(|provider| provider.supports(language))
        .map_or(ProviderState::NotConfigured, |provider| {
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
    operation: &OperationContext,
) -> Result<Arc<WorkspaceSnapshot>, QueryError> {
    operation.check()?;
    if requirement == FreshnessRequirement::AllowStale {
        return Ok(engine.snapshot());
    }
    for _ in 0..MAX_FRESH_SNAPSHOT_ATTEMPTS {
        engine
            .require_fresh_with_context(operation)
            .map_err(|error| {
                operation.check().map_or_else(QueryError::from, |_| {
                    QueryError::FreshnessUnavailable(error.to_string())
                })
            })?;
        operation.check()?;
        let snapshot = engine.snapshot();
        if requirement.is_satisfied_by(snapshot.freshness()) {
            return Ok(snapshot);
        }
    }
    let snapshot = engine.snapshot();
    enforce_freshness(requirement, &snapshot)?;
    Ok(snapshot)
}

/// Pins syntax, asks the outward adapter for Git state, then checks the
/// freshness barrier once more. If an edit landed during the Git read, the
/// revision changes and the whole join is retried rather than returning a
/// mixed snapshot/worktree result.
fn query_workspace_diff(
    engine: &WorkspaceEngine,
    requirement: FreshnessRequirement,
    operation: &OperationContext,
) -> Result<(Arc<WorkspaceSnapshot>, WorkspaceDiff), QueryError> {
    let provider = engine
        .diff_provider()
        .ok_or_else(|| QueryError::DiffUnavailable("provider is not configured".to_owned()))?;
    let attempts = if requirement == FreshnessRequirement::AllowStale {
        1
    } else {
        MAX_FRESH_SNAPSHOT_ATTEMPTS
    };
    let mut last_error = None;

    for _ in 0..attempts {
        operation.check()?;
        let snapshot = query_snapshot(engine, requirement, operation)?;
        let diff = match provider.diff_with_context(
            DiffWorkspace::from_snapshot_with_context(&snapshot, operation)?,
            operation,
        ) {
            Ok(diff) => diff,
            Err(error) => {
                operation.check()?;
                last_error = Some(QueryError::DiffUnavailable(error.to_string()));
                continue;
            }
        };
        if diff.revision != snapshot.revision() {
            last_error = Some(QueryError::DiffUnavailable(format!(
                "provider returned revision {}, expected {}",
                diff.revision,
                snapshot.revision()
            )));
            continue;
        }
        if requirement != FreshnessRequirement::AllowStale {
            engine
                .require_fresh_with_context(operation)
                .map_err(|error| {
                    operation.check().map_or_else(QueryError::from, |_| {
                        QueryError::FreshnessUnavailable(error.to_string())
                    })
                })?;
            let confirmed = engine.snapshot();
            if confirmed.revision() != snapshot.revision()
                || !requirement.is_satisfied_by(confirmed.freshness())
            {
                last_error = Some(QueryError::FreshnessUnavailable(
                    "workspace changed while Git diff state was being read".to_owned(),
                ));
                continue;
            }
        }
        operation.check()?;
        return Ok((snapshot, diff));
    }

    Err(last_error.unwrap_or_else(|| {
        QueryError::FreshnessUnavailable(
            "could not pin one syntax revision across the Git diff read".to_owned(),
        )
    }))
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
    .with_indexing(snapshot.indexing().clone())
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
            call_sites: snapshot.graph().call_site_count(),
            ambiguous_call_sites: snapshot.graph().ambiguous_call_site_count(),
            unresolved_call_sites: snapshot.graph().unresolved_call_site_count(),
        };
        let providers = vec![ProviderInfo {
            name: "rust-analyzer".to_owned(),
            languages: vec![chakra_domain::symbol::Language::Rust],
            capabilities: vec![
                ProviderCapability::IncomingCalls,
                ProviderCapability::OutgoingCalls,
                ProviderCapability::SynchronizationState,
            ],
            state: provider_state,
            last_error: self
                .precise_provider()
                .and_then(|provider| provider.last_error()),
        }];
        let data = StatusData {
            workspace: snapshot.identity().clone(),
            counts,
            providers,
            query_execution: None,
        };
        Ok(envelope(&snapshot, provider_state, false, data))
    }

    fn repo_map(&self, request: RepoMapRequest) -> Result<QueryEnvelope<RepoMapData>, QueryError> {
        self.repo_map_with_context(request, &OperationContext::unbounded())
    }

    fn repo_map_with_context(
        &self,
        request: RepoMapRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<RepoMapData>, QueryError> {
        let snapshot = query_snapshot(self, request.freshness, operation)?;
        let summaries = snapshot
            .graph()
            .file_summaries_with_context(operation)?
            .into_iter()
            .map(|(path, count, provenance, precision)| FileSummary {
                path,
                symbol_count: count,
                provenance,
                precision,
            })
            .collect();
        operation.check()?;
        let (files, truncated) = bounded(summaries, clamp_limit(request.limit));
        Ok(envelope(
            &snapshot,
            provider_state_for(self, &snapshot),
            truncated,
            RepoMapData { files },
        ))
    }

    fn search(&self, request: SearchRequest) -> Result<QueryEnvelope<SearchData>, QueryError> {
        self.search_with_context(request, &OperationContext::unbounded())
    }

    fn search_with_context(
        &self,
        request: SearchRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<SearchData>, QueryError> {
        if request.query.is_empty() {
            return Err(QueryError::Invalid("query must be non-empty".to_owned()));
        }
        if request.query.chars().count() > MAX_QUERY_PATTERN_CHARS {
            return Err(QueryError::Invalid(format!(
                "query exceeds the {MAX_QUERY_PATTERN_CHARS}-character pattern budget"
            )));
        }
        let snapshot = query_snapshot(self, request.freshness, operation)?;
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
        let mut poll = CancellationPoll::new();

        'files: for (path, source) in snapshot.graph().source_files_with_context(operation)? {
            for (line_index, line) in source.lines().enumerate() {
                poll.observe(operation)?;
                for found in matcher.find_iter(line) {
                    poll.observe(operation)?;
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
        self.symbol_search_with_context(request, &OperationContext::unbounded())
    }

    fn symbol_search_with_context(
        &self,
        request: SymbolSearchRequest,
        operation: &OperationContext,
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
        let snapshot = query_snapshot(self, request.freshness, operation)?;
        let limit = clamp_limit(request.limit);
        let (matches, truncated) = snapshot
            .graph()
            .search_names_with_context(query, limit, operation)?;
        operation.check()?;
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
        self.context_with_context(request, &OperationContext::unbounded())
    }

    fn context_with_context(
        &self,
        request: ContextRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<ContextData>, QueryError> {
        let reference = request
            .symbol
            .as_ref()
            .ok_or(QueryError::MissingSymbolRef)?;
        let snapshot = query_snapshot(self, request.freshness, operation)?;
        let graph = snapshot.graph();
        let symbol = resolve(graph, reference, snapshot.revision(), operation)?;
        let limit = clamp_limit(request.limit);

        let mut callers = collect_related(
            graph,
            graph.incoming_edges(symbol.id),
            EdgeKind::Calls,
            true,
            operation,
        )?;

        let mut callees = collect_related(
            graph,
            graph.outgoing_edges(symbol.id),
            EdgeKind::Calls,
            false,
            operation,
        )?;
        let mut provider_state = provider_state_for_language(self, &snapshot, symbol.key.language);
        let mut provider_truncated = false;
        if snapshot.freshness() == Freshness::Fresh
            && let Some(provider) = self
                .precise_provider()
                .filter(|provider| provider.supports(symbol.key.language))
        {
            let result = provider.enrich_with_context(
                PreciseQueryRequest {
                    workspace: ProviderWorkspace::from_snapshot_with_context(&snapshot, operation)?,
                    symbol: ProviderSymbol {
                        name: symbol.name().to_owned(),
                        declaration: symbol.location.clone(),
                        language: symbol.key.language,
                    },
                    directions: CallHierarchyDirections {
                        incoming: true,
                        outgoing: true,
                    },
                    limit,
                },
                operation,
            );
            operation.check()?;
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
        let resolved_caller_ids: std::collections::HashSet<_> =
            callers.iter().map(|caller| caller.symbol.id).collect();
        let resolved_callee_ids: std::collections::HashSet<_> =
            callees.iter().map(|callee| callee.symbol.id).collect();
        let (callers, callers_truncated) = bounded(callers, limit);
        let (callees, callees_truncated) = bounded(callees, limit);

        let mut implementations = collect_related(
            graph,
            graph.incoming_edges(symbol.id),
            EdgeKind::Implements,
            true,
            operation,
        )?;
        sort_related(&mut implementations);
        let (implementations, implementations_truncated) = bounded(implementations, limit);

        let mut tests = collect_related(
            graph,
            graph.incoming_edges(symbol.id),
            EdgeKind::Tests,
            true,
            operation,
        )?;
        sort_related(&mut tests);
        let (tests, tests_truncated) = bounded(tests, limit);

        let (mut syntax_call_candidates, outgoing_candidates_truncated) =
            outgoing_call_candidates(graph, symbol.id, limit, operation)?;
        let (incoming_candidates, incoming_candidates_truncated) =
            incoming_call_candidates(graph, symbol.id, limit, operation)?;
        syntax_call_candidates.extend(incoming_candidates);
        syntax_call_candidates.retain(|candidate| {
            if candidate.caller.id == symbol.id {
                candidate
                    .candidate_target
                    .as_ref()
                    .is_none_or(|target| !resolved_callee_ids.contains(&target.id))
            } else {
                !resolved_caller_ids.contains(&candidate.caller.id)
            }
        });
        sort_call_sites(&mut syntax_call_candidates);
        let (syntax_call_candidates, combined_candidates_truncated) =
            bounded(syntax_call_candidates, limit);

        let mut related_files: Vec<chakra_domain::location::RepoRelativePath> = callers
            .iter()
            .chain(callees.iter())
            .chain(implementations.iter())
            .chain(tests.iter())
            .map(|item| item.symbol.location.file().clone())
            .collect();
        for call_site in &syntax_call_candidates {
            if call_site.caller.id != symbol.id {
                related_files.push(call_site.caller.location.file().clone());
            }
            if let Some(target) = &call_site.candidate_target {
                related_files.push(target.location.file().clone());
            }
        }
        related_files.sort();
        related_files.dedup();
        let (related_files, files_truncated) = bounded(related_files, limit);

        let source = source_snippet(graph, symbol);
        let source_truncated = source.as_ref().is_some_and(|snippet| snippet.truncated);
        let truncated = callers_truncated
            || callees_truncated
            || implementations_truncated
            || tests_truncated
            || files_truncated
            || provider_truncated
            || source_truncated
            || outgoing_candidates_truncated
            || incoming_candidates_truncated
            || combined_candidates_truncated;
        let data = ContextData {
            symbol: SymbolView::from(symbol),
            source,
            callers,
            callees,
            implementations,
            tests,
            syntax_call_candidates,
            related_files,
        };
        Ok(envelope(&snapshot, provider_state, truncated, data))
    }

    fn callers(&self, request: CallersRequest) -> Result<QueryEnvelope<CallersData>, QueryError> {
        self.callers_with_context(request, &OperationContext::unbounded())
    }

    fn callers_with_context(
        &self,
        request: CallersRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<CallersData>, QueryError> {
        let reference = request
            .symbol
            .as_ref()
            .ok_or(QueryError::MissingSymbolRef)?;
        let snapshot = query_snapshot(self, request.freshness, operation)?;
        let graph = snapshot.graph();
        let target = resolve(graph, reference, snapshot.revision(), operation)?;
        let mut callers = collect_related(
            graph,
            graph.incoming_edges(target.id),
            EdgeKind::Calls,
            true,
            operation,
        )?;
        let limit = clamp_limit(request.limit);
        let mut provider_state = provider_state_for_language(self, &snapshot, target.key.language);
        let mut provider_truncated = false;
        let (mut syntax_candidates, candidates_truncated) =
            incoming_call_candidates(graph, target.id, limit, operation)?;
        if snapshot.freshness() == Freshness::Fresh
            && let Some(provider) = self
                .precise_provider()
                .filter(|provider| provider.supports(target.key.language))
        {
            let result = provider.enrich_with_context(
                PreciseQueryRequest {
                    workspace: ProviderWorkspace::from_snapshot_with_context(&snapshot, operation)?,
                    symbol: ProviderSymbol {
                        name: target.name().to_owned(),
                        declaration: target.location.clone(),
                        language: target.key.language,
                    },
                    directions: CallHierarchyDirections {
                        incoming: true,
                        outgoing: false,
                    },
                    limit,
                },
                operation,
            );
            operation.check()?;
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
        let resolved_caller_ids: std::collections::HashSet<_> =
            callers.iter().map(|caller| caller.symbol.id).collect();
        syntax_candidates.retain(|candidate| !resolved_caller_ids.contains(&candidate.caller.id));
        sort_related(&mut callers);
        let (callers, truncated) = bounded(callers, limit);
        let data = CallersData {
            target: SymbolView::from(target),
            callers,
            syntax_candidates,
        };
        Ok(envelope(
            &snapshot,
            provider_state,
            truncated || provider_truncated || candidates_truncated,
            data,
        ))
    }

    fn diff_context(
        &self,
        request: DiffContextRequest,
    ) -> Result<QueryEnvelope<DiffContextData>, QueryError> {
        self.diff_context_with_context(request, &OperationContext::unbounded())
    }

    fn diff_context_with_context(
        &self,
        request: DiffContextRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<DiffContextData>, QueryError> {
        let (snapshot, mut diff) = query_workspace_diff(self, request.freshness, operation)?;
        let graph = snapshot.graph();
        let limit = clamp_limit(request.limit);
        diff.files.sort_by(|a, b| a.path.cmp(&b.path));
        let (file_changes, files_truncated) = bounded(diff.files, limit);

        let mut symbol_ids: Vec<_> = file_changes
            .iter()
            .filter(|change| change.change != chakra_domain::query::ChangeKind::Deleted)
            .flat_map(|change| graph.symbols_in_file(&change.path).map(|symbol| symbol.id))
            .collect();
        symbol_ids.sort_by(|a, b| {
            let a = graph.symbol(*a);
            let b = graph.symbol(*b);
            a.map(|symbol| (&symbol.key.qualified_name, symbol.id))
                .cmp(&b.map(|symbol| (&symbol.key.qualified_name, symbol.id)))
        });
        symbol_ids.dedup();
        let (symbol_ids, symbols_truncated) = bounded(symbol_ids, limit);

        let mut callers = BTreeMap::new();
        let mut tests = BTreeMap::new();
        let mut call_candidates = Vec::with_capacity(limit.saturating_add(1));
        let mut call_candidates_truncated = false;
        let mut poll = CancellationPoll::new();
        for id in &symbol_ids {
            poll.observe(operation)?;
            for edge in graph.incoming_edges(*id) {
                poll.observe(operation)?;
                let Some(item) = related(graph, edge, edge.from) else {
                    continue;
                };
                let diff_relation = DiffRelatedSymbol {
                    changed_symbol_id: *id,
                    relation: item,
                };
                if edge.kind == EdgeKind::Calls {
                    callers
                        .entry((diff_relation.relation.symbol.id, *id))
                        .and_modify(|existing: &mut DiffRelatedSymbol| {
                            if diff_relation.relation.precision > existing.relation.precision {
                                *existing = diff_relation.clone();
                            }
                        })
                        .or_insert_with(|| diff_relation.clone());
                }
                if edge.kind == EdgeKind::Tests {
                    tests
                        .entry((diff_relation.relation.symbol.id, *id))
                        .and_modify(|existing: &mut DiffRelatedSymbol| {
                            if diff_relation.relation.precision > existing.relation.precision {
                                *existing = diff_relation.clone();
                            }
                        })
                        .or_insert(diff_relation);
                }
            }
            let remaining = limit
                .saturating_add(1)
                .saturating_sub(call_candidates.len());
            if remaining == 0 {
                call_candidates_truncated = true;
                continue;
            }
            let (candidates, truncated) =
                incoming_call_candidates(graph, *id, remaining, operation)?;
            call_candidates_truncated |= truncated;
            call_candidates.extend(candidates.into_iter().map(|call_site| DiffCallSite {
                changed_symbol_id: *id,
                call_site,
            }));
        }

        let mut related_callers: Vec<_> = callers.into_values().collect();
        let mut related_tests: Vec<_> = tests.into_values().collect();
        sort_diff_related(&mut related_callers);
        sort_diff_related(&mut related_tests);
        let (related_callers, callers_truncated) = bounded(related_callers, limit);
        let (related_tests, tests_truncated) = bounded(related_tests, limit);
        call_candidates.sort_by(|a, b| {
            a.changed_symbol_id
                .cmp(&b.changed_symbol_id)
                .then(a.call_site.caller.id.cmp(&b.call_site.caller.id))
                .then(
                    a.call_site
                        .location
                        .start()
                        .line()
                        .cmp(&b.call_site.location.start().line()),
                )
        });
        call_candidates_truncated |= call_candidates.len() > limit;
        call_candidates.truncate(limit);

        let changed_files = file_changes
            .into_iter()
            .map(|change| ChangedFile {
                path: change.path,
                previous_path: change.previous_path,
                change: change.change,
                provenance: change.provenance,
                precision: change.precision,
            })
            .collect();
        let changed_symbols = symbol_ids
            .into_iter()
            .filter_map(|id| graph.symbol(id))
            .map(|symbol| ChangedSymbol {
                symbol: SymbolView::from(symbol),
                basis: ChangedSymbolBasis::DeclaredInChangedFile,
                provenance: Provenance::Heuristic,
                precision: Precision::Heuristic,
            })
            .collect();
        let truncated = diff.truncated
            || files_truncated
            || symbols_truncated
            || callers_truncated
            || tests_truncated
            || call_candidates_truncated;
        let data = DiffContextData {
            changed_files,
            changed_symbols,
            related_callers,
            related_tests,
            related_call_candidates: call_candidates,
        };
        Ok(envelope(
            &snapshot,
            provider_state_for(self, &snapshot),
            truncated,
            data,
        ))
    }
}
