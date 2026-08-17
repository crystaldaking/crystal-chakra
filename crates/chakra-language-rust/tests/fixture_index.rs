//! End-to-end syntax index coverage over the project Rust fixture.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    CallersRequest, ContextRequest, DiffContextRequest, QueryError, QueryService, RepoMapRequest,
    SearchRequest, StatusRequest, SymbolRef, SymbolSearchRequest,
};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_domain::symbol::{EdgeKind, SymbolKind};
use chakra_engine::WorkspaceEngine;
use chakra_language_rust::{IndexMetrics, index_repository};
use tempfile::TempDir;

fn source_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("rust")
        .join("controller-service-provider")
}

fn copy_rust_tree(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            if entry.file_name() == "target" {
                continue;
            }
            fs::create_dir_all(&destination)?;
            copy_rust_tree(&entry.path(), &destination)?;
        } else if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "rs")
        {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

fn fixture_repository() -> Result<TempDir, Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    copy_rust_tree(&source_fixture_root(), repository.path())?;
    Ok(repository)
}

fn indexed_engine() -> Result<(TempDir, WorkspaceEngine, IndexMetrics), Box<dyn Error>> {
    let repository = fixture_repository()?;
    let report = index_repository(repository.path())?;
    let metrics = report.metrics;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    engine.install_diff_provider(std::sync::Arc::new(chakra_git::GitWorkspaceDiff))?;
    Ok((repository, engine, metrics))
}

#[test]
fn fixture_extracts_required_rust_syntax_facts() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, metrics) = indexed_engine()?;
    let snapshot = engine.snapshot();
    let graph = snapshot.graph();

    assert_eq!(metrics.discovered_files, 7);
    assert_eq!(metrics.parsed_files, 7);
    assert_eq!(metrics.syntax_error_files, 0);
    assert_eq!(metrics.truncated_call_sites, 0);
    assert_eq!(graph.file_count(), 7);
    assert!(metrics.elapsed.as_nanos() > 0);
    assert!(graph.symbol_count() >= 25);
    assert!(graph.edge_count() > 0);
    graph.validate_consistency()?;

    let required = [
        ("api", SymbolKind::Module),
        ("api::controller", SymbolKind::Module),
        ("api::controller::PaymentController", SymbolKind::Struct),
        (
            "api::controller::PaymentController::refund",
            SymbolKind::Method,
        ),
        ("provider::PaymentProvider", SymbolKind::Trait),
        ("provider::PaymentProvider::refund", SymbolKind::Method),
        ("provider::StripeProvider::api_key", SymbolKind::Field),
        (
            "service::payment_service::tests::refund_delegates_to_provider",
            SymbolKind::Test,
        ),
        (
            "service::payment_service::tests::service",
            SymbolKind::Function,
        ),
        (
            "tests::refund_flow::refund_flows_through_all_layers",
            SymbolKind::Test,
        ),
    ];
    for (qualified_name, kind) in required {
        assert!(
            graph.symbols().iter().any(|symbol| {
                symbol.key.qualified_name == qualified_name && symbol.key.kind == kind
            }),
            "missing {kind:?} {qualified_name}"
        );
    }
    assert!(
        graph
            .symbols()
            .iter()
            .any(|symbol| symbol.key.kind == SymbolKind::Import)
    );
    assert!(graph.symbols().iter().all(|symbol| {
        symbol.provenance == Provenance::TreeSitter && symbol.precision == Precision::Syntax
    }));

    let call_edges: Vec<_> = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.outgoing_edges(symbol.id))
        .filter(|edge| edge.kind == EdgeKind::Calls)
        .collect();
    assert!(!call_edges.is_empty());
    assert!(call_edges.iter().all(|edge| {
        edge.provenance == Provenance::TreeSitter
            && edge.precision == Precision::Heuristic
            && edge.location.is_some()
    }));

    let only = |name: &str| -> Result<chakra_domain::symbol::EntityId, Box<dyn Error>> {
        let matches = graph.resolve_name(name);
        if matches.len() != 1 {
            return Err(format!("expected one symbol for {name}, got {}", matches.len()).into());
        }
        Ok(matches[0])
    };
    let stripe = only("provider::StripeProvider")?;
    let provider_trait = only("provider::PaymentProvider")?;
    assert!(graph.outgoing_edges(stripe).iter().any(|edge| {
        edge.kind == EdgeKind::Implements
            && edge.to == provider_trait
            && edge.provenance == Provenance::TreeSitter
            && edge.precision == Precision::Heuristic
    }));
    let stripe_refund = only("provider::StripeProvider::refund")?;
    let trait_refund = only("provider::PaymentProvider::refund")?;
    assert!(graph.outgoing_edges(stripe_refund).iter().any(|edge| {
        edge.kind == EdgeKind::Implements
            && edge.to == trait_refund
            && edge.provenance == Provenance::TreeSitter
            && edge.precision == Precision::Heuristic
    }));
    Ok(())
}

#[test]
fn qualified_impl_paths_are_not_linked_to_same_named_local_declarations()
-> Result<(), Box<dyn Error>> {
    let repository = fixture_repository()?;
    fs::write(
        repository.path().join("src/impl_resolution.rs"),
        r#"
            pub trait Display {}
            pub trait Marker {}
            pub struct S;
            pub struct Vec;

            impl std::fmt::Display for S {}
            impl Marker for std::vec::Vec<u8> {}

            pub mod nested {
                pub trait Local {}
                pub struct Nested;
                impl Local for Nested {}
            }
        "#,
    )?;
    let report = index_repository(repository.path())?;
    let graph = &report.graph;
    let only = |name: &str| -> Result<chakra_domain::symbol::EntityId, Box<dyn Error>> {
        let matches = graph.resolve_name(name);
        if matches.len() != 1 {
            return Err(format!("expected one symbol for {name}, got {}", matches.len()).into());
        }
        Ok(matches[0])
    };

    let display = only("impl_resolution::Display")?;
    let marker = only("impl_resolution::Marker")?;
    let local_s = only("impl_resolution::S")?;
    let local_vec = only("impl_resolution::Vec")?;
    let nested_trait = only("impl_resolution::nested::Local")?;
    let nested_type = only("impl_resolution::nested::Nested")?;

    assert!(
        !graph
            .outgoing_edges(local_s)
            .iter()
            .any(|edge| { edge.kind == EdgeKind::Implements && edge.to == display })
    );
    assert!(
        !graph
            .outgoing_edges(local_vec)
            .iter()
            .any(|edge| { edge.kind == EdgeKind::Implements && edge.to == marker })
    );
    assert!(!graph.outgoing_edges(local_vec).iter().any(|edge| {
        edge.kind == EdgeKind::Contains
            && graph
                .symbol(edge.to)
                .is_some_and(|symbol| symbol.key.kind == SymbolKind::ImplBlock)
    }));

    let s_impl = graph
        .outgoing_edges(local_s)
        .iter()
        .find(|edge| {
            edge.kind == EdgeKind::Contains
                && graph
                    .symbol(edge.to)
                    .is_some_and(|symbol| symbol.key.kind == SymbolKind::ImplBlock)
        })
        .ok_or("local S impl containment missing")?;
    assert_eq!(s_impl.provenance, Provenance::TreeSitter);
    assert_eq!(s_impl.precision, Precision::Heuristic);
    assert!(graph.outgoing_edges(nested_type).iter().any(|edge| {
        edge.kind == EdgeKind::Implements
            && edge.to == nested_trait
            && edge.provenance == Provenance::TreeSitter
            && edge.precision == Precision::Heuristic
    }));
    Ok(())
}

#[test]
fn real_index_serves_bounded_repo_text_symbol_and_context_queries() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, _metrics) = indexed_engine()?;

    let status = engine.status(StatusRequest)?;
    assert_eq!(status.freshness, Freshness::Fresh);
    assert_eq!(status.status, WorkspaceStatus::Ready);
    assert_eq!(status.data.counts.files, 7);

    let repo_map = engine.repo_map(RepoMapRequest::default())?;
    assert_eq!(repo_map.data.files.len(), 7);
    assert!(
        repo_map
            .data
            .files
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path)
    );
    assert!(repo_map.data.files.iter().all(|file| {
        file.provenance == Provenance::Git && file.precision == Precision::Precise
    }));

    let text = engine.search(SearchRequest {
        query: "amount must be positive".to_owned(),
        case_sensitive: true,
        ..SearchRequest::default()
    })?;
    assert_eq!(text.data.matches.len(), 1);
    let matched = &text.data.matches[0];
    assert_eq!(matched.file.as_str(), "src/service/payment_service.rs");
    assert_eq!(matched.range.start().line(), 18);
    assert_eq!(matched.provenance, Provenance::TextSearch);
    assert_eq!(matched.precision, Precision::Textual);
    assert!(!matched.line_truncated);

    let regex = engine.search(SearchRequest {
        query: "refund_(delegates|rejects)".to_owned(),
        regex: true,
        case_sensitive: true,
        ..SearchRequest::default()
    })?;
    assert_eq!(regex.data.matches.len(), 2);

    let limited = engine.search(SearchRequest {
        query: "refund".to_owned(),
        limit: Some(1),
        ..SearchRequest::default()
    })?;
    assert_eq!(limited.data.matches.len(), 1);
    assert!(limited.truncated);
    assert!(matches!(
        engine.search(SearchRequest::default()),
        Err(QueryError::Invalid(_))
    ));
    assert!(matches!(
        engine.search(SearchRequest {
            query: "[".to_owned(),
            regex: true,
            ..SearchRequest::default()
        }),
        Err(QueryError::Invalid(_))
    ));

    let symbols = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    assert!(symbols.data.candidates.len() >= 7);
    assert!(symbols.data.candidates.iter().all(|symbol| {
        symbol.provenance == Provenance::TreeSitter && symbol.precision == Precision::Syntax
    }));

    let ambiguous = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName("refund".to_owned())),
        ..CallersRequest::default()
    });
    assert!(matches!(
        ambiguous,
        Err(QueryError::AmbiguousSymbol { candidates: 4, .. })
    ));

    let context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName(
            "service::payment_service::PaymentService::refund".to_owned(),
        )),
        ..ContextRequest::default()
    })?;
    let snippet = context
        .data
        .source
        .as_ref()
        .ok_or("source snippet missing")?;
    assert!(snippet.text.contains("amount_cents == 0"));
    assert!(!snippet.truncated);
    assert_eq!(snippet.provenance, Provenance::TreeSitter);
    assert_eq!(snippet.precision, Precision::Syntax);
    assert!(snippet.text.chars().count() <= 4_096);
    assert!(context.data.callers.is_empty());
    assert!(context.data.callees.is_empty());
    assert!(context.data.tests.is_empty());
    assert!(!context.data.syntax_call_candidates.is_empty());
    assert!(context.data.syntax_call_candidates.iter().any(|call_site| {
        call_site.name == "refund"
            && call_site.candidate_target.is_none()
            && call_site.resolution == chakra_domain::symbol::CallResolution::Unresolved
            && call_site.precision == Precision::Syntax
    }));
    Ok(())
}

#[test]
fn search_line_snippets_remain_bounded_and_keep_original_range() -> Result<(), Box<dyn Error>> {
    let repository = fixture_repository()?;
    let long_prefix = "x".repeat(800);
    fs::write(
        repository.path().join("src/long.rs"),
        format!("// {long_prefix} NEEDLE tail\n"),
    )?;
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let result = engine.search(SearchRequest {
        query: "NEEDLE".to_owned(),
        case_sensitive: true,
        ..SearchRequest::default()
    })?;
    let matched = result.data.matches.first().ok_or("match missing")?;
    assert!(matched.range.start().column() > 800);
    assert!(matched.line.chars().count() <= 512);
    assert!(matched.line.contains("NEEDLE"));
    assert!(matched.line_truncated);
    assert!(result.truncated);
    Ok(())
}

#[test]
fn context_source_budget_sets_both_local_and_envelope_truncation() -> Result<(), Box<dyn Error>> {
    let repository = fixture_repository()?;
    let body = (0..40)
        .map(|line| format!("    let value_{line} = {line};\n"))
        .collect::<String>();
    fs::write(
        repository.path().join("src/long_context.rs"),
        format!("pub fn long_context() {{\n{body}}}\n"),
    )?;
    let report = index_repository(repository.path())?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let result = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("long_context".to_owned())),
        ..ContextRequest::default()
    })?;
    let snippet = result.data.source.ok_or("source snippet missing")?;
    assert!(snippet.truncated);
    assert!(result.truncated);
    assert!(snippet.text.lines().count() <= 20);
    assert!(snippet.text.chars().count() <= 4_096);
    Ok(())
}

#[test]
fn ambiguous_call_candidates_are_lazy_and_bounded() -> Result<(), Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    fs::create_dir_all(repository.path().join("src"))?;
    fs::write(
        repository.path().join("src/lib.rs"),
        "pub fn invoke() { target(); }\n",
    )?;
    for index in 0..65 {
        fs::write(
            repository.path().join(format!("src/module_{index:02}.rs")),
            "pub fn target() {}\n",
        )?;
    }

    let report = index_repository(repository.path())?;
    assert_eq!(report.metrics.truncated_call_sites, 0);
    assert_eq!(report.graph.truncated_call_sites(), 0);
    assert_eq!(report.graph.call_site_count(), 1);
    assert_eq!(report.graph.ambiguous_call_site_count(), 1);
    assert_eq!(
        report
            .graph
            .symbols()
            .iter()
            .flat_map(|symbol| report.graph.outgoing_edges(symbol.id))
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .count(),
        0
    );
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    let revision = engine.publish(update)?.revision();

    let candidates = engine.symbol_search(SymbolSearchRequest {
        query: "target".to_owned(),
        limit: Some(500),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(candidates.data.candidates.len(), 65);
    let target = candidates
        .data
        .candidates
        .last()
        .ok_or("target candidate missing")?;
    let callers = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ById {
            id: target.id,
            revision,
        }),
        ..CallersRequest::default()
    })?;
    assert!(callers.data.callers.is_empty());
    assert_eq!(callers.data.syntax_candidates.len(), 1);
    assert_eq!(
        callers.data.syntax_candidates[0].resolution,
        chakra_domain::symbol::CallResolution::Ambiguous { candidates: 65 }
    );
    assert!(!callers.truncated);

    let context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("invoke".to_owned())),
        ..ContextRequest::default()
    })?;
    assert!(context.data.callees.is_empty());
    assert_eq!(context.data.syntax_call_candidates.len(), 20);
    assert!(context.truncated);
    Ok(())
}

#[test]
fn indexing_and_query_latencies_are_directly_measurable() -> Result<(), Box<dyn Error>> {
    let (_repository, engine, metrics) = indexed_engine()?;
    let symbol_started = Instant::now();
    let result = engine.symbol_search(SymbolSearchRequest {
        query: "refund".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    let symbol_search_elapsed = symbol_started.elapsed();
    let context_started = Instant::now();
    let context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName(
            "service::payment_service::PaymentService::refund".to_owned(),
        )),
        ..ContextRequest::default()
    })?;
    let context_elapsed = context_started.elapsed();
    let diff_started = Instant::now();
    let diff = engine.diff_context(DiffContextRequest::default())?;
    let diff_context_elapsed = diff_started.elapsed();

    assert!(metrics.elapsed.as_nanos() > 0);
    assert!(symbol_search_elapsed.as_nanos() > 0);
    assert!(context_elapsed.as_nanos() > 0);
    assert!(diff_context_elapsed.as_nanos() > 0);
    assert!(!result.data.candidates.is_empty());
    assert!(!context.data.callees.is_empty() || !context.data.syntax_call_candidates.is_empty());
    assert!(!diff.data.changed_files.is_empty());
    eprintln!(
        "syntax_index_fixture: initial={:?}, symbol_search={:?}, context={:?}, diff_context={:?}, files={}, symbols={}, edges={}, call_sites={}, ambiguous_call_sites={}, unresolved_call_sites={}",
        metrics.elapsed,
        symbol_search_elapsed,
        context_elapsed,
        diff_context_elapsed,
        metrics.parsed_files,
        metrics.symbols,
        metrics.edges,
        metrics.call_sites,
        metrics.ambiguous_call_sites,
        metrics.unresolved_call_sites,
    );
    Ok(())
}
