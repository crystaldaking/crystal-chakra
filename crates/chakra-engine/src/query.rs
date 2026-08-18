//! [`QueryService`] implementation over the published snapshot.
//!
//! Each method pins one `Arc<WorkspaceSnapshot>` up front, so a query always
//! observes exactly one revision (SPEC §5). Freshness is the snapshot's own
//! axis, claimed by the publisher — never inferred from the lifecycle
//! status. A request's [`FreshnessRequirement`] is enforced: when the pinned
//! snapshot does not satisfy it, the call fails with
//! [`QueryError::FreshnessNotMet`] instead of silently serving stale data.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::diagnostic::{DiagnosticTruncationCause, MAX_SYNTAX_DIAGNOSTICS_PER_FILE};
use chakra_domain::envelope::{
    QueryEnvelope, TruncationCause, TruncationDetail, TruncationSection,
};
use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::operation::OperationContext;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    CallSiteEvidence, CallSiteView, CallersData, CallersRequest, ChangedFile, ChangedSymbol,
    ChangedSymbolBasis, ContextData, ContextRequest, DEFAULT_QUERY_LIMIT, DiffCallSite,
    DiffContextData, DiffContextRequest, DiffDirectedRelatedSymbol, DiffRelatedSymbol, DiffScope,
    DirectedRelatedSymbol, FileSummary, IndexCounts, MAX_QUERY_LIMIT, MAX_STATUS_DIAGNOSTICS,
    ProviderCapability, ProviderInfo, ProviderQueryInfo, QueryError, QueryService, RelatedSymbol,
    RelationDirection, RepoMapCursor, RepoMapData, RepoMapGroup, RepoMapGroupKind, RepoMapRequest,
    RepoMapScope, SearchData, SearchRequest, SourceFilter, SourceSnippet, StatusData,
    StatusRequest, SymbolRef, SymbolSearchData, SymbolSearchRequest, SymbolView,
    SyntaxDiagnosticSummary, TextMatch,
};
use chakra_domain::revision::Revision;
use chakra_domain::source::{SourceClassification, SourceMetadata, SourceRole};
use chakra_domain::state::{Freshness, FreshnessRequirement, ProviderState};
use chakra_domain::symbol::{
    CallResolution, CallSite, Edge, EdgeKind, EntityId, Language, Symbol, SymbolKind,
};
use serde::Serialize;

use crate::engine::{WorkspaceEngine, WorkspaceSnapshot};
use crate::graph::{GraphFileSummary, SymbolGraph};
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
const MAX_QUERY_FILTER_VALUES: usize = 16;
const MAX_PACKAGE_FILTER_CHARS: usize = 256;
const MAX_NAMESPACE_FILTER_CHARS: usize = 1_024;

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
const MAX_REPRESENTATIVE_LOCATIONS: usize = 3;

// Section budgets deliberately sum far below the 1 MiB MCP guard. The
// selected symbol/target and envelope metadata are fixed response overhead;
// every variable-size collection and source snippet is budgeted here.
const STATUS_PROVIDERS_BYTES: usize = 16 * 1024;
const STATUS_DIAGNOSTICS_BYTES: usize = 128 * 1024;
const REPO_MAP_FILES_BYTES: usize = 256 * 1024;
const REPO_MAP_OVERVIEW_BYTES: usize = 64 * 1024;
const SEARCH_MATCHES_BYTES: usize = 256 * 1024;
const SYMBOL_SEARCH_CANDIDATES_BYTES: usize = 128 * 1024;
const CONTEXT_SOURCE_BYTES: usize = 16 * 1024;
const CONTEXT_CALLERS_BYTES: usize = 96 * 1024;
const CONTEXT_CALLEES_BYTES: usize = 96 * 1024;
const CONTEXT_IMPLEMENTATIONS_BYTES: usize = 64 * 1024;
const CONTEXT_TESTS_BYTES: usize = 64 * 1024;
const CONTEXT_RELATED_RELATIONS_BYTES: usize = 64 * 1024;
const CONTEXT_CALL_CANDIDATES_BYTES: usize = 96 * 1024;
const CONTEXT_RELATED_FILES_BYTES: usize = 32 * 1024;
const CALLERS_CALLERS_BYTES: usize = 128 * 1024;
const CALLERS_CANDIDATES_BYTES: usize = 192 * 1024;
const DIFF_CHANGED_FILES_BYTES: usize = 96 * 1024;
const DIFF_CHANGED_SYMBOLS_BYTES: usize = 128 * 1024;
const DIFF_RELATED_CALLERS_BYTES: usize = 128 * 1024;
const DIFF_RELATED_TESTS_BYTES: usize = 96 * 1024;
const DIFF_RELATED_RELATIONS_BYTES: usize = 96 * 1024;
const DIFF_CALL_CANDIDATES_BYTES: usize = 128 * 1024;
const MIN_EXAMINED_WORK: usize = 1_024;
const MAX_EXAMINED_WORK: usize = 8_192;
const EXAMINED_WORK_PER_RESULT: usize = 16;
const MIN_TRAVERSAL_WORK: usize = 2_048;
const MAX_TRAVERSAL_WORK: usize = 16_384;
const TRAVERSAL_WORK_PER_RESULT: usize = 32;
const MIN_INTERMEDIATE_ITEMS: usize = 2_048;
const MAX_INTERMEDIATE_ITEMS: usize = 4_096;
const INTERMEDIATE_ITEMS_PER_RESULT: usize = 8;
const SECTION_WALL_TIME: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
struct QueryWorkStats {
    files_examined: usize,
    symbols_examined: usize,
    candidates_examined: usize,
    edges_visited: usize,
    call_sites_visited: usize,
    intermediate_items_retained: usize,
    diff_wait_micros: u64,
    provider_wait_micros: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkStop {
    Examined,
    Traversal,
    Allocation,
    WallTime,
}

/// Per-response-section execution guard. Result and encoded-byte bounds live
/// in `bounded_section`; this guard bounds the upstream work needed to build
/// that section before response truncation is applied.
struct SectionWorkBudget {
    started: Instant,
    wall_time: Duration,
    examined_limit: usize,
    traversal_limit: usize,
    intermediate_item_limit: usize,
    files_examined: usize,
    symbols_examined: usize,
    candidates_examined: usize,
    edges_visited: usize,
    call_sites_visited: usize,
    intermediate_items_retained: usize,
    stopped: Option<WorkStop>,
}

impl SectionWorkBudget {
    fn new(result_limit: usize) -> Self {
        Self::with_limits(
            result_limit
                .saturating_mul(EXAMINED_WORK_PER_RESULT)
                .clamp(MIN_EXAMINED_WORK, MAX_EXAMINED_WORK),
            result_limit
                .saturating_mul(TRAVERSAL_WORK_PER_RESULT)
                .clamp(MIN_TRAVERSAL_WORK, MAX_TRAVERSAL_WORK),
            result_limit
                .saturating_mul(INTERMEDIATE_ITEMS_PER_RESULT)
                .clamp(MIN_INTERMEDIATE_ITEMS, MAX_INTERMEDIATE_ITEMS),
            SECTION_WALL_TIME,
        )
    }

    fn with_limits(
        examined_limit: usize,
        traversal_limit: usize,
        intermediate_item_limit: usize,
        wall_time: Duration,
    ) -> Self {
        Self {
            started: Instant::now(),
            wall_time,
            examined_limit,
            traversal_limit,
            intermediate_item_limit,
            files_examined: 0,
            symbols_examined: 0,
            candidates_examined: 0,
            edges_visited: 0,
            call_sites_visited: 0,
            intermediate_items_retained: 0,
            stopped: (wall_time == Duration::ZERO).then_some(WorkStop::WallTime),
        }
    }

    fn check_wall_time(&mut self) -> bool {
        if self.stopped.is_none() && self.started.elapsed() >= self.wall_time {
            self.stopped = Some(WorkStop::WallTime);
        }
        self.stopped.is_none()
    }

    fn examined(&self) -> usize {
        self.files_examined
            .saturating_add(self.symbols_examined)
            .saturating_add(self.candidates_examined)
    }

    fn traversed(&self) -> usize {
        self.edges_visited.saturating_add(self.call_sites_visited)
    }

    fn record_examined(&mut self, kind: ExaminedKind) -> bool {
        if !self.check_wall_time() {
            return false;
        }
        if self.examined() >= self.examined_limit {
            self.stopped = Some(WorkStop::Examined);
            return false;
        }
        match kind {
            ExaminedKind::File => self.files_examined += 1,
            ExaminedKind::Symbol => self.symbols_examined += 1,
            ExaminedKind::Candidate => self.candidates_examined += 1,
        }
        true
    }

    fn visit_edge(&mut self) -> bool {
        self.record_traversal(TraversalKind::Edge)
    }

    fn visit_call_site(&mut self) -> bool {
        self.record_traversal(TraversalKind::CallSite)
    }

    fn record_traversal(&mut self, kind: TraversalKind) -> bool {
        if !self.check_wall_time() {
            return false;
        }
        if self.traversed() >= self.traversal_limit {
            self.stopped = Some(WorkStop::Traversal);
            return false;
        }
        match kind {
            TraversalKind::Edge => self.edges_visited += 1,
            TraversalKind::CallSite => self.call_sites_visited += 1,
        }
        true
    }

    fn remaining_examined(&mut self) -> usize {
        if !self.check_wall_time() {
            return 0;
        }
        self.examined_limit.saturating_sub(self.examined())
    }

    fn remaining_traversal(&mut self) -> usize {
        if !self.check_wall_time() {
            return 0;
        }
        self.traversal_limit.saturating_sub(self.traversed())
    }

    fn remaining_intermediate(&mut self) -> usize {
        if !self.check_wall_time() {
            return 0;
        }
        self.intermediate_item_limit
            .saturating_sub(self.intermediate_items_retained)
    }

    fn retain_intermediate(&mut self) -> bool {
        if !self.check_wall_time() {
            return false;
        }
        if self.intermediate_items_retained >= self.intermediate_item_limit {
            self.stopped = Some(WorkStop::Allocation);
            return false;
        }
        self.intermediate_items_retained += 1;
        true
    }

    fn mark_examined_exhausted(&mut self) {
        if self.stopped.is_none() {
            self.stopped = Some(WorkStop::Examined);
        }
    }

    fn mark_traversal_exhausted(&mut self) {
        if self.stopped.is_none() {
            self.stopped = Some(WorkStop::Traversal);
        }
    }

    fn mark_allocation_exhausted(&mut self) {
        if self.stopped.is_none() {
            self.stopped = Some(WorkStop::Allocation);
        }
    }

    fn truncation(&self, section: TruncationSection) -> Option<TruncationDetail> {
        self.stopped.map(|stop| match stop {
            WorkStop::Examined => TruncationDetail::new(
                section,
                TruncationCause::ExaminedWorkLimit,
                self.examined_limit,
                None,
            ),
            WorkStop::Traversal => TruncationDetail::new(
                section,
                TruncationCause::GraphTraversalLimit,
                self.traversal_limit,
                None,
            ),
            WorkStop::Allocation => TruncationDetail::new(
                section,
                TruncationCause::IntermediateAllocationLimit,
                self.intermediate_item_limit,
                None,
            ),
            WorkStop::WallTime => TruncationDetail::new(
                section,
                TruncationCause::WallTimeLimit,
                usize::try_from(self.wall_time.as_millis()).unwrap_or(usize::MAX),
                None,
            ),
        })
    }

    fn add_to_stats(&self, stats: &mut QueryWorkStats) {
        stats.files_examined = stats.files_examined.saturating_add(self.files_examined);
        stats.symbols_examined = stats.symbols_examined.saturating_add(self.symbols_examined);
        stats.candidates_examined = stats
            .candidates_examined
            .saturating_add(self.candidates_examined);
        stats.edges_visited = stats.edges_visited.saturating_add(self.edges_visited);
        stats.call_sites_visited = stats
            .call_sites_visited
            .saturating_add(self.call_sites_visited);
        stats.intermediate_items_retained = stats
            .intermediate_items_retained
            .saturating_add(self.intermediate_items_retained);
    }
}

#[derive(Clone, Copy)]
enum ExaminedKind {
    File,
    Symbol,
    Candidate,
}

#[derive(Clone, Copy)]
enum TraversalKind {
    Edge,
    CallSite,
}

/// Applies the SPEC §29 budget: default when absent, hard cap always.
fn clamp_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(DEFAULT_QUERY_LIMIT).min(MAX_QUERY_LIMIT) as usize
}

#[derive(Default)]
struct JsonCountWriter {
    bytes: usize,
}

impl Write for JsonCountWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_len<T: Serialize>(value: &T) -> Result<usize, QueryError> {
    let mut writer = JsonCountWriter::default();
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| QueryError::ResponseConstruction(error.to_string()))?;
    Ok(writer.bytes)
}

struct BoundedSection<T> {
    items: Vec<T>,
    truncation: Vec<TruncationDetail>,
    retained_bytes: usize,
}

/// Applies item and exact serialized-byte bounds to one JSON array. Item
/// sizes are measured independently, so the completed envelope is not
/// serialized just to discover that one noisy section is too large.
fn bounded_section<T: Serialize>(
    items: Vec<T>,
    item_limit: usize,
    byte_limit: usize,
    section: TruncationSection,
) -> Result<BoundedSection<T>, QueryError> {
    bounded_section_with_wall_time(items, item_limit, byte_limit, section, SECTION_WALL_TIME)
}

fn bounded_section_with_wall_time<T: Serialize>(
    mut items: Vec<T>,
    item_limit: usize,
    byte_limit: usize,
    section: TruncationSection,
    wall_time: Duration,
) -> Result<BoundedSection<T>, QueryError> {
    let total_items = items.len();
    let desired_item_count = total_items.min(item_limit);
    let sizing_started = Instant::now();
    let mut item_sizes = Vec::with_capacity(desired_item_count);
    for item in items.iter().take(desired_item_count) {
        if sizing_started.elapsed() >= wall_time {
            break;
        }
        item_sizes.push(serialized_len(item)?);
    }
    let item_count = item_sizes.len();

    let item_limited_bytes = item_sizes
        .iter()
        .fold(2_usize, |bytes, size| bytes.saturating_add(*size))
        .saturating_add(item_count.saturating_sub(1));
    let mut retained_count = 0_usize;
    let mut retained_bytes = 2_usize;
    for size in item_sizes {
        let separator = usize::from(retained_count > 0);
        let next = retained_bytes
            .saturating_add(separator)
            .saturating_add(size);
        if next > byte_limit {
            break;
        }
        retained_bytes = next;
        retained_count += 1;
    }

    let mut truncation = Vec::new();
    if total_items > item_limit {
        truncation.push(TruncationDetail::new(
            section,
            TruncationCause::ItemLimit,
            item_limit,
            Some(total_items - item_limit),
        ));
    }
    if item_count < desired_item_count {
        truncation.push(TruncationDetail::new(
            section,
            TruncationCause::WallTimeLimit,
            usize::try_from(wall_time.as_millis()).unwrap_or(usize::MAX),
            None,
        ));
    }
    if retained_count < item_count {
        truncation.push(TruncationDetail::new(
            section,
            TruncationCause::ResponseByteLimit,
            byte_limit,
            Some(item_limited_bytes.saturating_sub(retained_bytes)),
        ));
    }
    items.truncate(retained_count);
    Ok(BoundedSection {
        items,
        truncation,
        retained_bytes,
    })
}

fn candidate_truncation(
    section: TruncationSection,
    limit: usize,
    truncated: bool,
) -> Option<TruncationDetail> {
    truncated.then(|| {
        TruncationDetail::new(
            section,
            TruncationCause::UnresolvedCandidateFanout,
            limit,
            None,
        )
    })
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

struct RelationEvidence {
    occurrence_count: u64,
    representative_locations: Vec<SourceRange>,
    locations_omitted: u64,
    representative_call_sites: Vec<CallSiteEvidence>,
    call_site_evidence_omitted: u64,
}

fn relation_evidence(graph: &SymbolGraph, edge: &Edge) -> RelationEvidence {
    if edge.kind == EdgeKind::Tests {
        let call_sites: Vec<_> = graph
            .call_sites_from(edge.from)
            .filter(|call_site| {
                matches!(
                    call_site.resolution,
                    CallResolution::Resolved { target } if target == edge.to
                )
            })
            .collect();
        if !call_sites.is_empty() {
            let occurrence_count = u64::try_from(call_sites.len()).unwrap_or(u64::MAX);
            let retained = call_sites.len().min(MAX_REPRESENTATIVE_LOCATIONS);
            return RelationEvidence {
                occurrence_count,
                representative_locations: call_sites
                    .iter()
                    .take(MAX_REPRESENTATIVE_LOCATIONS)
                    .map(|call_site| call_site.location.clone())
                    .collect(),
                locations_omitted: occurrence_count
                    .saturating_sub(u64::try_from(retained).unwrap_or(u64::MAX)),
                representative_call_sites: call_sites
                    .iter()
                    .take(MAX_REPRESENTATIVE_LOCATIONS)
                    .map(|call_site| call_site_evidence(call_site))
                    .collect(),
                call_site_evidence_omitted: occurrence_count
                    .saturating_sub(u64::try_from(retained).unwrap_or(u64::MAX)),
            };
        }
    }

    RelationEvidence {
        occurrence_count: 1,
        representative_locations: edge.location.iter().cloned().collect(),
        locations_omitted: 0,
        representative_call_sites: graph
            .call_site_for_edge(edge)
            .map(call_site_evidence)
            .into_iter()
            .collect(),
        call_site_evidence_omitted: 0,
    }
}

fn append_relation_evidence(item: &mut RelatedSymbol, evidence: RelationEvidence) {
    item.occurrence_count = item
        .occurrence_count
        .saturating_add(evidence.occurrence_count);
    item.locations_omitted = item
        .locations_omitted
        .saturating_add(evidence.locations_omitted);
    for location in evidence.representative_locations {
        if item.representative_locations.len() < MAX_REPRESENTATIVE_LOCATIONS {
            item.representative_locations.push(location);
        } else {
            item.locations_omitted = item.locations_omitted.saturating_add(1);
        }
    }
    item.call_site_evidence_omitted = item
        .call_site_evidence_omitted
        .saturating_add(evidence.call_site_evidence_omitted);
    for call_site in evidence.representative_call_sites {
        if item.representative_call_sites.len() < MAX_REPRESENTATIVE_LOCATIONS {
            item.representative_call_sites.push(call_site);
        } else {
            item.call_site_evidence_omitted = item.call_site_evidence_omitted.saturating_add(1);
        }
    }
}

struct RelatedViews {
    items: Vec<RelatedSymbol>,
    item_truncated: bool,
}

#[derive(Clone, Copy)]
enum RelatedEndpoint {
    From,
    To,
}

fn edge_kind_rank(kind: EdgeKind) -> u8 {
    match kind {
        EdgeKind::Contains => 0,
        EdgeKind::Defines => 1,
        EdgeKind::References => 2,
        EdgeKind::Calls => 3,
        EdgeKind::Imports => 4,
        EdgeKind::Implements => 5,
        EdgeKind::Extends => 6,
        EdgeKind::Tests => 7,
        EdgeKind::DependsOn => 8,
        EdgeKind::Binds => 9,
        EdgeKind::Resolves => 10,
        EdgeKind::RoutesTo => 11,
        EdgeKind::Dispatches => 12,
        EdgeKind::ListensTo => 13,
        EdgeKind::Schedules => 14,
        EdgeKind::Registers => 15,
        EdgeKind::AuthorizesWith => 16,
        EdgeKind::ModifiedBy => 17,
    }
}

fn related_from_edges(
    graph: &SymbolGraph,
    edges: &[Edge],
    kinds: &[EdgeKind],
    endpoint: RelatedEndpoint,
    limit: usize,
    work: &mut SectionWorkBudget,
    operation: &OperationContext,
) -> Result<RelatedViews, QueryError> {
    let capacity = limit.saturating_add(1);
    let mut items = BTreeMap::<(&str, EntityId, u8), RelatedSymbol>::new();
    let mut item_truncated = false;
    let mut poll = CancellationPoll::new();
    for edge in edges {
        poll.observe(operation)?;
        if !work.visit_edge() {
            break;
        }
        if !kinds.contains(&edge.kind) {
            continue;
        }
        let other = match endpoint {
            RelatedEndpoint::From => edge.from,
            RelatedEndpoint::To => edge.to,
        };
        let Some(symbol) = graph.symbol(other) else {
            continue;
        };
        let evidence = relation_evidence(graph, edge);
        let key = (
            symbol.key.qualified_name.as_str(),
            other,
            edge_kind_rank(edge.kind),
        );
        if let Some(item) = items.get_mut(&key) {
            append_relation_evidence(item, evidence);
            if edge.precision > item.precision {
                item.precision = edge.precision;
                item.provenance = edge.provenance;
            }
            continue;
        }

        if items.len() >= capacity {
            item_truncated = true;
            let belongs_in_prefix = items
                .last_key_value()
                .is_some_and(|(last_key, _)| key < *last_key);
            if !belongs_in_prefix {
                continue;
            }
            items.pop_last();
        } else if !work.retain_intermediate() {
            break;
        }

        items.insert(
            key,
            RelatedSymbol {
                symbol: symbol_view(graph, symbol),
                edge_kind: edge.kind,
                provenance: edge.provenance,
                precision: edge.precision,
                occurrence_count: evidence.occurrence_count,
                representative_locations: evidence.representative_locations,
                locations_omitted: evidence.locations_omitted,
                representative_call_sites: evidence.representative_call_sites,
                call_site_evidence_omitted: evidence.call_site_evidence_omitted,
            },
        );
    }
    if items.len() > limit {
        items.pop_last();
        item_truncated = true;
    }
    operation.check()?;
    Ok(RelatedViews {
        items: items.into_values().collect(),
        item_truncated,
    })
}

fn symbol_view(graph: &SymbolGraph, symbol: &Symbol) -> SymbolView {
    if let Some(metadata) = graph.file_metadata(&symbol.key.path) {
        return SymbolView::from_symbol_with_metadata(symbol, metadata);
    }
    SymbolView::from(symbol)
}

#[derive(Debug)]
struct PreparedSourceFilter {
    package: Option<String>,
    path_prefix: Option<chakra_domain::location::RepoRelativePath>,
    include_roles: Vec<SourceRole>,
    exclude_roles: Vec<SourceRole>,
}

impl PreparedSourceFilter {
    fn new(filter: SourceFilter) -> Result<Self, QueryError> {
        if filter.include_roles.len() > MAX_QUERY_FILTER_VALUES
            || filter.exclude_roles.len() > MAX_QUERY_FILTER_VALUES
        {
            return Err(QueryError::Invalid(format!(
                "source role filters accept at most {MAX_QUERY_FILTER_VALUES} values"
            )));
        }
        let package = filter
            .package
            .map(|package| {
                let package = package.trim();
                if package.is_empty() {
                    return Err(QueryError::Invalid(
                        "package filter must be non-empty".to_owned(),
                    ));
                }
                if package.chars().count() > MAX_PACKAGE_FILTER_CHARS {
                    return Err(QueryError::Invalid(format!(
                        "package filter exceeds {MAX_PACKAGE_FILTER_CHARS} characters"
                    )));
                }
                Ok(package.to_owned())
            })
            .transpose()?;
        let path_prefix = filter
            .path_prefix
            .map(|prefix| {
                let prefix = prefix.trim_end_matches('/');
                if prefix.is_empty() {
                    return Err(QueryError::Invalid(
                        "path prefix must be non-empty".to_owned(),
                    ));
                }
                chakra_domain::location::RepoRelativePath::new(prefix)
                    .map_err(|error| QueryError::Invalid(format!("invalid path prefix: {error}")))
            })
            .transpose()?;
        let mut include_roles = filter.include_roles;
        include_roles.sort_unstable();
        include_roles.dedup();
        let mut exclude_roles = filter.exclude_roles;
        exclude_roles.sort_unstable();
        exclude_roles.dedup();
        Ok(Self {
            package,
            path_prefix,
            include_roles,
            exclude_roles,
        })
    }

    fn matches(
        &self,
        path: &chakra_domain::location::RepoRelativePath,
        metadata: &SourceMetadata,
    ) -> bool {
        if self.package.as_ref().is_some_and(|expected| {
            metadata.package.as_ref().map(|package| &package.name) != Some(expected)
        }) {
            return false;
        }
        if let Some(prefix) = &self.path_prefix {
            let prefix = prefix.as_str();
            let path = path.as_str();
            if path != prefix
                && !path
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            {
                return false;
            }
        }
        (self.include_roles.is_empty() || self.include_roles.contains(&metadata.role))
            && !self.exclude_roles.contains(&metadata.role)
    }

    fn normalized(&self) -> SourceFilter {
        SourceFilter {
            package: self.package.clone(),
            path_prefix: self
                .path_prefix
                .as_ref()
                .map(|path| path.as_str().to_owned()),
            include_roles: self.include_roles.clone(),
            exclude_roles: self.exclude_roles.clone(),
        }
    }
}

#[derive(Debug)]
struct PreparedRepoMapScope {
    source: PreparedSourceFilter,
    include_languages: Vec<Language>,
}

impl PreparedRepoMapScope {
    fn new(scope: RepoMapScope) -> Result<Self, QueryError> {
        if scope.include_languages.len() > MAX_QUERY_FILTER_VALUES {
            return Err(QueryError::Invalid(format!(
                "language filters accept at most {MAX_QUERY_FILTER_VALUES} values"
            )));
        }
        let mut include_languages = scope.include_languages;
        include_languages.sort_unstable();
        include_languages.dedup();
        Ok(Self {
            source: PreparedSourceFilter::new(scope.source)?,
            include_languages,
        })
    }

    fn normalized(&self) -> RepoMapScope {
        RepoMapScope {
            include_languages: self.include_languages.clone(),
            source: self.source.normalized(),
        }
    }

    fn matches(
        &self,
        path: &RepoRelativePath,
        language: Language,
        metadata: &SourceMetadata,
    ) -> bool {
        (self.include_languages.is_empty() || self.include_languages.contains(&language))
            && self.source.matches(path, metadata)
    }
}

#[derive(Debug)]
struct PreparedSymbolFilter {
    source: PreparedSourceFilter,
    include_languages: Vec<Language>,
    include_kinds: Vec<SymbolKind>,
    exclude_kinds: Vec<SymbolKind>,
    namespace_prefix: Option<String>,
}

impl PreparedSymbolFilter {
    fn new(request: &mut SymbolSearchRequest) -> Result<Self, QueryError> {
        for (name, values) in [
            ("language", request.include_languages.len()),
            ("symbol kind", request.include_kinds.len()),
            ("excluded symbol kind", request.exclude_kinds.len()),
        ] {
            if values > MAX_QUERY_FILTER_VALUES {
                return Err(QueryError::Invalid(format!(
                    "{name} filters accept at most {MAX_QUERY_FILTER_VALUES} values"
                )));
            }
        }
        let namespace_prefix = request
            .namespace_prefix
            .take()
            .map(|prefix| {
                let prefix = prefix.trim().trim_end_matches("::");
                if prefix.is_empty() {
                    return Err(QueryError::Invalid(
                        "namespace prefix must be non-empty".to_owned(),
                    ));
                }
                if prefix.chars().count() > MAX_NAMESPACE_FILTER_CHARS {
                    return Err(QueryError::Invalid(format!(
                        "namespace prefix exceeds {MAX_NAMESPACE_FILTER_CHARS} characters"
                    )));
                }
                Ok(prefix.to_owned())
            })
            .transpose()?;
        Ok(Self {
            source: PreparedSourceFilter::new(std::mem::take(&mut request.source))?,
            include_languages: std::mem::take(&mut request.include_languages),
            include_kinds: std::mem::take(&mut request.include_kinds),
            exclude_kinds: std::mem::take(&mut request.exclude_kinds),
            namespace_prefix,
        })
    }

    fn matches(&self, symbol: &Symbol, metadata: &SourceMetadata) -> bool {
        if !self.source.matches(&symbol.key.path, metadata)
            || (!self.include_languages.is_empty()
                && !self.include_languages.contains(&symbol.key.language))
            || (!self.include_kinds.is_empty() && !self.include_kinds.contains(&symbol.key.kind))
            || self.exclude_kinds.contains(&symbol.key.kind)
        {
            return false;
        }
        self.namespace_prefix.as_ref().is_none_or(|prefix| {
            symbol.key.qualified_name == *prefix
                || symbol
                    .key
                    .qualified_name
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with("::"))
        })
    }
}

/// A smaller value is a better candidate. `BinaryHeap` keeps the greatest
/// (worst) retained value at its root so replacement remains O(log limit).
#[derive(Debug)]
struct RankedSymbol<'a> {
    match_rank: u8,
    kind_rank: u8,
    source_rank: u8,
    symbol: &'a Symbol,
}

impl PartialEq for RankedSymbol<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for RankedSymbol<'_> {}

impl PartialOrd for RankedSymbol<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedSymbol<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.match_rank
            .cmp(&other.match_rank)
            .then(self.kind_rank.cmp(&other.kind_rank))
            .then(self.source_rank.cmp(&other.source_rank))
            .then(self.symbol.key.language.cmp(&other.symbol.key.language))
            .then(
                self.symbol
                    .key
                    .qualified_name
                    .cmp(&other.symbol.key.qualified_name),
            )
            .then(self.symbol.key.path.cmp(&other.symbol.key.path))
            .then(
                self.symbol
                    .location
                    .start()
                    .cmp(&other.symbol.location.start()),
            )
            .then(self.symbol.location.end().cmp(&other.symbol.location.end()))
            .then(kind_order(self.symbol.key.kind).cmp(&kind_order(other.symbol.key.kind)))
            .then(self.symbol.id.cmp(&other.symbol.id))
    }
}

fn match_rank(symbol: &Symbol, query_lower: &str) -> Option<u8> {
    let qualified = symbol.key.qualified_name.to_lowercase();
    let simple = qualified.rsplit("::").next().unwrap_or(&qualified);
    if qualified == query_lower || simple == query_lower {
        Some(0)
    } else if simple.starts_with(query_lower) {
        Some(1)
    } else if qualified.starts_with(query_lower) {
        Some(2)
    } else if simple.contains(query_lower) {
        Some(3)
    } else if qualified.contains(query_lower) {
        Some(4)
    } else {
        None
    }
}

fn kind_rank(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::ImplBlock => 1,
        SymbolKind::Import => 2,
        _ => 0,
    }
}

fn kind_order(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Module => 0,
        SymbolKind::Function => 1,
        SymbolKind::Method => 2,
        SymbolKind::Struct => 3,
        SymbolKind::Class => 4,
        SymbolKind::Enum => 5,
        SymbolKind::Trait => 6,
        SymbolKind::Interface => 7,
        SymbolKind::Constant => 8,
        SymbolKind::Field => 9,
        SymbolKind::Property => 10,
        SymbolKind::ImplBlock => 11,
        SymbolKind::Import => 12,
        SymbolKind::Configuration => 13,
        SymbolKind::Test => 14,
    }
}

fn source_rank(role: SourceRole) -> u8 {
    match role {
        SourceRole::Production => 0,
        SourceRole::Test => 1,
        SourceRole::Example => 2,
        SourceRole::Bench => 3,
        SourceRole::Fixture => 4,
        SourceRole::Generated => 5,
        SourceRole::Vendor => 6,
    }
}

fn file_language(path: &RepoRelativePath) -> Option<Language> {
    if path.as_str().ends_with(".rs") {
        Some(Language::Rust)
    } else if path.as_str().ends_with(".php") {
        Some(Language::Php)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RepoMapGroupKey {
    kind: RepoMapGroupKind,
    name: String,
    root: Option<RepoRelativePath>,
    language: Language,
}

fn add_repo_map_group(
    groups: &mut BTreeMap<RepoMapGroupKey, (u64, u64)>,
    key: RepoMapGroupKey,
    symbol_count: u64,
) {
    let counts = groups.entry(key).or_default();
    counts.0 += 1;
    counts.1 = counts.1.saturating_add(symbol_count);
}

fn repo_map_overview(files: &[(GraphFileSummary, Language)]) -> Vec<RepoMapGroup> {
    let mut groups = BTreeMap::new();
    for (file, language) in files {
        let top_level = file.path.as_str().split('/').next().unwrap_or_default();
        let (name, root) = if top_level == file.path.as_str() {
            ("(root)".to_owned(), None)
        } else {
            (top_level.to_owned(), RepoRelativePath::new(top_level).ok())
        };
        add_repo_map_group(
            &mut groups,
            RepoMapGroupKey {
                kind: RepoMapGroupKind::TopLevelDirectory,
                name,
                root,
                language: *language,
            },
            file.symbol_count,
        );
        let kind = match file.metadata.classification {
            SourceClassification::CargoMetadata => Some(RepoMapGroupKind::CargoPackage),
            SourceClassification::ComposerMetadata => Some(RepoMapGroupKind::ComposerPsr4),
            SourceClassification::PathFallback => None,
        };
        if let (Some(kind), Some(package)) = (kind, &file.metadata.package) {
            add_repo_map_group(
                &mut groups,
                RepoMapGroupKey {
                    kind,
                    name: package.name.clone(),
                    root: package.root.clone(),
                    language: *language,
                },
                file.symbol_count,
            );
        }
    }
    let mut groups: Vec<_> = groups
        .into_iter()
        .map(|(key, (file_count, symbol_count))| RepoMapGroup {
            kind: key.kind,
            name: key.name,
            root: key.root,
            language: key.language,
            file_count,
            symbol_count,
        })
        .collect();
    groups.sort_by(|a, b| {
        b.file_count
            .cmp(&a.file_count)
            .then(a.kind.cmp(&b.kind))
            .then(a.language.cmp(&b.language))
            .then(a.root.cmp(&b.root))
            .then(a.name.cmp(&b.name))
    });
    groups
}

fn ranked_symbol_matches(
    graph: &SymbolGraph,
    query: &str,
    filter: &PreparedSymbolFilter,
    limit: usize,
    operation: &OperationContext,
    work: &mut SectionWorkBudget,
) -> Result<(Vec<EntityId>, bool), QueryError> {
    let query_lower = query.to_lowercase();
    let mut best = BinaryHeap::with_capacity(limit.min(graph.symbol_count() as usize));
    let mut truncated = false;
    let mut poll = CancellationPoll::new();
    for symbol in graph.symbols() {
        poll.observe(operation)?;
        if !work.record_examined(ExaminedKind::Symbol) {
            break;
        }
        let Some(metadata) = graph.file_metadata(&symbol.key.path) else {
            continue;
        };
        if !filter.matches(symbol, metadata) {
            continue;
        }
        let Some(match_rank) = match_rank(symbol, &query_lower) else {
            continue;
        };
        let candidate = RankedSymbol {
            match_rank,
            kind_rank: kind_rank(symbol.key.kind),
            source_rank: source_rank(metadata.role),
            symbol,
        };
        if best.len() < limit {
            if !work.retain_intermediate() {
                break;
            }
            best.push(candidate);
        } else {
            truncated = true;
            if best
                .peek()
                .is_some_and(|worst| candidate.cmp(worst) == Ordering::Less)
            {
                best.pop();
                best.push(candidate);
            }
        }
    }
    let mut best = best.into_vec();
    best.sort_unstable();
    operation.check()?;
    Ok((
        best.into_iter()
            .map(|candidate| candidate.symbol.id)
            .collect(),
        truncated,
    ))
}

fn sort_related(items: &mut [RelatedSymbol]) {
    items.sort_by(|a, b| {
        a.symbol
            .qualified_name
            .cmp(&b.symbol.qualified_name)
            .then(a.symbol.id.cmp(&b.symbol.id))
            .then(edge_kind_rank(a.edge_kind).cmp(&edge_kind_rank(b.edge_kind)))
    });
}

fn sort_tests(items: &mut [RelatedSymbol]) {
    items.sort_by(|a, b| {
        b.precision
            .cmp(&a.precision)
            .then(
                (!b.representative_locations.is_empty())
                    .cmp(&!a.representative_locations.is_empty()),
            )
            .then(a.symbol.qualified_name.cmp(&b.symbol.qualified_name))
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

const CONTEXT_RELATION_KINDS: &[EdgeKind] = &[
    EdgeKind::DependsOn,
    EdgeKind::Binds,
    EdgeKind::Resolves,
    EdgeKind::RoutesTo,
    EdgeKind::Dispatches,
    EdgeKind::ListensTo,
    EdgeKind::Schedules,
    EdgeKind::Registers,
    EdgeKind::AuthorizesWith,
];

fn sort_directed_related(items: &mut [DirectedRelatedSymbol]) {
    items.sort_by(|a, b| {
        a.relation
            .symbol
            .qualified_name
            .cmp(&b.relation.symbol.qualified_name)
            .then(a.direction.cmp(&b.direction))
            .then(a.relation.edge_kind.cmp(&b.relation.edge_kind))
            .then(a.relation.symbol.id.cmp(&b.relation.symbol.id))
    });
}

fn sort_diff_directed_related(items: &mut [DiffDirectedRelatedSymbol]) {
    items.sort_by(|a, b| {
        a.changed_symbol_id
            .cmp(&b.changed_symbol_id)
            .then_with(|| {
                a.relation
                    .relation
                    .symbol
                    .qualified_name
                    .cmp(&b.relation.relation.symbol.qualified_name)
            })
            .then(a.relation.direction.cmp(&b.relation.direction))
            .then(
                a.relation
                    .relation
                    .symbol
                    .id
                    .cmp(&b.relation.relation.symbol.id),
            )
    });
}

fn call_site_evidence(call_site: &CallSite) -> CallSiteEvidence {
    CallSiteEvidence {
        form: call_site.form,
        target_kind: call_site.target_kind,
        name: call_site.name.clone(),
        qualifier: call_site.qualifier.clone(),
        receiver_type: call_site.receiver_type.clone(),
        receiver_type_source: call_site.receiver_type_source,
        receiver_hint: call_site.receiver_hint.clone(),
        location: call_site.location.clone(),
        resolution: call_site.resolution.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CallSiteTargetKey {
    Candidate(EntityId),
    Unresolved {
        form: chakra_domain::symbol::CallForm,
        target_kind: chakra_domain::symbol::CallTargetKind,
        name: String,
        qualifier: Option<String>,
        receiver_hint: Option<String>,
    },
}

fn aggregate_call_sites<'a>(
    graph: &SymbolGraph,
    sites: impl IntoIterator<Item = (&'a CallSite, Option<&'a Symbol>)>,
) -> Vec<CallSiteView> {
    let mut index = HashMap::<(EntityId, CallSiteTargetKey), usize>::new();
    let mut items = Vec::<CallSiteView>::new();
    for (call_site, candidate_target) in sites {
        let Some(caller) = graph.symbol(call_site.caller) else {
            continue;
        };
        let target_key = candidate_target.map_or_else(
            || CallSiteTargetKey::Unresolved {
                form: call_site.form,
                target_kind: call_site.target_kind,
                name: call_site.name.clone(),
                qualifier: call_site.qualifier.clone(),
                receiver_hint: call_site.receiver_hint.clone(),
            },
            |target| CallSiteTargetKey::Candidate(target.id),
        );
        let key = (call_site.caller, target_key);
        if let Some(position) = index.get(&key).copied() {
            let item = &mut items[position];
            item.occurrence_count = item.occurrence_count.saturating_add(1);
            if item.representative_evidence.len() < MAX_REPRESENTATIVE_LOCATIONS {
                item.representative_evidence
                    .push(call_site_evidence(call_site));
            } else {
                item.evidence_omitted = item.evidence_omitted.saturating_add(1);
            }
            continue;
        }

        index.insert(key, items.len());
        items.push(CallSiteView {
            caller: symbol_view(graph, caller),
            candidate_target: candidate_target.map(|target| symbol_view(graph, target)),
            occurrence_count: 1,
            representative_evidence: vec![call_site_evidence(call_site)],
            evidence_omitted: 0,
            provenance: call_site.provenance,
            precision: if candidate_target.is_some() {
                Precision::Heuristic
            } else {
                call_site.precision
            },
        });
    }
    items
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
            .then_with(|| {
                let a = a
                    .representative_evidence
                    .first()
                    .map(|evidence| &evidence.location);
                let b = b
                    .representative_evidence
                    .first()
                    .map(|evidence| &evidence.location);
                a.map(|range| (range.file(), range.start()))
                    .cmp(&b.map(|range| (range.file(), range.start())))
            })
    });
}

struct CandidateViews {
    items: Vec<CallSiteView>,
    item_truncated: bool,
    fanout_truncated: bool,
}

fn outgoing_call_candidates(
    graph: &SymbolGraph,
    caller: EntityId,
    limit: usize,
    operation: &OperationContext,
    work: &mut SectionWorkBudget,
) -> Result<CandidateViews, QueryError> {
    let mut expanded = Vec::new();
    let mut fanout_truncated = false;
    let mut poll = CancellationPoll::new();
    'sites: for call_site in graph.call_sites_from(caller) {
        poll.observe(operation)?;
        if !work.visit_call_site() {
            break;
        }
        match call_site.resolution {
            CallResolution::Resolved { .. } => continue,
            CallResolution::Unresolved => {
                if !work.retain_intermediate() {
                    break;
                }
                expanded.push((call_site, None));
            }
            CallResolution::Ambiguous { candidates: total } => {
                let per_call_limit = limit.saturating_add(1);
                let remaining = work.remaining_examined();
                if remaining == 0 {
                    work.mark_examined_exhausted();
                    break;
                }
                let effective_limit = per_call_limit.min(remaining);
                let (candidates, _) = graph.call_candidates(call_site, effective_limit);
                fanout_truncated |= total > per_call_limit as u64;
                let work_exhausted =
                    total > effective_limit as u64 && effective_limit < per_call_limit;
                for target in candidates {
                    if !work.record_examined(ExaminedKind::Candidate) {
                        break;
                    }
                    if !work.retain_intermediate() {
                        break 'sites;
                    }
                    expanded.push((call_site, Some(target)));
                }
                if work_exhausted {
                    work.mark_examined_exhausted();
                    break;
                }
            }
        }
    }
    let mut items = aggregate_call_sites(graph, expanded);
    sort_call_sites(&mut items);
    Ok(CandidateViews {
        items,
        item_truncated: false,
        fanout_truncated,
    })
}

fn incoming_call_candidates(
    graph: &SymbolGraph,
    target: EntityId,
    operation: &OperationContext,
    work: &mut SectionWorkBudget,
) -> Result<CandidateViews, QueryError> {
    operation.check()?;
    let traversal_remaining = work.remaining_traversal();
    let allocation_remaining = work.remaining_intermediate();
    let scan_limit = traversal_remaining.min(allocation_remaining);
    let (call_sites, traversal_truncated) = graph.call_sites_for_target(target, scan_limit);
    let target = graph.symbol(target);
    let mut examined = Vec::with_capacity(call_sites.len());
    for call_site in call_sites {
        if !work.visit_call_site() {
            break;
        }
        if !work.retain_intermediate() {
            break;
        }
        examined.push((call_site, target));
    }
    if traversal_truncated {
        if allocation_remaining < traversal_remaining {
            work.mark_allocation_exhausted();
        } else {
            work.mark_traversal_exhausted();
        }
    }
    let mut items = aggregate_call_sites(graph, examined);
    sort_call_sites(&mut items);
    operation.check()?;
    Ok(CandidateViews {
        items,
        item_truncated: false,
        fanout_truncated: false,
    })
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

fn provider_query_info(
    engine: &WorkspaceEngine,
    language: chakra_domain::symbol::Language,
    state: ProviderState,
) -> Option<ProviderQueryInfo> {
    let provider = engine
        .precise_provider()
        .filter(|provider| provider.supports(language))?;
    Some(ProviderQueryInfo {
        name: "rust-analyzer".to_owned(),
        state,
        fallback_used: state != ProviderState::Ready,
        fallback_reason: match state {
            ProviderState::Ready => None,
            ProviderState::CatchingUp | ProviderState::Initializing => Some(
                "provider did not prove readiness for the pinned revision within the wait budget; syntax facts were retained"
                    .to_owned(),
            ),
            ProviderState::Degraded => Some(
                "provider is degraded; syntax facts were retained without claiming precision"
                    .to_owned(),
            ),
            ProviderState::NotConfigured => Some(
                "no precise provider is configured; syntax facts were retained".to_owned(),
            ),
        },
        last_error: provider.last_error(),
        progress: provider.progress(),
        wait_budget_millis: provider
            .query_wait_budget()
            .map(|budget| u64::try_from(budget.as_millis()).unwrap_or(u64::MAX)),
    })
}

fn precise_result_is_current(
    engine: &WorkspaceEngine,
    snapshot: &WorkspaceSnapshot,
    result_revision: Revision,
) -> bool {
    if result_revision != snapshot.revision() {
        return false;
    }
    let current = engine.snapshot();
    current.revision() == snapshot.revision()
        && current.freshness() == Freshness::Fresh
        && snapshot.freshness() == Freshness::Fresh
}

fn precise_related(graph: &SymbolGraph, relation: PreciseRelation) -> Option<RelatedSymbol> {
    let position = relation.declaration.start();
    let symbol = graph
        .symbols_in_file(relation.declaration.file())
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
    let occurrence_count = relation
        .occurrence_count
        .max(u64::try_from(relation.call_sites.len()).unwrap_or(u64::MAX))
        .max(1);
    let retained_locations = relation.call_sites.len().min(MAX_REPRESENTATIVE_LOCATIONS);
    Some(RelatedSymbol {
        symbol: symbol_view(graph, symbol),
        edge_kind: EdgeKind::Calls,
        provenance: relation.provenance,
        precision: Precision::Precise,
        occurrence_count,
        locations_omitted: occurrence_count
            .saturating_sub(u64::try_from(retained_locations).unwrap_or(u64::MAX)),
        representative_locations: relation
            .call_sites
            .into_iter()
            .take(MAX_REPRESENTATIVE_LOCATIONS)
            .collect(),
        representative_call_sites: Vec::new(),
        call_site_evidence_omitted: 0,
    })
}

fn merge_related_occurrences(existing: &mut RelatedSymbol, incoming: RelatedSymbol) {
    existing.occurrence_count = existing
        .occurrence_count
        .saturating_add(incoming.occurrence_count);
    existing.locations_omitted = existing
        .locations_omitted
        .saturating_add(incoming.locations_omitted);
    for location in incoming.representative_locations {
        if existing.representative_locations.len() < MAX_REPRESENTATIVE_LOCATIONS {
            existing.representative_locations.push(location);
        } else {
            existing.locations_omitted = existing.locations_omitted.saturating_add(1);
        }
    }
    existing.call_site_evidence_omitted = existing
        .call_site_evidence_omitted
        .saturating_add(incoming.call_site_evidence_omitted);
    for evidence in incoming.representative_call_sites {
        if existing.representative_call_sites.len() < MAX_REPRESENTATIVE_LOCATIONS {
            existing.representative_call_sites.push(evidence);
        } else {
            existing.call_site_evidence_omitted =
                existing.call_site_evidence_omitted.saturating_add(1);
        }
    }
    if incoming.precision > existing.precision {
        existing.precision = incoming.precision;
        existing.provenance = incoming.provenance;
    }
}

/// Precise relations win for the same revision-scoped entity; unmatched
/// syntax candidates remain visible with their original lower precision.
fn merge_precise(
    graph: &SymbolGraph,
    syntax: Vec<RelatedSymbol>,
    precise: Vec<PreciseRelation>,
) -> Vec<RelatedSymbol> {
    let mut merged = Vec::<RelatedSymbol>::new();
    let mut precise_index = HashMap::<EntityId, usize>::new();
    for relation in precise
        .into_iter()
        .filter_map(|relation| precise_related(graph, relation))
    {
        if let Some(position) = precise_index.get(&relation.symbol.id).copied() {
            merge_related_occurrences(&mut merged[position], relation);
        } else {
            precise_index.insert(relation.symbol.id, merged.len());
            merged.push(relation);
        }
    }
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
    scope: DiffScope,
) -> Result<(Arc<WorkspaceSnapshot>, WorkspaceDiff), QueryError> {
    let provider = engine
        .diff_provider()
        .ok_or_else(|| QueryError::DiffUnavailable("provider is not configured".to_owned()))?;
    let attempts = if requirement == FreshnessRequirement::AllowStale {
        1
    } else {
        MAX_FRESH_SNAPSHOT_ATTEMPTS
    };
    let requested_scope = scope.clone();
    let mut last_error = None;

    for _ in 0..attempts {
        operation.check()?;
        let snapshot = query_snapshot(engine, requirement, operation)?;
        let diff = match provider.diff_with_context(
            DiffWorkspace::from_snapshot_with_context(
                &snapshot,
                requested_scope.clone(),
                operation,
            )?,
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
        if diff.scope.requested != requested_scope {
            last_error = Some(QueryError::DiffUnavailable(
                "provider returned a different diff scope than requested".to_owned(),
            ));
            continue;
        }
        if !matches!(requested_scope, DiffScope::Worktree) && diff.scope.base_commit.is_none() {
            last_error = Some(QueryError::DiffUnavailable(
                "provider returned an explicit diff scope without a resolved base commit"
                    .to_owned(),
            ));
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

struct QueryConstruction<'a> {
    query: &'static str,
    started: Instant,
    bounded_section_bytes: usize,
    work: &'a QueryWorkStats,
}

fn envelope<T>(
    construction: QueryConstruction<'_>,
    snapshot: &WorkspaceSnapshot,
    provider_state: ProviderState,
    truncation: Vec<TruncationDetail>,
    data: T,
) -> QueryEnvelope<T> {
    tracing::debug!(
        query = construction.query,
        construction_micros =
            u64::try_from(construction.started.elapsed().as_micros()).unwrap_or(u64::MAX),
        bounded_section_bytes = construction.bounded_section_bytes,
        files_examined = construction.work.files_examined,
        symbols_examined = construction.work.symbols_examined,
        candidates_examined = construction.work.candidates_examined,
        edges_visited = construction.work.edges_visited,
        call_sites_visited = construction.work.call_sites_visited,
        intermediate_items_retained = construction.work.intermediate_items_retained,
        diff_wait_micros = construction.work.diff_wait_micros,
        provider_wait_micros = construction.work.provider_wait_micros,
        "query response constructed"
    );
    QueryEnvelope::new(
        snapshot.identity().workspace.clone(),
        snapshot.revision(),
        snapshot.freshness(),
        snapshot.status(),
        provider_state,
        truncation,
        data,
    )
    .with_indexing(snapshot.indexing().clone())
}

fn bounded_match_line(line: &str, match_start: usize, match_end: usize) -> (String, Option<usize>) {
    let total_chars = line.chars().count();
    if total_chars <= MAX_MATCH_LINE_CHARS {
        return (line.to_owned(), None);
    }

    let match_start_char = line[..match_start].chars().count();
    let match_end_char = line[..match_end].chars().count();
    let match_chars = match_end_char.saturating_sub(match_start_char);
    let surrounding = MAX_MATCH_LINE_CHARS.saturating_sub(match_chars);
    let start_char = match_start_char.saturating_sub(surrounding / 2);
    let end_char = (start_char + MAX_MATCH_LINE_CHARS).max(match_end_char);
    let end_char = end_char.min(total_chars);
    let start_char = end_char.saturating_sub(MAX_MATCH_LINE_CHARS);
    let snippet: String = line
        .chars()
        .skip(start_char)
        .take(end_char - start_char)
        .collect();
    let retained_chars = snippet.chars().count();
    (snippet, Some(total_chars.saturating_sub(retained_chars)))
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

fn source_snippet(
    graph: &SymbolGraph,
    symbol: &Symbol,
) -> (Option<SourceSnippet>, Option<TruncationDetail>) {
    let range = &symbol.location;
    let Some(source) = graph.file_source(range.file()) else {
        return (None, None);
    };
    let Some(start_byte) = position_byte_offset(source, range.start()) else {
        return (None, None);
    };
    let Some(end_byte) = position_byte_offset(source, range.end()) else {
        return (None, None);
    };
    let Some(full) = source.get(start_byte..end_byte) else {
        return (None, None);
    };

    let mut end = full.len();
    let mut lines = 1_usize;
    let mut cause = None;
    for (chars, (offset, character)) in full.char_indices().enumerate() {
        if chars >= MAX_SNIPPET_CHARS {
            end = offset;
            cause = Some((
                TruncationCause::SourceSnippetCharacterLimit,
                MAX_SNIPPET_CHARS,
            ));
            break;
        }
        if character == '\n' && lines >= MAX_SNIPPET_LINES {
            end = offset;
            cause = Some((TruncationCause::SourceSnippetLineLimit, MAX_SNIPPET_LINES));
            break;
        }
        if character == '\n' {
            lines += 1;
        }
    }
    let Some(text) = full.get(..end).map(str::to_owned) else {
        return (None, None);
    };
    let truncated = end < full.len();
    let Some(snippet_end) = advance_position(range.start(), &text) else {
        return (None, None);
    };
    let Ok(snippet_range) = SourceRange::new(range.file().clone(), range.start(), snippet_end)
    else {
        return (None, None);
    };
    let detail = cause.map(|(cause, limit)| {
        TruncationDetail::new(
            TruncationSection::ContextSource,
            cause,
            limit,
            Some(full.chars().count().saturating_sub(text.chars().count())),
        )
    });
    (
        Some(SourceSnippet {
            range: snippet_range,
            text,
            truncated,
            provenance: symbol.provenance,
            precision: symbol.precision,
        }),
        detail,
    )
}

fn byte_bounded_source(
    source: Option<SourceSnippet>,
) -> Result<(Option<SourceSnippet>, Option<TruncationDetail>, usize), QueryError> {
    let Some(source) = source else {
        return Ok((None, None, serialized_len(&Option::<SourceSnippet>::None)?));
    };
    let original_bytes = serialized_len(&source)?;
    if original_bytes <= CONTEXT_SOURCE_BYTES {
        return Ok((Some(source), None, original_bytes));
    }

    let mut boundaries: Vec<usize> = source.text.char_indices().map(|(index, _)| index).collect();
    boundaries.push(source.text.len());
    let character_count = boundaries.len().saturating_sub(1);
    let mut low = 0_usize;
    let mut high = character_count;
    let mut best = None::<(SourceSnippet, usize)>;
    while low <= high {
        let midpoint = low + (high - low) / 2;
        let Some(text) = source.text.get(..boundaries[midpoint]) else {
            break;
        };
        let Some(end) = advance_position(source.range.start(), text) else {
            break;
        };
        let Ok(range) = SourceRange::new(source.range.file().clone(), source.range.start(), end)
        else {
            break;
        };
        let candidate = SourceSnippet {
            range,
            text: text.to_owned(),
            truncated: true,
            provenance: source.provenance,
            precision: source.precision,
        };
        let bytes = serialized_len(&candidate)?;
        if bytes <= CONTEXT_SOURCE_BYTES {
            best = Some((candidate, bytes));
            low = midpoint.saturating_add(1);
        } else if midpoint == 0 {
            break;
        } else {
            high = midpoint - 1;
        }
    }

    let (source, retained_bytes) = best.map_or((None, 4), |(source, bytes)| (Some(source), bytes));
    Ok((
        source,
        Some(TruncationDetail::new(
            TruncationSection::ContextSource,
            TruncationCause::ResponseByteLimit,
            CONTEXT_SOURCE_BYTES,
            Some(original_bytes.saturating_sub(retained_bytes)),
        )),
        retained_bytes,
    ))
}

impl QueryService for WorkspaceEngine {
    fn status(&self, _request: StatusRequest) -> Result<QueryEnvelope<StatusData>, QueryError> {
        let started = Instant::now();
        let work_stats = QueryWorkStats::default();
        let snapshot = self.snapshot();
        let provider_state = provider_state_for(self, &snapshot);
        let graph_diagnostics = snapshot.graph().syntax_diagnostics(MAX_STATUS_DIAGNOSTICS);
        let mut truncation_causes = Vec::new();
        if graph_diagnostics.capture_omitted > 0 {
            truncation_causes.push(DiagnosticTruncationCause::PerFileLimit);
        }
        if graph_diagnostics.response_omitted > 0 {
            truncation_causes.push(DiagnosticTruncationCause::StatusLimit);
        }
        let captured_diagnostic_count = graph_diagnostics.diagnostics.len();
        let diagnostics = bounded_section(
            graph_diagnostics.diagnostics,
            MAX_STATUS_DIAGNOSTICS,
            STATUS_DIAGNOSTICS_BYTES,
            TruncationSection::StatusSyntaxDiagnostics,
        )?;
        let byte_omitted_diagnostics =
            captured_diagnostic_count.saturating_sub(diagnostics.items.len());
        if byte_omitted_diagnostics > 0 {
            truncation_causes.push(DiagnosticTruncationCause::ResponseByteLimit);
        }
        let omitted_diagnostics = graph_diagnostics
            .capture_omitted
            .saturating_add(graph_diagnostics.response_omitted)
            .saturating_add(u64::try_from(byte_omitted_diagnostics).unwrap_or(u64::MAX));
        let diagnostics_truncated = omitted_diagnostics > 0;
        let mut truncation = Vec::new();
        if graph_diagnostics.capture_omitted > 0 {
            truncation.push(TruncationDetail::new(
                TruncationSection::StatusSyntaxDiagnostics,
                TruncationCause::SyntaxDiagnosticPerFileLimit,
                MAX_SYNTAX_DIAGNOSTICS_PER_FILE,
                Some(usize::try_from(graph_diagnostics.capture_omitted).unwrap_or(usize::MAX)),
            ));
        }
        if graph_diagnostics.response_omitted > 0 {
            truncation.push(TruncationDetail::new(
                TruncationSection::StatusSyntaxDiagnostics,
                TruncationCause::ItemLimit,
                MAX_STATUS_DIAGNOSTICS,
                Some(usize::try_from(graph_diagnostics.response_omitted).unwrap_or(usize::MAX)),
            ));
        }
        truncation.extend(diagnostics.truncation.iter().cloned());
        let counts = IndexCounts {
            files: snapshot.graph().file_count(),
            symbols: snapshot.graph().symbol_count(),
            edges: snapshot.graph().edge_count(),
            call_sites: snapshot.graph().call_site_count(),
            ambiguous_call_sites: snapshot.graph().ambiguous_call_site_count(),
            unresolved_call_sites: snapshot.graph().unresolved_call_site_count(),
            call_sites_with_truncated_candidates: snapshot.graph().truncated_call_sites(),
        };
        let provider = self.precise_provider();
        let providers = vec![ProviderInfo {
            name: "rust-analyzer".to_owned(),
            languages: vec![chakra_domain::symbol::Language::Rust],
            capabilities: vec![
                ProviderCapability::IncomingCalls,
                ProviderCapability::OutgoingCalls,
                ProviderCapability::SynchronizationState,
                ProviderCapability::ProgressReporting,
                ProviderCapability::RevisionDeltaSynchronization,
                ProviderCapability::CacheMetrics,
            ],
            state: provider_state,
            last_error: provider.and_then(|provider| provider.last_error()),
            progress: provider.and_then(|provider| provider.progress()),
            metrics: provider.and_then(|provider| provider.metrics()),
            query_wait_budget_millis: provider.and_then(|provider| {
                provider
                    .query_wait_budget()
                    .map(|budget| u64::try_from(budget.as_millis()).unwrap_or(u64::MAX))
            }),
        }];
        let providers = bounded_section(
            providers,
            MAX_QUERY_LIMIT as usize,
            STATUS_PROVIDERS_BYTES,
            TruncationSection::StatusProviders,
        )?;
        truncation.extend(providers.truncation.iter().cloned());
        let data = StatusData {
            workspace: snapshot.identity().clone(),
            counts,
            providers: providers.items,
            query_execution: None,
            source_metadata: snapshot.graph().source_metadata_coverage(),
            syntax_diagnostics: SyntaxDiagnosticSummary {
                files_with_diagnostics: graph_diagnostics.files_with_diagnostics,
                total_diagnostics: graph_diagnostics.total_diagnostics,
                diagnostics: diagnostics.items,
                omitted_diagnostics,
                truncated: diagnostics_truncated,
                truncation_causes,
            },
        };
        Ok(envelope(
            QueryConstruction {
                query: "status",
                started,
                bounded_section_bytes: providers
                    .retained_bytes
                    .saturating_add(diagnostics.retained_bytes),
                work: &work_stats,
            },
            &snapshot,
            provider_state,
            truncation,
            data,
        ))
    }

    fn repo_map(&self, request: RepoMapRequest) -> Result<QueryEnvelope<RepoMapData>, QueryError> {
        self.repo_map_with_context(request, &OperationContext::unbounded())
    }

    fn repo_map_with_context(
        &self,
        request: RepoMapRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<RepoMapData>, QueryError> {
        let started = Instant::now();
        if request.limit == Some(0) {
            return Err(QueryError::Invalid(
                "repo_map limit must be greater than zero".to_owned(),
            ));
        }
        let first_page = request.cursor.is_none();
        let (scope, cursor_workspace, cursor_revision, after) = if let Some(cursor) = request.cursor
        {
            if !request.include_languages.is_empty() || request.source != SourceFilter::default() {
                return Err(QueryError::Invalid(
                    "repo_map filters must be omitted when continuing with a cursor".to_owned(),
                ));
            }
            (
                PreparedRepoMapScope::new(cursor.scope)?,
                Some(cursor.workspace_id),
                Some(cursor.revision),
                Some(cursor.after),
            )
        } else {
            (
                PreparedRepoMapScope::new(RepoMapScope {
                    include_languages: request.include_languages,
                    source: request.source,
                })?,
                None,
                None,
                None,
            )
        };
        let snapshot = query_snapshot(self, request.freshness, operation)?;
        if let Some(cursor_workspace) = cursor_workspace
            && cursor_workspace != snapshot.identity().workspace
        {
            return Err(QueryError::CursorWorkspaceMismatch {
                cursor_workspace,
                current_workspace: snapshot.identity().workspace.clone(),
            });
        }
        if let Some(cursor_revision) = cursor_revision
            && cursor_revision != snapshot.revision()
        {
            return Err(QueryError::StaleCursor {
                cursor_revision,
                current_revision: snapshot.revision(),
            });
        }
        let limit = clamp_limit(request.limit);
        let known_total_matching = (first_page && scope.normalized() == RepoMapScope::default())
            .then(|| usize::try_from(snapshot.graph().file_count()).unwrap_or(usize::MAX));
        let mut work = SectionWorkBudget::new(limit);
        let mut filtered = Vec::new();
        let mut poll = CancellationPoll::new();
        for summary in snapshot.graph().file_summaries_iter() {
            poll.observe(operation)?;
            if !work.check_wall_time() {
                break;
            }
            if after.as_ref().is_some_and(|after| &summary.path <= after) {
                continue;
            }
            let Some(language) = file_language(&summary.path) else {
                continue;
            };
            if !scope.matches(&summary.path, language, &summary.metadata) {
                continue;
            }
            if !work.record_examined(ExaminedKind::File) || !work.retain_intermediate() {
                break;
            }
            filtered.push((summary, language));
        }
        filtered.sort_by(|(left, _), (right, _)| left.path.cmp(&right.path));
        let overview_items = if first_page {
            repo_map_overview(&filtered)
        } else {
            Vec::new()
        };
        let overview = bounded_section(
            overview_items,
            limit,
            REPO_MAP_OVERVIEW_BYTES,
            TruncationSection::RepoMapOverview,
        )?;
        let files = filtered
            .into_iter()
            .map(|(summary, language)| FileSummary {
                path: summary.path,
                language,
                symbol_count: summary.symbol_count,
                provenance: summary.provenance,
                precision: summary.precision,
                source_role: summary.metadata.role,
                source_classification: summary.metadata.classification,
                package: summary.metadata.package,
            })
            .collect();
        let mut files = bounded_section(
            files,
            limit,
            REPO_MAP_FILES_BYTES,
            TruncationSection::RepoMapFiles,
        )?;
        if let Some(total) = known_total_matching
            && let Some(detail) = files
                .truncation
                .iter_mut()
                .find(|detail| detail.cause == TruncationCause::ItemLimit)
        {
            detail.omitted = Some(u64::try_from(total.saturating_sub(limit)).unwrap_or(u64::MAX));
        }
        let next_cursor = (!files.truncation.is_empty() || work.stopped.is_some())
            .then(|| {
                files.items.last().map(|summary| RepoMapCursor {
                    workspace_id: snapshot.identity().workspace.clone(),
                    revision: snapshot.revision(),
                    after: summary.path.clone(),
                    scope: scope.normalized(),
                })
            })
            .flatten();
        operation.check()?;
        let mut truncation = overview.truncation.clone();
        truncation.extend(files.truncation.iter().cloned());
        truncation.extend(work.truncation(TruncationSection::RepoMapFiles));
        if first_page {
            truncation.extend(work.truncation(TruncationSection::RepoMapOverview));
        }
        let retained_bytes = overview.retained_bytes.saturating_add(files.retained_bytes);
        let overview_truncated = !overview.truncation.is_empty() || work.stopped.is_some();
        let mut work_stats = QueryWorkStats::default();
        work.add_to_stats(&mut work_stats);
        Ok(envelope(
            QueryConstruction {
                query: "repo_map",
                started,
                bounded_section_bytes: retained_bytes,
                work: &work_stats,
            },
            &snapshot,
            provider_state_for(self, &snapshot),
            truncation,
            RepoMapData {
                overview: overview.items,
                overview_truncated,
                files: files.items,
                next_cursor,
                source_metadata: snapshot.graph().source_metadata_coverage(),
            },
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
        let started = Instant::now();
        let work_stats = QueryWorkStats::default();
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
        let mut poll = CancellationPoll::new();
        let mut matches_truncated = false;
        let mut line_omitted = 0_usize;

        'files: for (path, source) in snapshot.graph().source_files_with_context(operation)? {
            for (line_index, line) in source.lines().enumerate() {
                poll.observe(operation)?;
                for found in matcher.find_iter(line) {
                    poll.observe(operation)?;
                    if matches.len() >= limit {
                        matches_truncated = true;
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
                    let (line, omitted) = bounded_match_line(line, found.start(), found.end());
                    line_omitted = line_omitted.saturating_add(omitted.unwrap_or_default());
                    matches.push(TextMatch {
                        file: path.clone(),
                        range,
                        line,
                        line_truncated: omitted.is_some(),
                        provenance: Provenance::TextSearch,
                        precision: Precision::Textual,
                    });
                }
            }
        }
        let matches = bounded_section(
            matches,
            limit,
            SEARCH_MATCHES_BYTES,
            TruncationSection::SearchMatches,
        )?;
        let mut truncation = matches.truncation;
        if matches_truncated {
            truncation.push(TruncationDetail::new(
                TruncationSection::SearchMatches,
                TruncationCause::ItemLimit,
                limit,
                None,
            ));
        }
        if line_omitted > 0 {
            truncation.push(TruncationDetail::new(
                TruncationSection::SearchMatchLine,
                TruncationCause::SourceSnippetCharacterLimit,
                MAX_MATCH_LINE_CHARS,
                Some(line_omitted),
            ));
        }
        Ok(envelope(
            QueryConstruction {
                query: "search",
                started,
                bounded_section_bytes: matches.retained_bytes,
                work: &work_stats,
            },
            &snapshot,
            provider_state_for(self, &snapshot),
            truncation,
            SearchData {
                matches: matches.items,
            },
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
        mut request: SymbolSearchRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<SymbolSearchData>, QueryError> {
        let started = Instant::now();
        let mut work_stats = QueryWorkStats::default();
        let query = request.query.trim().to_owned();
        if query.is_empty() {
            return Err(QueryError::Invalid("query must be non-empty".to_owned()));
        }
        if query.chars().count() > MAX_QUERY_PATTERN_CHARS {
            return Err(QueryError::Invalid(format!(
                "query exceeds the {MAX_QUERY_PATTERN_CHARS}-character pattern budget"
            )));
        }
        let filter = PreparedSymbolFilter::new(&mut request)?;
        let snapshot = query_snapshot(self, request.freshness, operation)?;
        let limit = clamp_limit(request.limit);
        let mut work = SectionWorkBudget::new(limit);
        let (matches, truncated) = ranked_symbol_matches(
            snapshot.graph(),
            &query,
            &filter,
            limit,
            operation,
            &mut work,
        )?;
        work.add_to_stats(&mut work_stats);
        let candidates: Vec<SymbolView> = matches
            .into_iter()
            .filter_map(|id| snapshot.graph().symbol(id))
            .map(|symbol| symbol_view(snapshot.graph(), symbol))
            .collect();
        let candidates = bounded_section(
            candidates,
            limit,
            SYMBOL_SEARCH_CANDIDATES_BYTES,
            TruncationSection::SymbolSearchCandidates,
        )?;
        let data = SymbolSearchData {
            query,
            candidates: candidates.items,
        };
        let mut truncation = candidates.truncation;
        truncation.extend(truncated.then(|| {
            TruncationDetail::new(
                TruncationSection::SymbolSearchCandidates,
                TruncationCause::ItemLimit,
                limit,
                None,
            )
        }));
        truncation.extend(work.truncation(TruncationSection::SymbolSearchCandidates));
        Ok(envelope(
            QueryConstruction {
                query: "symbol_search",
                started,
                bounded_section_bytes: candidates.retained_bytes,
                work: &work_stats,
            },
            &snapshot,
            provider_state_for(self, &snapshot),
            truncation,
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
        let started = Instant::now();
        let reference = request
            .symbol
            .as_ref()
            .ok_or(QueryError::MissingSymbolRef)?;
        let snapshot = query_snapshot(self, request.freshness, operation)?;
        let graph = snapshot.graph();
        let symbol = resolve(graph, reference, snapshot.revision(), operation)?;
        let limit = clamp_limit(request.limit);
        let mut work_stats = QueryWorkStats::default();

        let mut callers_work = SectionWorkBudget::new(limit);
        let RelatedViews {
            items: mut callers,
            item_truncated: callers_item_truncated,
        } = related_from_edges(
            graph,
            graph.incoming_edges(symbol.id),
            &[EdgeKind::Calls],
            RelatedEndpoint::From,
            limit,
            &mut callers_work,
            operation,
        )?;
        callers_work.add_to_stats(&mut work_stats);

        let mut callees_work = SectionWorkBudget::new(limit);
        let RelatedViews {
            items: mut callees,
            item_truncated: callees_item_truncated,
        } = related_from_edges(
            graph,
            graph.outgoing_edges(symbol.id),
            &[EdgeKind::Calls],
            RelatedEndpoint::To,
            limit,
            &mut callees_work,
            operation,
        )?;
        callees_work.add_to_stats(&mut work_stats);
        let mut provider_state = provider_state_for_language(self, &snapshot, symbol.key.language);
        let mut provider_incoming_truncated = false;
        let mut provider_outgoing_truncated = false;
        if snapshot.freshness() == Freshness::Fresh
            && let Some(provider) = self
                .precise_provider()
                .filter(|provider| provider.supports(symbol.key.language))
        {
            let provider_started = Instant::now();
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
            work_stats.provider_wait_micros = work_stats.provider_wait_micros.saturating_add(
                u64::try_from(provider_started.elapsed().as_micros()).unwrap_or(u64::MAX),
            );
            provider_state = if precise_result_is_current(self, &snapshot, result.revision) {
                result.state
            } else {
                ProviderState::CatchingUp
            };
            if provider_state == ProviderState::Ready {
                provider_incoming_truncated = result.incoming_truncated;
                provider_outgoing_truncated = result.outgoing_truncated;
                callers = merge_precise(graph, callers, result.incoming);
                callees = merge_precise(graph, callees, result.outgoing);
            }
        }
        sort_related(&mut callers);
        sort_related(&mut callees);
        let resolved_caller_ids: std::collections::HashSet<_> =
            callers.iter().map(|caller| caller.symbol.id).collect();
        let resolved_callee_ids: std::collections::HashSet<_> =
            callees.iter().map(|callee| callee.symbol.id).collect();
        let BoundedSection {
            items: callers,
            truncation: callers_truncation,
            retained_bytes: callers_bytes,
        } = bounded_section(
            callers,
            limit,
            CONTEXT_CALLERS_BYTES,
            TruncationSection::ContextCallers,
        )?;
        let callers_response_item_truncated = callers_truncation
            .iter()
            .any(|detail| detail.cause == TruncationCause::ItemLimit);
        let BoundedSection {
            items: callees,
            truncation: callees_truncation,
            retained_bytes: callees_bytes,
        } = bounded_section(
            callees,
            limit,
            CONTEXT_CALLEES_BYTES,
            TruncationSection::ContextCallees,
        )?;
        let callees_response_item_truncated = callees_truncation
            .iter()
            .any(|detail| detail.cause == TruncationCause::ItemLimit);

        let mut implementations_work = SectionWorkBudget::new(limit);
        let RelatedViews {
            items: mut implementations,
            item_truncated: implementations_item_truncated,
        } = related_from_edges(
            graph,
            graph.incoming_edges(symbol.id),
            &[EdgeKind::Implements],
            RelatedEndpoint::From,
            limit,
            &mut implementations_work,
            operation,
        )?;
        implementations_work.add_to_stats(&mut work_stats);
        sort_related(&mut implementations);
        let BoundedSection {
            items: implementations,
            truncation: implementations_truncation,
            retained_bytes: implementations_bytes,
        } = bounded_section(
            implementations,
            limit,
            CONTEXT_IMPLEMENTATIONS_BYTES,
            TruncationSection::ContextImplementations,
        )?;
        let implementations_response_item_truncated = implementations_truncation
            .iter()
            .any(|detail| detail.cause == TruncationCause::ItemLimit);

        let mut tests_work = SectionWorkBudget::new(limit);
        let RelatedViews {
            items: mut tests,
            item_truncated: tests_item_truncated,
        } = related_from_edges(
            graph,
            graph.incoming_edges(symbol.id),
            &[EdgeKind::Tests],
            RelatedEndpoint::From,
            limit,
            &mut tests_work,
            operation,
        )?;
        tests_work.add_to_stats(&mut work_stats);
        sort_tests(&mut tests);
        let BoundedSection {
            items: tests,
            truncation: tests_truncation,
            retained_bytes: tests_bytes,
        } = bounded_section(
            tests,
            limit,
            CONTEXT_TESTS_BYTES,
            TruncationSection::ContextTests,
        )?;
        let tests_response_item_truncated = tests_truncation
            .iter()
            .any(|detail| detail.cause == TruncationCause::ItemLimit);

        let mut relations_work = SectionWorkBudget::new(limit);
        let incoming_relations = related_from_edges(
            graph,
            graph.incoming_edges(symbol.id),
            CONTEXT_RELATION_KINDS,
            RelatedEndpoint::From,
            limit,
            &mut relations_work,
            operation,
        )?;
        let outgoing_relations = related_from_edges(
            graph,
            graph.outgoing_edges(symbol.id),
            CONTEXT_RELATION_KINDS,
            RelatedEndpoint::To,
            limit,
            &mut relations_work,
            operation,
        )?;
        let relations_item_truncated =
            incoming_relations.item_truncated || outgoing_relations.item_truncated;
        let mut related_relations: Vec<_> = incoming_relations
            .items
            .into_iter()
            .map(|relation| DirectedRelatedSymbol {
                direction: RelationDirection::Incoming,
                relation,
            })
            .collect();
        related_relations.extend(outgoing_relations.items.into_iter().map(|relation| {
            DirectedRelatedSymbol {
                direction: RelationDirection::Outgoing,
                relation,
            }
        }));
        relations_work.add_to_stats(&mut work_stats);
        sort_directed_related(&mut related_relations);
        let BoundedSection {
            items: related_relations,
            truncation: related_relations_truncation,
            retained_bytes: related_relations_bytes,
        } = bounded_section(
            related_relations,
            limit,
            CONTEXT_RELATED_RELATIONS_BYTES,
            TruncationSection::ContextRelatedRelations,
        )?;
        let relations_response_item_truncated = related_relations_truncation
            .iter()
            .any(|detail| detail.cause == TruncationCause::ItemLimit);

        let mut candidates_work = SectionWorkBudget::new(limit);
        let outgoing_candidates =
            outgoing_call_candidates(graph, symbol.id, limit, operation, &mut candidates_work)?;
        let incoming_candidates =
            incoming_call_candidates(graph, symbol.id, operation, &mut candidates_work)?;
        candidates_work.add_to_stats(&mut work_stats);
        let mut syntax_call_candidates = outgoing_candidates.items;
        syntax_call_candidates.extend(
            incoming_candidates
                .items
                .into_iter()
                .filter(|candidate| candidate.caller.id != symbol.id),
        );
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
        let BoundedSection {
            items: syntax_call_candidates,
            truncation: candidates_truncation,
            retained_bytes: candidates_bytes,
        } = bounded_section(
            syntax_call_candidates,
            limit,
            CONTEXT_CALL_CANDIDATES_BYTES,
            TruncationSection::ContextSyntaxCallCandidates,
        )?;

        let mut related_files: Vec<chakra_domain::location::RepoRelativePath> = callers
            .iter()
            .chain(callees.iter())
            .chain(implementations.iter())
            .chain(tests.iter())
            .map(|item| item.symbol.location.file().clone())
            .collect();
        related_files.extend(
            related_relations
                .iter()
                .map(|item| item.relation.symbol.location.file().clone()),
        );
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
        let BoundedSection {
            items: related_files,
            truncation: files_truncation,
            retained_bytes: related_files_bytes,
        } = bounded_section(
            related_files,
            limit,
            CONTEXT_RELATED_FILES_BYTES,
            TruncationSection::ContextRelatedFiles,
        )?;

        let (source, source_truncation) = source_snippet(graph, symbol);
        let (source, source_byte_truncation, source_bytes) = byte_bounded_source(source)?;
        let mut truncation = Vec::new();
        truncation.extend(callers_truncation);
        truncation.extend(callees_truncation);
        truncation.extend(implementations_truncation);
        truncation.extend(tests_truncation);
        truncation.extend(related_relations_truncation);
        truncation.extend(candidates_truncation);
        truncation.extend(files_truncation);
        truncation.extend(callers_work.truncation(TruncationSection::ContextCallers));
        truncation.extend(callees_work.truncation(TruncationSection::ContextCallees));
        truncation
            .extend(implementations_work.truncation(TruncationSection::ContextImplementations));
        truncation.extend(tests_work.truncation(TruncationSection::ContextTests));
        truncation.extend(relations_work.truncation(TruncationSection::ContextRelatedRelations));
        truncation
            .extend(candidates_work.truncation(TruncationSection::ContextSyntaxCallCandidates));
        for (section, was_truncated, response_already_truncated) in [
            (
                TruncationSection::ContextCallers,
                callers_item_truncated,
                callers_response_item_truncated,
            ),
            (
                TruncationSection::ContextCallees,
                callees_item_truncated,
                callees_response_item_truncated,
            ),
            (
                TruncationSection::ContextImplementations,
                implementations_item_truncated,
                implementations_response_item_truncated,
            ),
            (
                TruncationSection::ContextTests,
                tests_item_truncated,
                tests_response_item_truncated,
            ),
        ] {
            truncation.extend(
                (was_truncated && !response_already_truncated).then(|| {
                    TruncationDetail::new(section, TruncationCause::ItemLimit, limit, None)
                }),
            );
        }
        truncation.extend(
            (relations_item_truncated && !relations_response_item_truncated).then(|| {
                TruncationDetail::new(
                    TruncationSection::ContextRelatedRelations,
                    TruncationCause::ItemLimit,
                    limit,
                    None,
                )
            }),
        );
        truncation.extend(candidate_truncation(
            TruncationSection::ContextSyntaxCallCandidates,
            limit,
            outgoing_candidates.fanout_truncated,
        ));
        truncation.extend(
            (outgoing_candidates.item_truncated || incoming_candidates.item_truncated).then(|| {
                TruncationDetail::new(
                    TruncationSection::ContextSyntaxCallCandidates,
                    TruncationCause::ItemLimit,
                    limit,
                    None,
                )
            }),
        );
        truncation.extend(provider_incoming_truncated.then(|| {
            TruncationDetail::new(
                TruncationSection::ContextCallers,
                TruncationCause::ProviderLimit,
                limit,
                None,
            )
        }));
        truncation.extend(provider_outgoing_truncated.then(|| {
            TruncationDetail::new(
                TruncationSection::ContextCallees,
                TruncationCause::ProviderLimit,
                limit,
                None,
            )
        }));
        truncation.extend(source_truncation);
        truncation.extend(source_byte_truncation);
        let data = ContextData {
            symbol: symbol_view(graph, symbol),
            source,
            callers,
            callees,
            implementations,
            tests,
            related_relations,
            syntax_call_candidates,
            related_files,
            provider: provider_query_info(self, symbol.key.language, provider_state),
        };
        Ok(envelope(
            QueryConstruction {
                query: "context",
                started,
                bounded_section_bytes: source_bytes
                    .saturating_add(callers_bytes)
                    .saturating_add(callees_bytes)
                    .saturating_add(implementations_bytes)
                    .saturating_add(tests_bytes)
                    .saturating_add(related_relations_bytes)
                    .saturating_add(candidates_bytes)
                    .saturating_add(related_files_bytes),
                work: &work_stats,
            },
            &snapshot,
            provider_state,
            truncation,
            data,
        ))
    }

    fn callers(&self, request: CallersRequest) -> Result<QueryEnvelope<CallersData>, QueryError> {
        self.callers_with_context(request, &OperationContext::unbounded())
    }

    fn callers_with_context(
        &self,
        request: CallersRequest,
        operation: &OperationContext,
    ) -> Result<QueryEnvelope<CallersData>, QueryError> {
        let started = Instant::now();
        let reference = request
            .symbol
            .as_ref()
            .ok_or(QueryError::MissingSymbolRef)?;
        let snapshot = query_snapshot(self, request.freshness, operation)?;
        let graph = snapshot.graph();
        let target = resolve(graph, reference, snapshot.revision(), operation)?;
        let limit = clamp_limit(request.limit);
        let mut work_stats = QueryWorkStats::default();
        let mut callers_work = SectionWorkBudget::new(limit);
        let RelatedViews {
            items: mut callers,
            item_truncated: callers_item_truncated,
        } = related_from_edges(
            graph,
            graph.incoming_edges(target.id),
            &[EdgeKind::Calls],
            RelatedEndpoint::From,
            limit,
            &mut callers_work,
            operation,
        )?;
        callers_work.add_to_stats(&mut work_stats);
        let mut provider_state = provider_state_for_language(self, &snapshot, target.key.language);
        let mut provider_truncated = false;
        let mut candidates_work = SectionWorkBudget::new(limit);
        let candidate_views =
            incoming_call_candidates(graph, target.id, operation, &mut candidates_work)?;
        candidates_work.add_to_stats(&mut work_stats);
        let mut syntax_candidates = candidate_views.items;
        if snapshot.freshness() == Freshness::Fresh
            && let Some(provider) = self
                .precise_provider()
                .filter(|provider| provider.supports(target.key.language))
        {
            let provider_started = Instant::now();
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
            work_stats.provider_wait_micros = work_stats.provider_wait_micros.saturating_add(
                u64::try_from(provider_started.elapsed().as_micros()).unwrap_or(u64::MAX),
            );
            provider_state = if precise_result_is_current(self, &snapshot, result.revision) {
                result.state
            } else {
                ProviderState::CatchingUp
            };
            if provider_state == ProviderState::Ready {
                provider_truncated = result.incoming_truncated;
                callers = merge_precise(graph, callers, result.incoming);
            }
        }
        let resolved_caller_ids: std::collections::HashSet<_> =
            callers.iter().map(|caller| caller.symbol.id).collect();
        syntax_candidates.retain(|candidate| !resolved_caller_ids.contains(&candidate.caller.id));
        sort_related(&mut callers);
        let BoundedSection {
            items: callers,
            truncation: callers_truncation,
            retained_bytes: callers_bytes,
        } = bounded_section(
            callers,
            limit,
            CALLERS_CALLERS_BYTES,
            TruncationSection::CallersCallers,
        )?;
        let callers_response_item_truncated = callers_truncation
            .iter()
            .any(|detail| detail.cause == TruncationCause::ItemLimit);
        let BoundedSection {
            items: syntax_candidates,
            truncation: candidates_truncation,
            retained_bytes: candidates_bytes,
        } = bounded_section(
            syntax_candidates,
            limit,
            CALLERS_CANDIDATES_BYTES,
            TruncationSection::CallersSyntaxCandidates,
        )?;
        let mut truncation = callers_truncation;
        truncation.extend(candidates_truncation);
        truncation.extend(callers_work.truncation(TruncationSection::CallersCallers));
        truncation.extend(candidates_work.truncation(TruncationSection::CallersSyntaxCandidates));
        truncation.extend(
            (callers_item_truncated && !callers_response_item_truncated).then(|| {
                TruncationDetail::new(
                    TruncationSection::CallersCallers,
                    TruncationCause::ItemLimit,
                    limit,
                    None,
                )
            }),
        );
        truncation.extend(candidate_views.item_truncated.then(|| {
            TruncationDetail::new(
                TruncationSection::CallersSyntaxCandidates,
                TruncationCause::ItemLimit,
                limit,
                None,
            )
        }));
        truncation.extend(provider_truncated.then(|| {
            TruncationDetail::new(
                TruncationSection::CallersCallers,
                TruncationCause::ProviderLimit,
                limit,
                None,
            )
        }));
        let data = CallersData {
            target: symbol_view(graph, target),
            callers,
            syntax_candidates,
            provider: provider_query_info(self, target.key.language, provider_state),
        };
        Ok(envelope(
            QueryConstruction {
                query: "callers",
                started,
                bounded_section_bytes: callers_bytes.saturating_add(candidates_bytes),
                work: &work_stats,
            },
            &snapshot,
            provider_state,
            truncation,
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
        let started = Instant::now();
        let mut work_stats = QueryWorkStats::default();
        let diff_started = Instant::now();
        let (snapshot, diff) =
            query_workspace_diff(self, request.freshness, operation, request.scope)?;
        work_stats.diff_wait_micros =
            u64::try_from(diff_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let graph = snapshot.graph();
        let limit = clamp_limit(request.limit);
        let diff_inventory_truncation = diff.truncation;
        let total_diff_files = diff.files.len();
        let mut files_work = SectionWorkBudget::new(limit);
        let mut changed_files = Vec::with_capacity(total_diff_files.min(files_work.examined_limit));
        for change in diff.files {
            if !files_work.record_examined(ExaminedKind::File) {
                break;
            }
            if !files_work.retain_intermediate() {
                break;
            }
            changed_files.push(ChangedFile {
                path: change.path,
                previous_path: change.previous_path,
                change: change.change,
                provenance: change.provenance,
                precision: change.precision,
            });
        }
        if changed_files.len() < total_diff_files {
            files_work.mark_examined_exhausted();
        }
        changed_files.sort_by(|a, b| a.path.cmp(&b.path));
        // Response-byte truncation remains local to `changed_files`; the
        // deterministic item-scoped prefix drives downstream symbol work.
        let scoped_changed_paths: Vec<_> = changed_files
            .iter()
            .take(limit)
            .filter(|change| change.change != chakra_domain::query::ChangeKind::Deleted)
            .map(|change| change.path.clone())
            .collect();
        files_work.add_to_stats(&mut work_stats);
        let BoundedSection {
            items: changed_files,
            truncation: files_truncation,
            retained_bytes: changed_files_bytes,
        } = bounded_section(
            changed_files,
            limit,
            DIFF_CHANGED_FILES_BYTES,
            TruncationSection::DiffContextChangedFiles,
        )?;

        let mut symbols_work = SectionWorkBudget::new(limit);
        let mut symbol_ids = Vec::new();
        'files: for path in &scoped_changed_paths {
            for symbol in graph.symbols_in_file(path) {
                if !symbols_work.record_examined(ExaminedKind::Symbol) {
                    break 'files;
                }
                if !symbols_work.retain_intermediate() {
                    break 'files;
                }
                symbol_ids.push(symbol.id);
            }
        }
        symbol_ids.sort_by(|a, b| {
            let a = graph.symbol(*a);
            let b = graph.symbol(*b);
            a.map(|symbol| (&symbol.key.qualified_name, symbol.id))
                .cmp(&b.map(|symbol| (&symbol.key.qualified_name, symbol.id)))
        });
        symbol_ids.dedup();
        let scoped_symbol_ids: Vec<_> = symbol_ids.iter().take(limit).copied().collect();
        let changed_symbols = symbol_ids
            .iter()
            .filter_map(|id| graph.symbol(*id))
            .map(|symbol| ChangedSymbol {
                symbol: symbol_view(graph, symbol),
                basis: ChangedSymbolBasis::DeclaredInChangedFile,
                provenance: Provenance::Heuristic,
                precision: Precision::Heuristic,
            })
            .collect();
        symbols_work.add_to_stats(&mut work_stats);
        let BoundedSection {
            items: changed_symbols,
            truncation: symbols_truncation,
            retained_bytes: changed_symbols_bytes,
        } = bounded_section(
            changed_symbols,
            limit,
            DIFF_CHANGED_SYMBOLS_BYTES,
            TruncationSection::DiffContextChangedSymbols,
        )?;
        let mut callers = BTreeMap::new();
        let mut tests = BTreeMap::new();
        let mut relations = Vec::new();
        let mut call_candidates = Vec::with_capacity(limit.saturating_add(1));
        let mut call_candidates_fanout_truncated = false;
        let mut call_candidates_item_truncated = false;
        let mut callers_work = SectionWorkBudget::new(limit);
        let mut tests_work = SectionWorkBudget::new(limit);
        let mut relations_work = SectionWorkBudget::new(limit);
        let mut callers_item_truncated = false;
        let mut tests_item_truncated = false;
        let mut relations_item_truncated = false;
        let mut candidates_work = SectionWorkBudget::new(limit);
        let mut poll = CancellationPoll::new();
        for id in &scoped_symbol_ids {
            poll.observe(operation)?;
            if callers_work.stopped.is_some()
                && tests_work.stopped.is_some()
                && relations_work.stopped.is_some()
                && candidates_work.stopped.is_some()
            {
                break;
            }
            let call_relations = related_from_edges(
                graph,
                graph.incoming_edges(*id),
                &[EdgeKind::Calls],
                RelatedEndpoint::From,
                limit,
                &mut callers_work,
                operation,
            )?;
            callers_item_truncated |= call_relations.item_truncated;
            for item in call_relations.items {
                let diff_relation = DiffRelatedSymbol {
                    changed_symbol_id: *id,
                    relation: item,
                };
                callers
                    .entry((diff_relation.relation.symbol.id, *id))
                    .and_modify(|existing: &mut DiffRelatedSymbol| {
                        merge_related_occurrences(
                            &mut existing.relation,
                            diff_relation.relation.clone(),
                        );
                    })
                    .or_insert(diff_relation);
            }
            let test_relations = related_from_edges(
                graph,
                graph.incoming_edges(*id),
                &[EdgeKind::Tests],
                RelatedEndpoint::From,
                limit,
                &mut tests_work,
                operation,
            )?;
            tests_item_truncated |= test_relations.item_truncated;
            for item in test_relations.items {
                let diff_relation = DiffRelatedSymbol {
                    changed_symbol_id: *id,
                    relation: item,
                };
                tests
                    .entry((diff_relation.relation.symbol.id, *id))
                    .and_modify(|existing: &mut DiffRelatedSymbol| {
                        merge_related_occurrences(
                            &mut existing.relation,
                            diff_relation.relation.clone(),
                        );
                    })
                    .or_insert(diff_relation);
            }
            for (direction, endpoint, edges) in [
                (
                    RelationDirection::Incoming,
                    RelatedEndpoint::From,
                    graph.incoming_edges(*id),
                ),
                (
                    RelationDirection::Outgoing,
                    RelatedEndpoint::To,
                    graph.outgoing_edges(*id),
                ),
            ] {
                let related = related_from_edges(
                    graph,
                    edges,
                    CONTEXT_RELATION_KINDS,
                    endpoint,
                    limit,
                    &mut relations_work,
                    operation,
                )?;
                relations_item_truncated |= related.item_truncated;
                relations.extend(related.items.into_iter().map(|relation| {
                    DiffDirectedRelatedSymbol {
                        changed_symbol_id: *id,
                        relation: DirectedRelatedSymbol {
                            direction,
                            relation,
                        },
                    }
                }));
            }
            let candidates = incoming_call_candidates(graph, *id, operation, &mut candidates_work)?;
            call_candidates_item_truncated |= candidates.item_truncated;
            call_candidates_fanout_truncated |= candidates.fanout_truncated;
            call_candidates.extend(candidates.items.into_iter().map(|call_site| DiffCallSite {
                changed_symbol_id: *id,
                call_site,
            }));
        }
        callers_work.add_to_stats(&mut work_stats);
        tests_work.add_to_stats(&mut work_stats);
        relations_work.add_to_stats(&mut work_stats);
        candidates_work.add_to_stats(&mut work_stats);

        let mut related_callers: Vec<_> = callers.into_values().collect();
        let mut related_tests: Vec<_> = tests.into_values().collect();
        sort_diff_related(&mut related_callers);
        sort_diff_related(&mut related_tests);
        let BoundedSection {
            items: related_callers,
            truncation: callers_truncation,
            retained_bytes: related_callers_bytes,
        } = bounded_section(
            related_callers,
            limit,
            DIFF_RELATED_CALLERS_BYTES,
            TruncationSection::DiffContextRelatedCallers,
        )?;
        let callers_response_item_truncated = callers_truncation
            .iter()
            .any(|detail| detail.cause == TruncationCause::ItemLimit);
        let BoundedSection {
            items: related_tests,
            truncation: tests_truncation,
            retained_bytes: related_tests_bytes,
        } = bounded_section(
            related_tests,
            limit,
            DIFF_RELATED_TESTS_BYTES,
            TruncationSection::DiffContextRelatedTests,
        )?;
        let tests_response_item_truncated = tests_truncation
            .iter()
            .any(|detail| detail.cause == TruncationCause::ItemLimit);
        sort_diff_directed_related(&mut relations);
        let BoundedSection {
            items: related_relations,
            truncation: relations_truncation,
            retained_bytes: related_relations_bytes,
        } = bounded_section(
            relations,
            limit,
            DIFF_RELATED_RELATIONS_BYTES,
            TruncationSection::DiffContextRelatedRelations,
        )?;
        let relations_response_item_truncated = relations_truncation
            .iter()
            .any(|detail| detail.cause == TruncationCause::ItemLimit);
        call_candidates.sort_by(|a, b| {
            a.changed_symbol_id
                .cmp(&b.changed_symbol_id)
                .then(a.call_site.caller.id.cmp(&b.call_site.caller.id))
                .then_with(|| {
                    let a = a
                        .call_site
                        .representative_evidence
                        .first()
                        .map(|evidence| (evidence.location.file(), evidence.location.start()));
                    let b = b
                        .call_site
                        .representative_evidence
                        .first()
                        .map(|evidence| (evidence.location.file(), evidence.location.start()));
                    a.cmp(&b)
                })
        });
        let BoundedSection {
            items: call_candidates,
            truncation: candidates_truncation,
            retained_bytes: call_candidates_bytes,
        } = bounded_section(
            call_candidates,
            limit,
            DIFF_CALL_CANDIDATES_BYTES,
            TruncationSection::DiffContextRelatedCallCandidates,
        )?;
        let mut truncation = Vec::new();
        truncation.extend(diff_inventory_truncation.map(|inventory| {
            TruncationDetail::new(
                TruncationSection::DiffContextChangedFiles,
                TruncationCause::DiffInventoryLimit,
                inventory.limit,
                inventory.omitted,
            )
        }));
        truncation.extend(files_truncation);
        truncation.extend(symbols_truncation);
        truncation.extend(callers_truncation);
        truncation.extend(tests_truncation);
        truncation.extend(relations_truncation);
        truncation.extend(candidates_truncation);
        truncation.extend(files_work.truncation(TruncationSection::DiffContextChangedFiles));
        truncation.extend(symbols_work.truncation(TruncationSection::DiffContextChangedSymbols));
        truncation.extend(callers_work.truncation(TruncationSection::DiffContextRelatedCallers));
        truncation.extend(tests_work.truncation(TruncationSection::DiffContextRelatedTests));
        truncation
            .extend(relations_work.truncation(TruncationSection::DiffContextRelatedRelations));
        truncation.extend(
            candidates_work.truncation(TruncationSection::DiffContextRelatedCallCandidates),
        );
        for (section, was_truncated, response_already_truncated) in [
            (
                TruncationSection::DiffContextRelatedCallers,
                callers_item_truncated,
                callers_response_item_truncated,
            ),
            (
                TruncationSection::DiffContextRelatedTests,
                tests_item_truncated,
                tests_response_item_truncated,
            ),
            (
                TruncationSection::DiffContextRelatedRelations,
                relations_item_truncated,
                relations_response_item_truncated,
            ),
        ] {
            truncation.extend(
                (was_truncated && !response_already_truncated).then(|| {
                    TruncationDetail::new(section, TruncationCause::ItemLimit, limit, None)
                }),
            );
        }
        truncation.extend(call_candidates_item_truncated.then(|| {
            TruncationDetail::new(
                TruncationSection::DiffContextRelatedCallCandidates,
                TruncationCause::ItemLimit,
                limit,
                None,
            )
        }));
        truncation.extend(candidate_truncation(
            TruncationSection::DiffContextRelatedCallCandidates,
            limit,
            call_candidates_fanout_truncated,
        ));
        let data = DiffContextData {
            scope: diff.scope,
            changed_files,
            changed_symbols,
            related_callers,
            related_tests,
            related_relations,
            related_call_candidates: call_candidates,
        };
        Ok(envelope(
            QueryConstruction {
                query: "diff_context",
                started,
                bounded_section_bytes: changed_files_bytes
                    .saturating_add(changed_symbols_bytes)
                    .saturating_add(related_callers_bytes)
                    .saturating_add(related_tests_bytes)
                    .saturating_add(related_relations_bytes)
                    .saturating_add(call_candidates_bytes),
                work: &work_stats,
            },
            &snapshot,
            provider_state_for(self, &snapshot),
            truncation,
            data,
        ))
    }
}

#[cfg(test)]
mod work_budget_tests {
    use super::*;

    #[test]
    fn work_causes_are_distinct_and_wall_time_is_deterministic_without_sleeping()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut examined = SectionWorkBudget::with_limits(1, 10, 10, Duration::from_secs(1));
        assert!(examined.record_examined(ExaminedKind::Symbol));
        assert!(!examined.record_examined(ExaminedKind::Symbol));
        assert_eq!(
            examined
                .truncation(TruncationSection::ContextCallers)
                .map(|detail| detail.cause),
            Some(TruncationCause::ExaminedWorkLimit)
        );

        let mut traversal = SectionWorkBudget::with_limits(10, 1, 10, Duration::from_secs(1));
        assert!(traversal.visit_edge());
        assert!(!traversal.visit_call_site());
        assert_eq!(
            traversal
                .truncation(TruncationSection::ContextCallers)
                .map(|detail| detail.cause),
            Some(TruncationCause::GraphTraversalLimit)
        );

        let mut allocation = SectionWorkBudget::with_limits(10, 10, 1, Duration::from_secs(1));
        assert!(allocation.retain_intermediate());
        assert!(!allocation.retain_intermediate());
        assert_eq!(
            allocation
                .truncation(TruncationSection::ContextCallers)
                .map(|detail| detail.cause),
            Some(TruncationCause::IntermediateAllocationLimit)
        );

        let mut wall = SectionWorkBudget::with_limits(10, 10, 10, Duration::ZERO);
        assert!(!wall.record_examined(ExaminedKind::File));
        let detail = wall
            .truncation(TruncationSection::RepoMapFiles)
            .ok_or("wall-time truncation missing")?;
        assert_eq!(detail.cause, TruncationCause::WallTimeLimit);
        assert_eq!(detail.limit, 0);

        let bounded = bounded_section_with_wall_time(
            vec!["payload"],
            10,
            1_024,
            TruncationSection::ContextCallers,
            Duration::ZERO,
        )?;
        assert!(bounded.items.is_empty());
        assert!(
            bounded.truncation.iter().any(|detail| {
                detail.cause == TruncationCause::WallTimeLimit && detail.limit == 0
            })
        );

        let mut graph = SymbolGraph::new();
        let path = chakra_domain::location::RepoRelativePath::new("src/target.rs")?;
        let target = graph.add_symbol(
            chakra_domain::symbol::SymbolKey {
                language: chakra_domain::symbol::Language::Rust,
                qualified_name: "target".to_owned(),
                container: None,
                kind: chakra_domain::symbol::SymbolKind::Function,
                path: path.clone(),
            },
            SourceRange::new(path, TextPosition::new(1, 1)?, TextPosition::new(1, 7)?)?,
            None,
            Provenance::TreeSitter,
            Precision::Syntax,
        )?;
        let mut exact_boundary = SectionWorkBudget::with_limits(10, 0, 10, Duration::from_secs(1));
        let candidates = incoming_call_candidates(
            &graph,
            target,
            &OperationContext::unbounded(),
            &mut exact_boundary,
        )?;
        assert!(candidates.items.is_empty());
        assert!(
            exact_boundary
                .truncation(TruncationSection::CallersSyntaxCandidates)
                .is_none(),
            "an exhausted count is not truncation when no work was omitted"
        );
        Ok(())
    }
}
