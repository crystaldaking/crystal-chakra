use std::error::Error;
use std::fs::{self, File};
use std::process::Command;
use std::sync::Barrier;

use tempfile::TempDir;

use super::*;

fn graph_snapshot(graph: &SymbolGraph) -> Vec<String> {
    let mut snapshot = vec![format!("files:{:?}", graph.file_summaries())];
    for symbol in graph.symbols() {
        snapshot.push(format!("symbol:{symbol:?}"));
        snapshot.push(format!("outgoing:{:?}", graph.outgoing_edges(symbol.id)));
        snapshot.push(format!(
            "calls:{:?}",
            graph.call_sites_from(symbol.id).collect::<Vec<_>>()
        ));
    }
    snapshot
}

#[test]
fn bounded_parallel_parsing_is_deterministic() -> Result<(), Box<dyn Error>> {
    let mut sources = BTreeMap::new();
    for index in 0..64 {
        sources.insert(
            RepoRelativePath::new(format!("src/generated_{index:03}.cs"))?,
            Arc::<str>::from(format!(
                "class Generated{index:03} {{ void caller_{index}() {{ helper_{index}(); }} void helper_{index}() {{}} }}\n"
            )),
        );
    }
    let cancellation = IndexCancellation::default();
    let (_, sequential, sequential_metrics) = CSharpSyntaxIndex::from_sources_scheduled(
        sources.clone(),
        GraphBuildLimits::UNLIMITED,
        1,
        1,
        &cancellation,
    )?;
    let (_, parallel, parallel_metrics) = CSharpSyntaxIndex::from_sources_scheduled(
        sources,
        GraphBuildLimits::UNLIMITED,
        4,
        1,
        &cancellation,
    )?;

    assert_eq!(graph_snapshot(&sequential), graph_snapshot(&parallel));
    assert_eq!(sequential_metrics.facts, parallel_metrics.facts);
    assert_eq!(sequential_metrics.graph, parallel_metrics.graph);
    let parallel_parse = parallel_metrics
        .phases
        .iter()
        .find(|phase| phase.phase == IndexPhase::ParseExtraction)
        .ok_or("parallel parse phase missing")?;
    assert_eq!(parallel_parse.effective_workers, 4);
    assert!((1..=parallel_parse.effective_workers).contains(&parallel_parse.peak_active_workers));
    assert_eq!(parallel_parse.peak_queue_depth, 0);
    assert_eq!(parallel_parse.work_items, 64);
    Ok(())
}

#[test]
fn cancelling_parallel_parse_joins_every_scoped_worker() -> Result<(), Box<dyn Error>> {
    const WORKERS: usize = 4;
    let mut sources = BTreeMap::new();
    for index in 0..64 {
        sources.insert(
            RepoRelativePath::new(format!("src/cancel_{index:03}.cs"))?,
            Arc::<str>::from(format!(
                "class Cancel{index:03} {{ void item_{index}() {{}} }}\n"
            )),
        );
    }
    let cancellation = IndexCancellation::default();
    let worker_cancellation = cancellation.clone();
    let started = Arc::new(Barrier::new(WORKERS + 1));
    let release = Arc::new(Barrier::new(WORKERS + 1));
    let worker_started = started.clone();
    let worker_release = release.clone();
    let indexing = std::thread::spawn(move || {
        let observer = || {
            worker_started.wait();
            worker_release.wait();
        };
        parse_sources_scheduled_inner(sources, WORKERS, 1, &worker_cancellation, Some(&observer))
    });

    started.wait();
    cancellation.cancel();
    release.wait();
    let result = indexing.join().map_err(|_| "index owner panicked")?;
    assert!(matches!(result, Err(CSharpIndexError::Cancelled)));
    Ok(())
}

#[test]
fn duplicate_call_fanout_is_stored_linearly() -> Result<(), Box<dyn Error>> {
    const TARGETS: usize = 256;
    const CALLS: usize = 256;

    let mut source = String::new();
    for index in 0..TARGETS {
        source.push_str(&format!(
            "class Outer{index} {{ void target_{index}() {{}} void target() {{}} }}\n"
        ));
    }
    for index in 0..CALLS {
        source.push_str(&format!(
            "class Caller{index} {{ void caller_{index}() {{ target(); }} }}\n"
        ));
    }
    let mut sources = BTreeMap::new();
    sources.insert(
        RepoRelativePath::new("src/lib.cs")?,
        Arc::<str>::from(source),
    );

    let started = Instant::now();
    let (_, graph) = CSharpSyntaxIndex::from_sources(sources)?;
    let elapsed = started.elapsed();
    let call_edges = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.outgoing_edges(symbol.id))
        .filter(|edge| edge.kind == EdgeKind::Calls)
        .count();

    assert_eq!(graph.call_site_count(), CALLS as u64);
    assert_eq!(graph.ambiguous_call_site_count(), CALLS as u64);
    assert_eq!(graph.unresolved_call_site_count(), 0);
    assert_eq!(call_edges, 0, "ambiguous calls must not fan out into edges");
    assert_eq!(graph.truncated_call_sites(), 0);
    eprintln!(
        "lazy_call_sites: targets={TARGETS}, calls={CALLS}, call_sites={}, call_edges={call_edges}, eager_edge_product={}, elapsed={elapsed:?}",
        graph.call_site_count(),
        TARGETS * CALLS,
    );
    Ok(())
}

#[test]
fn overloaded_extension_methods_remain_ambiguous() -> Result<(), Box<dyn Error>> {
    let source = Arc::<str>::from(
        "namespace Chakra;\n\
         public static class Extensions {\n\
         \x20   public static string Normalize(this string value) => value;\n\
         \x20   public static int Normalize(this int value) => value;\n\
         }\n\
         public class Caller { public void Run() { \"value\".Normalize(); } }\n",
    );
    let (index, graph) = CSharpSyntaxIndex::from_sources(BTreeMap::from([(
        RepoRelativePath::new("src/Extensions.cs")?,
        source,
    )]))?;
    let parsed = index
        .files
        .values()
        .next()
        .ok_or("parsed extension file missing")?;
    assert_eq!(
        parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.is_extension_method)
            .count(),
        2,
        "symbols: {:?}",
        parsed.symbols
    );
    assert_eq!(parsed.extension_scopes, ["Chakra"]);
    let call = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.call_sites_from(symbol.id))
        .find(|call| call.name == "Normalize")
        .ok_or("extension call missing")?;
    assert!(
        matches!(call.resolution, CallResolution::Ambiguous { candidates: 2 }),
        "resolution: {:?}, qualifier: {:?}",
        call.resolution,
        call.qualifier
    );
    graph.validate_consistency()?;
    Ok(())
}

#[test]
fn extension_lookup_work_ignores_unrelated_repository_files() -> Result<(), Box<dyn Error>> {
    const UNRELATED_FILES: usize = 1_024;
    let extension_path = RepoRelativePath::new("src/Extensions.cs")?;
    let caller_path = RepoRelativePath::new("src/Caller.cs")?;
    let mut sources = BTreeMap::from([
        (
            extension_path,
            Arc::<str>::from(
                "namespace Chakra; public static class Extensions { public static string Normalize(this string value) => value; }\n",
            ),
        ),
        (
            caller_path.clone(),
            Arc::<str>::from(
                "namespace Chakra; public class Caller { public void Run() { \"value\".Normalize(); } }\n",
            ),
        ),
    ]);
    for index in 0..UNRELATED_FILES {
        sources.insert(
            RepoRelativePath::new(format!("src/unrelated/File{index:04}.cs"))?,
            Arc::<str>::from(format!(
                "namespace Unrelated; public class File{index:04} {{ public void Run{index:04}() {{}} }}\n"
            )),
        );
    }

    let (index, graph) = CSharpSyntaxIndex::from_sources(sources)?;
    let caller = index
        .files
        .get(&caller_path)
        .ok_or("parsed extension caller missing")?;
    let call = caller
        .calls
        .iter()
        .find(|call| call.name == "Normalize")
        .ok_or("extension call draft missing")?;
    let (qualifier, candidates_examined) = index.extension_catalog.qualifier(caller, call);

    assert_eq!(qualifier.as_deref(), Some("Extensions"));
    assert_eq!(candidates_examined, 1);
    assert_eq!(index.extension_catalog.by_name.len(), 1);
    assert!(graph.symbols().iter().any(|symbol| {
        graph.call_sites_from(symbol.id).any(|call| {
            call.name == "Normalize" && matches!(call.resolution, CallResolution::Resolved { .. })
        })
    }));
    graph.validate_consistency()?;
    Ok(())
}

#[test]
fn extension_modifier_changes_recompute_external_callers() -> Result<(), Box<dyn Error>> {
    let extension_path = RepoRelativePath::new("src/Extensions.cs")?;
    let caller_path = RepoRelativePath::new("src/Caller.cs")?;
    let extension = "namespace Chakra; public static class Extensions { public static string Normalize(this string value) => value; }\n";
    let caller =
        "namespace Chakra; public class Caller { public void Run() { \"value\".Normalize(); } }\n";
    let mut sources = BTreeMap::from([
        (extension_path.clone(), Arc::<str>::from(extension)),
        (caller_path.clone(), Arc::<str>::from(caller)),
    ]);
    let (index, graph) = CSharpSyntaxIndex::from_sources(sources.clone())?;
    let resolution = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.call_sites_from(symbol.id))
        .find(|call| call.name == "Normalize")
        .map(|call| call.resolution.clone());
    assert!(matches!(resolution, Some(CallResolution::Resolved { .. })));

    sources.insert(
        extension_path,
        Arc::<str>::from(extension.replace("this string", "string")),
    );
    let reconciled = index.reconcile_sources(sources)?;
    assert_eq!(reconciled.metrics.reparsed_files, 1);
    let next = reconciled.graph.ok_or("reconciled graph missing")?;
    let resolution = next
        .symbols()
        .iter()
        .flat_map(|symbol| next.call_sites_from(symbol.id))
        .find(|call| call.name == "Normalize")
        .map(|call| call.resolution.clone());
    assert_eq!(resolution, Some(CallResolution::Unresolved));
    next.validate_consistency()?;
    Ok(())
}

#[test]
fn appended_declaration_preserves_unchanged_callable_identity_and_callers()
-> Result<(), Box<dyn Error>> {
    const CALLERS: usize = 128;
    let target_path = RepoRelativePath::new("src/Target.cs")?;
    let target = "namespace Chakra; public class Target { public static void Common() {} }\n";
    let mut sources = BTreeMap::from([(target_path.clone(), Arc::<str>::from(target))]);
    for index in 0..CALLERS {
        sources.insert(
            RepoRelativePath::new(format!("src/callers/Caller{index:03}.cs"))?,
            Arc::<str>::from(format!(
                "namespace Chakra; public class Caller{index:03} {{ public void Run() {{ Target.Common(); }} }}\n"
            )),
        );
    }

    let (index, graph) = CSharpSyntaxIndex::from_sources(sources.clone())?;
    let target_before = graph
        .resolve_name("Chakra::Target::Common")
        .first()
        .copied()
        .ok_or("common target missing before append")?;
    assert_eq!(graph.call_site_count(), CALLERS as u64);

    sources.insert(
        target_path,
        Arc::<str>::from(format!("{target}public class ChakraCorpusProbeOne {{}}\n")),
    );
    let reconciled = index.reconcile_sources(sources)?;
    assert_eq!(reconciled.metrics.reparsed_files, 1);
    assert_eq!(reconciled.metrics.publication.rebuilt_files, 1);
    assert_eq!(reconciled.metrics.publication.rebuilt_symbols, 1);
    assert_eq!(reconciled.metrics.publication.rebuilt_call_sites, 0);
    assert_eq!(
        reconciled.metrics.publication.reused_call_sites,
        CALLERS as u64
    );
    assert!(reconciled.metrics.publication.structurally_incremental);
    let next = reconciled.graph.ok_or("reconciled graph missing")?;
    assert_eq!(next.resolve_name("Chakra::Target::Common"), [target_before]);
    assert_eq!(next.call_site_count(), CALLERS as u64);
    next.validate_consistency()?;
    Ok(())
}

#[test]
fn deleting_target_rebuilds_only_its_resolved_callers_not_same_name_calls()
-> Result<(), Box<dyn Error>> {
    const UNRELATED_CALLERS: usize = 128;
    let removed_path = RepoRelativePath::new("src/RemovedTarget.cs")?;
    let direct_caller_path = RepoRelativePath::new("src/DirectCaller.cs")?;
    let mut sources = BTreeMap::from([
        (
            removed_path.clone(),
            Arc::<str>::from(
                "namespace Removed; public class Target { public static void Common() {} }\n",
            ),
        ),
        (
            direct_caller_path,
            Arc::<str>::from(
                "namespace Removed; public class DirectCaller { public void Run() { Target.Common(); } }\n",
            ),
        ),
    ]);
    for index in 0..UNRELATED_CALLERS {
        sources.insert(
            RepoRelativePath::new(format!("src/unrelated/Pair{index:03}.cs"))?,
            Arc::<str>::from(format!(
                "namespace Other{index:03}; public class Target{index:03} {{ public static void Common() {{}} }} public class Caller{index:03} {{ public void Run() {{ Target{index:03}.Common(); }} }}\n"
            )),
        );
    }

    let (index, graph) = CSharpSyntaxIndex::from_sources(sources.clone())?;
    assert_eq!(
        graph.call_site_count(),
        (UNRELATED_CALLERS.saturating_add(1)) as u64
    );
    sources.remove(&removed_path);
    let reconciled = index.reconcile_sources(sources)?;

    assert_eq!(reconciled.metrics.deleted_files, 1);
    assert!(reconciled.metrics.publication.structurally_incremental);
    assert_eq!(reconciled.metrics.publication.rebuilt_call_sites, 1);
    assert_eq!(
        reconciled.metrics.publication.reused_call_sites,
        UNRELATED_CALLERS as u64
    );
    let next = reconciled.graph.ok_or("target deletion graph missing")?;
    assert!(next.resolve_name("Removed::Target::Common").is_empty());
    assert_eq!(next.unresolved_call_site_count(), 1);
    assert_eq!(
        next.call_site_count(),
        (UNRELATED_CALLERS.saturating_add(1)) as u64
    );
    next.validate_consistency()?;
    Ok(())
}

#[test]
fn adding_target_rebuilds_only_matching_qualified_same_name_calls() -> Result<(), Box<dyn Error>> {
    const UNRELATED_CALLERS: usize = 128;
    let target_path = RepoRelativePath::new("src/AddedTarget.cs")?;
    let mut sources = BTreeMap::from([(
        RepoRelativePath::new("src/DirectCaller.cs")?,
        Arc::<str>::from(
            "namespace Added; public class DirectCaller { public void Run() { Target.Common(); } }\n",
        ),
    )]);
    for index in 0..UNRELATED_CALLERS {
        sources.insert(
            RepoRelativePath::new(format!("src/unrelated/Pair{index:03}.cs"))?,
            Arc::<str>::from(format!(
                "namespace Other{index:03}; public class Target{index:03} {{ public static void Common() {{}} }} public class Caller{index:03} {{ public void Run() {{ Target{index:03}.Common(); }} }}\n"
            )),
        );
    }

    let (index, graph) = CSharpSyntaxIndex::from_sources(sources.clone())?;
    assert_eq!(graph.unresolved_call_site_count(), 1);
    sources.insert(
        target_path,
        Arc::<str>::from(
            "namespace Added; public class Target { public static void Common() {} }\n",
        ),
    );
    let reconciled = index.reconcile_sources(sources)?;

    assert_eq!(reconciled.metrics.created_files, 1);
    assert!(reconciled.metrics.publication.structurally_incremental);
    assert_eq!(reconciled.metrics.publication.rebuilt_call_sites, 1);
    assert_eq!(
        reconciled.metrics.publication.reused_call_sites,
        UNRELATED_CALLERS as u64
    );
    let next = reconciled.graph.ok_or("target addition graph missing")?;
    assert_eq!(next.unresolved_call_site_count(), 0);
    assert_eq!(
        next.call_site_count(),
        (UNRELATED_CALLERS.saturating_add(1)) as u64
    );
    next.validate_consistency()?;
    Ok(())
}

#[test]
fn truncated_catalog_never_turns_ambiguity_into_a_unique_call() -> Result<(), Box<dyn Error>> {
    let mut sources = BTreeMap::new();
    sources.insert(
        RepoRelativePath::new("src/a_caller.cs")?,
        Arc::<str>::from("class Invoke { void invoke() { target(); } }\n"),
    );
    sources.insert(
        RepoRelativePath::new("src/b_target.cs")?,
        Arc::<str>::from("class Target { void target() {} }\n"),
    );
    sources.insert(
        RepoRelativePath::new("src/c_target.cs")?,
        Arc::<str>::from("class Target { void target() {} }\n"),
    );
    let cancellation = IndexCancellation::default();
    let (_, graph, metrics) = CSharpSyntaxIndex::from_sources_bounded(
        sources,
        GraphBuildLimits {
            max_symbols: 2,
            max_edges: 10,
            max_call_sites: 10,
        },
        &cancellation,
    )?;

    // Each C# file declares two symbols (the class plus its method):
    // with a two-symbol budget the caller file fits and both target
    // files are omitted entirely.
    assert_eq!(metrics.graph.omitted_symbols, 4);
    assert_eq!(metrics.graph.call_sites_omitted_by_symbol_budget, 1);
    assert_eq!(graph.call_site_count(), 0);
    let call_edges = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.outgoing_edges(symbol.id))
        .filter(|edge| edge.kind == EdgeKind::Calls)
        .count();
    assert_eq!(call_edges, 0);
    graph.validate_consistency()?;
    Ok(())
}

#[test]
fn metadata_change_republishes_without_reparsing_source() -> Result<(), Box<dyn Error>> {
    let path = RepoRelativePath::new("src/lib.cs")?;
    let source = Arc::<str>::from("class Stable { void stable() {} }\n");
    let initial = CSharpSources {
        files: BTreeMap::from([(path.clone(), source.clone())]),
        metadata: BTreeMap::from([(path.clone(), SourceMetadata::path_fallback(&path))]),
    };
    let (index, _) = CSharpSyntaxIndex::from_classified_sources(initial)?;
    let changed = CSharpSources {
        files: BTreeMap::from([(path.clone(), source)]),
        metadata: BTreeMap::from([(
            path.clone(),
            SourceMetadata {
                role: chakra_domain::source::SourceRole::Production,
                classification: chakra_domain::source::SourceClassification::DotnetProjectMetadata,
                package: Some(chakra_domain::source::SourcePackage {
                    name: "app".to_owned(),
                    root: None,
                }),
            },
        )]),
    };
    let reconciled = index.reconcile_classified_sources(changed)?;
    assert_eq!(reconciled.metrics.reparsed_files, 0);
    let graph = reconciled.graph.ok_or("metadata-only graph missing")?;
    assert_eq!(
        graph
            .file_metadata(&path)
            .and_then(|metadata| metadata.package.as_ref())
            .map(|package| package.name.as_str()),
        Some("app")
    );
    Ok(())
}

#[test]
fn source_set_change_keeps_delta_when_metadata_changes_only_with_those_paths()
-> Result<(), Box<dyn Error>> {
    let stable_path = RepoRelativePath::new("src/Stable.cs")?;
    let removed_path = RepoRelativePath::new("src/BoundaryOld.cs")?;
    let created_path = RepoRelativePath::new("src/BoundaryNew.cs")?;
    let initial = BTreeMap::from([
        (stable_path.clone(), Arc::<str>::from("class Stable {}\n")),
        (removed_path, Arc::<str>::from("class BoundaryOld {}\n")),
    ]);
    let (index, _) = CSharpSyntaxIndex::from_sources(initial)?;
    let next = BTreeMap::from([
        (stable_path, Arc::<str>::from("class Stable {}\n")),
        (
            created_path.clone(),
            Arc::<str>::from("class BoundaryNew {}\n"),
        ),
    ]);

    let reconciled = index.reconcile_sources(next)?;
    assert_eq!(reconciled.metrics.created_files, 1);
    assert_eq!(reconciled.metrics.deleted_files, 1);
    assert!(reconciled.metrics.publication.structurally_incremental);
    assert_eq!(reconciled.metrics.publication.rebuilt_files, 1);
    assert_eq!(reconciled.metrics.publication.rebuilt_symbols, 1);
    let graph = reconciled.graph.ok_or("source-set delta graph missing")?;
    assert_eq!(graph.resolve_name("BoundaryNew").len(), 1);
    assert!(graph.resolve_name("BoundaryOld").is_empty());
    assert_eq!(graph.symbol_count(), 2);
    assert_eq!(
        graph
            .file_metadata(&created_path)
            .map(|metadata| metadata.classification),
        Some(chakra_domain::source::SourceClassification::PathFallback)
    );
    graph.validate_consistency()?;
    Ok(())
}

#[test]
fn rejects_a_source_larger_than_the_file_budget() -> Result<(), Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    assert!(status.success());
    fs::create_dir_all(repository.path().join("src"))?;
    let file = File::create(repository.path().join("src/large.cs"))?;
    file.set_len((MAX_SOURCE_FILE_BYTES + 1) as u64)?;

    let error = match read_sources(repository.path()) {
        Ok(_) => return Err("oversized source was indexed".into()),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        CSharpIndexError::SourceTooLarge { limit, .. } if limit == MAX_SOURCE_FILE_BYTES
    ));
    Ok(())
}
