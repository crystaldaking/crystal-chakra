//! [`QueryService`] implementation over the published snapshot.
//!
//! Each method pins one `Arc<WorkspaceSnapshot>` up front, so a query always
//! observes exactly one revision (SPEC §5). Reported freshness derives from
//! the workspace status: only a snapshot that completed reconciliation
//! (`Ready`) is `Fresh`; everything else reports `Stale`. This is
//! deliberately conservative until the watcher/reconciliation phases land.

use chakra_domain::envelope::QueryEnvelope;
use chakra_domain::query::{
    CallersData, CallersRequest, ContextData, ContextRequest, DEFAULT_QUERY_LIMIT, DiffContextData,
    DiffContextRequest, FileSummary, IndexCounts, MAX_QUERY_LIMIT, ProviderInfo, QueryError,
    QueryService, RelatedSymbol, RepoMapData, RepoMapRequest, SearchData, SearchRequest,
    StatusData, StatusRequest, SymbolRef, SymbolSearchData, SymbolSearchRequest, SymbolView,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_domain::symbol::{Edge, EdgeKind, Symbol};

use crate::engine::{WorkspaceEngine, WorkspaceSnapshot};
use crate::graph::SymbolGraph;

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

/// Conservative freshness until watcher/reconciliation phases exist: only a
/// workspace that completed indexing may claim fresh data.
fn freshness_of(status: WorkspaceStatus) -> Freshness {
    match status {
        WorkspaceStatus::Ready => Freshness::Fresh,
        _ => Freshness::Stale,
    }
}

fn envelope<T>(snapshot: &WorkspaceSnapshot, truncated: bool, data: T) -> QueryEnvelope<T> {
    QueryEnvelope::new(
        snapshot.identity().workspace.clone(),
        snapshot.revision(),
        freshness_of(snapshot.status()),
        snapshot.status(),
        snapshot.provider_state(),
        truncated,
        data,
    )
}

impl QueryService for WorkspaceEngine {
    fn status(&self, _request: StatusRequest) -> Result<QueryEnvelope<StatusData>, QueryError> {
        let snapshot = self.snapshot();
        let counts = IndexCounts {
            files: snapshot.graph().file_count(),
            symbols: snapshot.graph().symbol_count(),
            edges: snapshot.graph().edge_count(),
        };
        let providers = vec![ProviderInfo {
            name: "rust-analyzer".to_owned(),
            state: snapshot.provider_state(),
        }];
        let data = StatusData {
            workspace: snapshot.identity().clone(),
            counts,
            providers,
        };
        Ok(envelope(&snapshot, false, data))
    }

    fn repo_map(&self, request: RepoMapRequest) -> Result<QueryEnvelope<RepoMapData>, QueryError> {
        let snapshot = self.snapshot();
        let summaries = snapshot
            .graph()
            .file_summaries()
            .into_iter()
            .map(|(path, count)| FileSummary {
                path,
                symbol_count: count,
            })
            .collect();
        let (files, truncated) = bounded(summaries, clamp_limit(request.limit));
        Ok(envelope(&snapshot, truncated, RepoMapData { files }))
    }

    fn search(&self, _request: SearchRequest) -> Result<QueryEnvelope<SearchData>, QueryError> {
        // Text search needs file contents, which arrive with the indexer.
        Err(QueryError::Unsupported("search"))
    }

    fn symbol_search(
        &self,
        request: SymbolSearchRequest,
    ) -> Result<QueryEnvelope<SymbolSearchData>, QueryError> {
        let query = request.query.trim();
        if query.is_empty() {
            return Err(QueryError::Invalid("query must be non-empty".to_owned()));
        }
        let snapshot = self.snapshot();
        let mut candidates: Vec<SymbolView> = snapshot
            .graph()
            .search_names(query)
            .into_iter()
            .filter_map(|id| snapshot.graph().symbol(id))
            .map(SymbolView::from)
            .collect();
        candidates.sort_by(|a, b| {
            a.qualified_name
                .cmp(&b.qualified_name)
                .then(a.id.cmp(&b.id))
        });
        let (candidates, truncated) = bounded(candidates, clamp_limit(request.limit));
        let data = SymbolSearchData {
            query: query.to_owned(),
            candidates,
        };
        Ok(envelope(&snapshot, truncated, data))
    }

    fn context(&self, request: ContextRequest) -> Result<QueryEnvelope<ContextData>, QueryError> {
        let reference = request
            .symbol
            .as_ref()
            .ok_or(QueryError::MissingSymbolRef)?;
        let snapshot = self.snapshot();
        let graph = snapshot.graph();
        let symbol = resolve(graph, reference, snapshot.revision())?;
        let limit = clamp_limit(request.limit);

        let mut callers: Vec<RelatedSymbol> = graph
            .incoming_edges(symbol.id)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .filter_map(|edge| related(graph, edge, edge.from))
            .collect();
        sort_related(&mut callers);
        let (callers, callers_truncated) = bounded(callers, limit);

        let mut callees: Vec<RelatedSymbol> = graph
            .outgoing_edges(symbol.id)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .filter_map(|edge| related(graph, edge, edge.to))
            .collect();
        sort_related(&mut callees);
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
            || files_truncated;
        let data = ContextData {
            symbol: SymbolView::from(symbol),
            callers,
            callees,
            implementations,
            tests,
            related_files,
        };
        Ok(envelope(&snapshot, truncated, data))
    }

    fn callers(&self, request: CallersRequest) -> Result<QueryEnvelope<CallersData>, QueryError> {
        let reference = request
            .symbol
            .as_ref()
            .ok_or(QueryError::MissingSymbolRef)?;
        let snapshot = self.snapshot();
        let graph = snapshot.graph();
        let target = resolve(graph, reference, snapshot.revision())?;
        let mut callers: Vec<RelatedSymbol> = graph
            .incoming_edges(target.id)
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .filter_map(|edge| related(graph, edge, edge.from))
            .collect();
        sort_related(&mut callers);
        let (callers, truncated) = bounded(callers, clamp_limit(request.limit));
        let data = CallersData {
            target: SymbolView::from(target),
            callers,
        };
        Ok(envelope(&snapshot, truncated, data))
    }

    fn diff_context(
        &self,
        _request: DiffContextRequest,
    ) -> Result<QueryEnvelope<DiffContextData>, QueryError> {
        // Needs the Git subsystem; arrives with diff awareness.
        Err(QueryError::Unsupported("diff_context"))
    }
}
