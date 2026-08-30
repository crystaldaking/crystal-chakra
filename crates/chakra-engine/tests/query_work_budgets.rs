//! High-degree regressions for traversal-time query work limits.

use std::error::Error;
use std::sync::Arc;

use chakra_domain::envelope::{TruncationCause, TruncationSection};
use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::operation::OperationContext;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    CallersRequest, ChangeKind, DiffContextRequest, QueryService, RepoMapRequest,
    ResolvedDiffScope, SymbolRef,
};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_domain::symbol::{
    CallForm, CallResolution, CallTargetKind, Language, SymbolKey, SymbolKind,
};
use chakra_engine::{
    CallSiteInput, DiffWorkspace, SymbolGraph, WorkspaceDiff, WorkspaceDiffError,
    WorkspaceDiffProvider, WorkspaceEngine, WorkspaceFileChange,
};

const HIGH_DEGREE: usize = 3_000;
const ALLOCATION_DEGREE: usize = 5_000;
const DEFAULT_TRAVERSAL_LIMIT: u64 = 2_048;
const DEFAULT_EXAMINED_LIMIT: u64 = 1_024;
const REQUEST_INTERMEDIATE_LIMIT: u64 = 4_000;

fn range(path: &RepoRelativePath, line: u32) -> Result<SourceRange, Box<dyn Error>> {
    Ok(SourceRange::new(
        path.clone(),
        TextPosition::new(line, 1)?,
        TextPosition::new(line, 8)?,
    )?)
}

fn add_function(
    graph: &mut SymbolGraph,
    path: &RepoRelativePath,
    qualified_name: String,
    line: u32,
) -> Result<chakra_domain::symbol::EntityId, Box<dyn Error>> {
    Ok(graph.add_symbol(
        SymbolKey {
            language: Language::Rust,
            qualified_name,
            container: None,
            kind: SymbolKind::Function,
            path: path.clone(),
        },
        range(path, line)?,
        None,
        Provenance::TreeSitter,
        Precision::Syntax,
    )?)
}

fn publish(graph: SymbolGraph) -> Result<WorkspaceEngine, Box<dyn Error>> {
    let identity = WorkspaceIdentity::for_primary_worktree(std::path::Path::new("."))?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    Ok(engine)
}

#[test]
fn callers_stop_at_the_edge_budget_and_keep_deterministic_top_results() -> Result<(), Box<dyn Error>>
{
    let path = RepoRelativePath::new("src/high_degree.rs")?;
    let mut graph = SymbolGraph::new();
    let target = add_function(&mut graph, &path, "target::hot".to_owned(), 1)?;
    for insertion in 0..HIGH_DEGREE {
        let rank = HIGH_DEGREE - insertion - 1;
        let line = u32::try_from(insertion + 2)?;
        let caller = add_function(&mut graph, &path, format!("caller::{rank:04}"), line)?;
        graph.add_edge(
            chakra_domain::symbol::EdgeKind::Calls,
            caller,
            target,
            Provenance::TreeSitter,
            Precision::Syntax,
            Some(range(&path, line)?),
        )?;
    }
    let engine = publish(graph)?;
    let revision = engine.snapshot().revision();
    let request = CallersRequest {
        source: Default::default(),
        symbol: Some(SymbolRef::ById {
            id: target,
            revision,
        }),
        limit: Some(20),
        ..CallersRequest::default()
    };

    let first = engine.callers(request.clone())?;
    let second = engine.callers(request)?;
    assert_eq!(first.data, second.data);
    assert_eq!(first.truncation, second.truncation);
    assert_eq!(first.data.callers.len(), 20);
    assert!(
        first
            .data
            .callers
            .windows(2)
            .all(|pair| { pair[0].symbol.qualified_name < pair[1].symbol.qualified_name })
    );
    assert!(first.truncation.iter().any(|detail| {
        detail.section == TruncationSection::CallersCallers
            && detail.cause == TruncationCause::GraphTraversalLimit
            && detail.limit == DEFAULT_TRAVERSAL_LIMIT
    }));
    Ok(())
}

#[test]
fn repeated_ambiguous_sites_aggregate_without_scanning_the_whole_degree()
-> Result<(), Box<dyn Error>> {
    let path = RepoRelativePath::new("src/calls.rs")?;
    let mut graph = SymbolGraph::new();
    let caller = add_function(&mut graph, &path, "caller::invoke".to_owned(), 1)?;
    let target_a = add_function(&mut graph, &path, "targets::a::hot".to_owned(), 2)?;
    add_function(&mut graph, &path, "targets::b::hot".to_owned(), 3)?;
    for index in 0..HIGH_DEGREE {
        let resolution = graph.add_call_site(CallSiteInput {
            caller,
            form: CallForm::Function,
            target_kind: CallTargetKind::Function,
            name: "hot".to_owned(),
            qualifier: None,
            receiver_type: None,
            receiver_type_source: None,
            receiver_hint: None,
            location: range(&path, u32::try_from(index + 4)?)?,
            provenance: Provenance::TreeSitter,
            precision: Precision::Syntax,
        })?;
        assert_eq!(resolution, CallResolution::Ambiguous { candidates: 2 });
    }
    let engine = publish(graph)?;
    let revision = engine.snapshot().revision();
    let result = engine.callers(CallersRequest {
        source: Default::default(),
        symbol: Some(SymbolRef::ById {
            id: target_a,
            revision,
        }),
        limit: Some(20),
        ..CallersRequest::default()
    })?;

    assert_eq!(result.data.syntax_candidates.len(), 1);
    assert_eq!(
        result.data.syntax_candidates[0].occurrence_count,
        DEFAULT_TRAVERSAL_LIMIT
    );
    assert_eq!(
        result.data.syntax_candidates[0]
            .representative_evidence
            .len(),
        3
    );
    assert!(result.truncation.iter().any(|detail| {
        detail.section == TruncationSection::CallersSyntaxCandidates
            && detail.cause == TruncationCause::GraphTraversalLimit
            && detail.limit == DEFAULT_TRAVERSAL_LIMIT
    }));
    Ok(())
}

#[test]
fn incoming_candidates_stop_before_exceeding_the_intermediate_budget() -> Result<(), Box<dyn Error>>
{
    let path = RepoRelativePath::new("src/allocation.rs")?;
    let mut graph = SymbolGraph::new();
    let caller = add_function(&mut graph, &path, "caller::invoke".to_owned(), 1)?;
    let target_a = add_function(&mut graph, &path, "targets::a::hot".to_owned(), 2)?;
    add_function(&mut graph, &path, "targets::b::hot".to_owned(), 3)?;
    for index in 0..ALLOCATION_DEGREE {
        graph.add_call_site(CallSiteInput {
            caller,
            form: CallForm::Function,
            target_kind: CallTargetKind::Function,
            name: "hot".to_owned(),
            qualifier: None,
            receiver_type: None,
            receiver_type_source: None,
            receiver_hint: None,
            location: range(&path, u32::try_from(index + 4)?)?,
            provenance: Provenance::TreeSitter,
            precision: Precision::Syntax,
        })?;
    }
    let engine = publish(graph)?;
    let revision = engine.snapshot().revision();
    let result = engine.callers(CallersRequest {
        source: Default::default(),
        symbol: Some(SymbolRef::ById {
            id: target_a,
            revision,
        }),
        limit: Some(500),
        ..CallersRequest::default()
    })?;

    assert_eq!(result.data.syntax_candidates.len(), 1);
    assert_eq!(
        result.data.syntax_candidates[0].occurrence_count,
        REQUEST_INTERMEDIATE_LIMIT
    );
    assert!(result.truncation.iter().any(|detail| {
        detail.section == TruncationSection::CallersSyntaxCandidates
            && detail.cause == TruncationCause::IntermediateAllocationLimit
            && detail.limit == REQUEST_INTERMEDIATE_LIMIT
    }));
    Ok(())
}

#[test]
fn repo_map_streams_a_sorted_prefix_without_materializing_every_summary()
-> Result<(), Box<dyn Error>> {
    let mut graph = SymbolGraph::new();
    for insertion in 0..HIGH_DEGREE {
        let rank = HIGH_DEGREE - insertion - 1;
        graph.add_file(RepoRelativePath::new(format!("src/file-{rank:04}.rs"))?, "")?;
    }
    let engine = publish(graph)?;
    let result = engine.repo_map(RepoMapRequest {
        include_project_scope: false,
        limit: Some(20),
        ..RepoMapRequest::default()
    })?;

    assert_eq!(result.data.files.len(), 20);
    assert_eq!(result.data.files[0].path.as_str(), "src/file-0000.rs");
    assert_eq!(result.data.files[19].path.as_str(), "src/file-0019.rs");
    assert!(result.truncation.iter().any(|detail| {
        detail.section == TruncationSection::RepoMapFiles
            && detail.cause == TruncationCause::ItemLimit
            && detail.omitted == Some(u64::try_from(HIGH_DEGREE - 20).unwrap_or(u64::MAX))
    }));
    Ok(())
}

#[test]
fn repo_map_pages_composite_language_partitions_in_global_path_order() -> Result<(), Box<dyn Error>>
{
    let mut rust = SymbolGraph::new();
    let rust_path = RepoRelativePath::new("z-last.rs")?;
    add_function(&mut rust, &rust_path, "rust::last".to_owned(), 1)?;

    let mut php = SymbolGraph::new();
    let php_path = RepoRelativePath::new("a-first.php")?;
    php.add_symbol(
        SymbolKey {
            language: Language::Php,
            qualified_name: "App::first".to_owned(),
            container: None,
            kind: SymbolKind::Function,
            path: php_path.clone(),
        },
        range(&php_path, 1)?,
        None,
        Provenance::TreeSitter,
        Precision::Syntax,
    )?;

    let engine = publish(SymbolGraph::merge([rust, php])?)?;
    let first = engine.repo_map(RepoMapRequest {
        include_project_scope: false,
        limit: Some(1),
        ..RepoMapRequest::default()
    })?;
    assert_eq!(first.data.files[0].path.as_str(), "a-first.php");
    let second = engine.repo_map(RepoMapRequest {
        include_project_scope: false,
        cursor: first.data.next_cursor,
        limit: Some(1),
        ..RepoMapRequest::default()
    })?;
    assert_eq!(second.data.files[0].path.as_str(), "z-last.rs");
    assert!(second.data.next_cursor.is_none());
    Ok(())
}

#[derive(Debug)]
struct OneFileDiff {
    path: RepoRelativePath,
}

impl WorkspaceDiffProvider for OneFileDiff {
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
            files: vec![WorkspaceFileChange {
                path: self.path.clone(),
                previous_path: None,
                change: ChangeKind::Modified,
                provenance: Provenance::Git,
                precision: Precision::Precise,
            }],
            truncation: None,
        })
    }
}

#[test]
fn diff_context_stops_symbol_collection_at_the_examined_budget() -> Result<(), Box<dyn Error>> {
    let path = RepoRelativePath::new("src/large_diff.rs")?;
    let mut graph = SymbolGraph::new();
    graph.add_file(path.clone(), "")?;
    for insertion in 0..HIGH_DEGREE {
        let rank = HIGH_DEGREE - insertion - 1;
        add_function(
            &mut graph,
            &path,
            format!("large_diff::item_{rank:04}"),
            u32::try_from(insertion + 1)?,
        )?;
    }
    let engine = publish(graph)?;
    engine.install_diff_provider(Arc::new(OneFileDiff { path }))?;

    let first = engine.diff_context(DiffContextRequest {
        source: Default::default(),
        limit: Some(20),
        ..DiffContextRequest::default()
    })?;
    let second = engine.diff_context(DiffContextRequest {
        source: Default::default(),
        limit: Some(20),
        ..DiffContextRequest::default()
    })?;
    assert_eq!(first.data, second.data);
    assert_eq!(first.truncation, second.truncation);
    assert_eq!(first.data.changed_symbols.len(), 20);
    assert!(first.truncation.iter().any(|detail| {
        detail.section == TruncationSection::DiffContextChangedSymbols
            && detail.cause == TruncationCause::ExaminedWorkLimit
            && detail.limit == DEFAULT_EXAMINED_LIMIT
    }));
    Ok(())
}
