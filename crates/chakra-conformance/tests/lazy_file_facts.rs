//! Measured lazy-vs-eager comparison for revision-scoped file facts
//! (issue #42).
//!
//! The hermetic test generates a synthetic Rust worktree, indexes it, and
//! compares computing the `FileOutlineDigest` for *every* file (eager) with
//! on-demand computation for a small `context`-style workload (lazy). The
//! assertions are deterministic (producer invocation counts, cache stats,
//! retention bounds); wall times are recorded for the evaluation record.
//!
//! The ignored variant runs the same comparison against a real external Git
//! worktree: `CHAKRA_LAZY_FACTS_WORKTREE=/path cargo test --release -p
//! chakra-conformance --test lazy_file_facts -- --ignored --nocapture`.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_engine::{
    FactStoreBounds, FileFactInput, FileOutlineDigestProducer, LazyFact, LazyFactProducer,
    LazyFactStore, SymbolGraph, WorkspaceEngine, WorkspaceSnapshot,
};
use chakra_language::index_repository;
use tempfile::TempDir;

/// Default synthetic workspace size for the hermetic CI run. The evaluation
/// record uses `CHAKRA_LAZY_FACTS_SYNTHETIC_FILES` for a larger corpus.
const DEFAULT_SYNTHETIC_FILES: usize = 400;
const FUNCTIONS_PER_FILE: usize = 24;
/// Share of files a `context`-style workload actually touches.
const WORKLOAD_DIVISOR: usize = 25;

struct Comparison {
    files: usize,
    workload_files: usize,
    eager: Phase,
    lazy: Phase,
    lazy_stats: chakra_engine::LazyFactStats,
}

struct Phase {
    wall: Duration,
    computations: u64,
    retained_bytes: u64,
}

fn git(root: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let output = Command::new("git").current_dir(root).args(args).output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}

fn synthetic_source(file_index: usize) -> String {
    let mut source = String::new();
    for function in 0..FUNCTIONS_PER_FILE {
        source.push_str(&format!(
            "pub fn file_{file_index}_fn_{function}(value: u64) -> u64 {{ value + {function} }}\n"
        ));
    }
    source
}

fn synthetic_workspace(files: usize) -> Result<TempDir, Box<dyn Error>> {
    let repository = TempDir::new()?;
    let src = repository.path().join("src");
    fs::create_dir_all(&src)?;
    for index in 0..files {
        fs::write(
            src.join(format!("file_{index:05}.rs")),
            synthetic_source(index),
        )?;
    }
    git(repository.path(), &["init", "--quiet"])?;
    Ok(repository)
}

fn publish_snapshot(repository: &Path) -> Result<Arc<WorkspaceSnapshot>, Box<dyn Error>> {
    let report = index_repository(repository)?;
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    Ok(engine.publish(update)?)
}

fn input_for<'a>(
    graph: &'a SymbolGraph,
    snapshot: &WorkspaceSnapshot,
    path: &'a RepoRelativePath,
) -> Result<FileFactInput<'a>, Box<dyn Error>> {
    let source = graph
        .file_source(path)
        .ok_or_else(|| format!("no retained source for {path:?}"))?;
    Ok(FileFactInput {
        path,
        revision: snapshot.revision(),
        source,
        graph,
    })
}

/// Runs the eager-vs-lazy comparison against one indexed snapshot.
///
/// Eager: every indexed file's digest is computed up front (what an eager
/// indexer would pay at revision publication). Lazy: a workload touching
/// `files / WORKLOAD_DIVISOR` files is served through a bounded store, each
/// file requested twice (second request must be a cache hit).
fn compare(snapshot: &WorkspaceSnapshot) -> Result<Comparison, Box<dyn Error>> {
    let graph = snapshot.graph();
    let files: Vec<RepoRelativePath> = snapshot
        .graph()
        .file_summaries()
        .into_iter()
        .map(|summary| summary.path)
        .collect();
    let producer = FileOutlineDigestProducer::new();
    let operation = OperationContext::unbounded();

    // Eager: compute the digest for every file at "index time".
    let eager_started = Instant::now();
    let mut eager_retained = 0_u64;
    for path in &files {
        let fact = producer.compute(&input_for(graph, snapshot, path)?, &operation)?;
        eager_retained = eager_retained.saturating_add(fact.retained_bytes() as u64);
    }
    let eager = Phase {
        wall: eager_started.elapsed(),
        computations: files.len() as u64,
        retained_bytes: eager_retained,
    };

    // Lazy: serve the workload through a bounded store; every file twice.
    let store = LazyFactStore::new(
        Arc::new(FileOutlineDigestProducer::new()),
        FactStoreBounds::default(),
    );
    let workload: Vec<&RepoRelativePath> = files.iter().step_by(WORKLOAD_DIVISOR).collect();
    let lazy_started = Instant::now();
    for path in &workload {
        let first = store.get_or_compute(&input_for(graph, snapshot, path)?, &operation)?;
        let second = store.get_or_compute(&input_for(graph, snapshot, path)?, &operation)?;
        assert_eq!(first.origin, chakra_engine::FactOrigin::Computed);
        assert_eq!(second.origin, chakra_engine::FactOrigin::Cached);
        assert_eq!(second.provenance, Provenance::TreeSitter);
        assert_eq!(second.precision, Precision::Syntax);
        assert!(!first.fact.digest.is_empty());
    }
    let lazy_stats = store.stats();
    let lazy = Phase {
        wall: lazy_started.elapsed(),
        computations: lazy_stats.misses,
        retained_bytes: lazy_stats.retained_bytes,
    };

    Ok(Comparison {
        files: files.len(),
        workload_files: workload.len(),
        eager,
        lazy,
        lazy_stats,
    })
}

fn report(comparison: &Comparison) {
    println!("lazy file facts comparison (issue #42)");
    println!("  files indexed:        {}", comparison.files);
    println!("  workload files:       {}", comparison.workload_files);
    println!(
        "  eager:                {} computations, {:.3} ms, {} retained bytes",
        comparison.eager.computations,
        comparison.eager.wall.as_secs_f64() * 1_000.0,
        comparison.eager.retained_bytes,
    );
    println!(
        "  lazy:                 {} computations, {:.3} ms, {} retained bytes",
        comparison.lazy.computations,
        comparison.lazy.wall.as_secs_f64() * 1_000.0,
        comparison.lazy.retained_bytes,
    );
    println!(
        "  lazy store stats:     hits={} misses={} evictions={} failures={} entries={}",
        comparison.lazy_stats.hits,
        comparison.lazy_stats.misses,
        comparison.lazy_stats.evictions,
        comparison.lazy_stats.failures,
        comparison.lazy_stats.entries,
    );
}

#[test]
fn lazy_facts_beat_eager_under_sparse_workload() -> Result<(), Box<dyn Error>> {
    let files = std::env::var("CHAKRA_LAZY_FACTS_SYNTHETIC_FILES")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(DEFAULT_SYNTHETIC_FILES);
    let repository = synthetic_workspace(files)?;
    let snapshot = publish_snapshot(repository.path())?;

    let comparison = compare(&snapshot)?;
    report(&comparison);

    // Deterministic core of the measured case: eager pays one computation
    // per indexed file; lazy pays one per *touched* file and serves repeats
    // from the bounded store.
    assert_eq!(comparison.eager.computations, comparison.files as u64);
    assert_eq!(
        comparison.lazy.computations,
        comparison.workload_files as u64
    );
    assert!(comparison.lazy.computations < comparison.eager.computations);
    assert_eq!(comparison.lazy_stats.hits, comparison.workload_files as u64);
    assert_eq!(comparison.lazy_stats.failures, 0);
    assert!(
        comparison.lazy_stats.retained_bytes <= FactStoreBounds::default().max_total_bytes as u64
    );
    Ok(())
}

#[test]
#[ignore = "set CHAKRA_LAZY_FACTS_WORKTREE to an external Git worktree"]
fn lazy_facts_beat_eager_on_real_corpus() -> Result<(), Box<dyn Error>> {
    let worktree = PathBuf::from(
        std::env::var_os("CHAKRA_LAZY_FACTS_WORKTREE")
            .ok_or("CHAKRA_LAZY_FACTS_WORKTREE must name an external Git worktree")?,
    );
    let snapshot = publish_snapshot(&worktree)?;
    let comparison = compare(&snapshot)?;
    report(&comparison);

    assert_eq!(comparison.eager.computations, comparison.files as u64);
    assert_eq!(
        comparison.lazy.computations,
        comparison.workload_files as u64
    );
    assert_eq!(comparison.lazy_stats.failures, 0);
    Ok(())
}
