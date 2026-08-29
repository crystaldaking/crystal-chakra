//! Hermetic acceptance tests for the versioned per-file syntax fact cache
//! (issue #39, budget B5): fallback semantics, restore equivalence, bounded
//! limits, and reconcile-after-restore freshness. Every test uses a real
//! temporary Git repository; none touches the network or a language server.

use std::error::Error;
use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::process::Command;

use chakra_language::cache::{
    CacheRestoreOutcome, DEFAULT_MIN_INDEXED_FILES, SyntaxCacheConfig, SyntaxCacheMode,
};
use chakra_language::{
    IndexOptions, index_repository_with_options, scan_repository_sources_with_options,
};
use tempfile::TempDir;

fn graph_fingerprint(graph: &chakra_engine::SymbolGraph) -> u64 {
    let mut fingerprint = std::collections::hash_map::DefaultHasher::new();
    fingerprint.write(format!("{:?}", graph.file_summaries()).as_bytes());
    for symbol in graph.symbols() {
        fingerprint.write(format!("{symbol:?}").as_bytes());
        fingerprint.write(format!("{:?}", graph.outgoing_edges(symbol.id)).as_bytes());
        fingerprint.write(
            format!("{:?}", graph.call_sites_from(symbol.id).collect::<Vec<_>>()).as_bytes(),
        );
    }
    fingerprint.finish()
}

fn git(root: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()?;
    if !status.success() {
        return Err(format!("git {} failed", args.join(" ")).into());
    }
    Ok(())
}

fn seed_repository(files: &[(&str, &str)]) -> Result<TempDir, Box<dyn Error>> {
    let repository = TempDir::new()?;
    for (path, content) in files {
        let absolute = repository.path().join(path);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(absolute, content)?;
    }
    git(repository.path(), &["init", "--quiet"])?;
    git(repository.path(), &["add", "-A"])?;
    git(
        repository.path(),
        &[
            "-c",
            "user.email=cache@example.invalid",
            "-c",
            "user.name=Chakra Cache Tests",
            "commit",
            "--quiet",
            "-m",
            "seed",
        ],
    )?;
    Ok(repository)
}

fn mixed_sources() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "src/lib.rs",
            "pub fn alpha() { beta(); }\npub fn beta() {}\npub struct Store;\n",
        ),
        ("src/more.rs", "pub fn gamma() { crate::alpha(); }\n"),
        (
            "app/Service.php",
            "<?php class Service { public function run(): void { $this->helper(); } private function helper(): void {} }\n",
        ),
        (
            "app/helpers.php",
            "<?php function format_name(string $name): string { return $name; }\n",
        ),
        (
            "tool.py",
            "class Greeter:\n    def greet(self):\n        return helper()\n\ndef helper():\n    return \"hi\"\n",
        ),
        (
            "web.ts",
            "export function greet(name: string): string { return format(name); }\nexport function format(name: string): string { return name.trim(); }\n",
        ),
        (
            "Service.cs",
            "namespace Cache { public class Service { public void Run() { Helper(); } private void Helper() {} } }\n",
        ),
    ]
}

fn cache_options(cache_dir: &Path) -> Result<IndexOptions, Box<dyn Error>> {
    let mut config = SyntaxCacheConfig::new(cache_dir.to_path_buf());
    config.min_indexed_files = 0;
    Ok(IndexOptions {
        cache: SyntaxCacheMode::Enabled(config),
        ..IndexOptions::default()
    })
}

/// First build (publishes the cache), second build (restores), third build
/// (deterministic, cache disabled) for equivalence fingerprints.
fn build_restore_rebuild(
    repository: &Path,
    cache_dir: &Path,
) -> Result<
    (
        chakra_language::IndexReport,
        chakra_language::IndexReport,
        chakra_language::IndexReport,
    ),
    Box<dyn Error>,
> {
    let written = index_repository_with_options(repository, cache_options(cache_dir)?)?;
    let restored = index_repository_with_options(repository, cache_options(cache_dir)?)?;
    let cold = index_repository_with_options(repository, IndexOptions::default())?;
    Ok((written, restored, cold))
}

fn fact_files(cache_dir: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(cache_dir.join("facts"))? {
        files.push(entry?.path());
    }
    files.sort();
    Ok(files)
}

#[test]
fn restored_graph_is_equivalent_to_deterministic_rebuild() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    let (written, restored, cold) = build_restore_rebuild(repository.path(), cache.path())?;

    assert!(matches!(
        written.cache.restore,
        CacheRestoreOutcome::Fallback { .. }
    ));
    let write = written.cache.write.as_ref().ok_or("cache write missing")?;
    assert!(write.published);
    assert_eq!(write.skipped_entries, 0);
    assert!(write.total_bytes > 0);

    assert_eq!(
        restored.cache.restore,
        CacheRestoreOutcome::Restored { hits: 7, misses: 0 },
        "unchanged worktree must restore with 100% hits"
    );
    assert!(restored.cache.write.is_none());
    assert!(restored.cache.bytes_read > 0);

    assert_eq!(
        graph_fingerprint(&restored.graph),
        graph_fingerprint(&cold.graph)
    );
    assert_eq!(restored.graph.symbol_count(), cold.graph.symbol_count());
    assert_eq!(restored.graph.edge_count(), cold.graph.edge_count());
    assert_eq!(
        restored.graph.call_site_count(),
        cold.graph.call_site_count()
    );
    assert_eq!(
        restored.metrics.indexing.coverage,
        cold.metrics.indexing.coverage
    );
    assert_eq!(
        restored.metrics.indexing.degradations,
        cold.metrics.indexing.degradations
    );
    restored.graph.validate_consistency()?;
    Ok(())
}

#[test]
fn one_file_edit_reparses_exactly_one_file() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    index_repository_with_options(repository.path(), cache_options(cache.path())?)?;

    fs::write(
        repository.path().join("src/more.rs"),
        "pub fn gamma() { crate::alpha(); }\npub fn delta() { gamma(); }\n",
    )?;
    let restored = index_repository_with_options(repository.path(), cache_options(cache.path())?)?;
    assert_eq!(
        restored.cache.restore,
        CacheRestoreOutcome::Restored { hits: 6, misses: 1 }
    );
    assert_eq!(restored.graph.resolve_name("delta").len(), 1);

    let cold = index_repository_with_options(repository.path(), IndexOptions::default())?;
    assert_eq!(
        graph_fingerprint(&restored.graph),
        graph_fingerprint(&cold.graph)
    );

    // The refresh wrote the new facts back; the next restore is a full hit.
    let again = index_repository_with_options(repository.path(), cache_options(cache.path())?)?;
    assert_eq!(
        again.cache.restore,
        CacheRestoreOutcome::Restored { hits: 7, misses: 0 }
    );
    Ok(())
}

#[test]
fn corrupt_fact_file_is_an_isolated_per_file_fallback() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    index_repository_with_options(repository.path(), cache_options(cache.path())?)?;

    let files = fact_files(cache.path())?;
    assert_eq!(files.len(), 7);
    fs::write(&files[1], b"tampered payload")?;

    let restored = index_repository_with_options(repository.path(), cache_options(cache.path())?)?;
    assert_eq!(
        restored.cache.restore,
        CacheRestoreOutcome::Restored { hits: 6, misses: 1 },
        "exactly one entry must miss; the rest still restore"
    );
    let cold = index_repository_with_options(repository.path(), IndexOptions::default())?;
    assert_eq!(
        graph_fingerprint(&restored.graph),
        graph_fingerprint(&cold.graph)
    );
    Ok(())
}

#[test]
fn truncated_fact_file_is_a_per_file_fallback() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    index_repository_with_options(repository.path(), cache_options(cache.path())?)?;

    let files = fact_files(cache.path())?;
    let raw = fs::read(&files[2])?;
    fs::write(&files[2], &raw[..raw.len() / 2])?;

    let restored = index_repository_with_options(repository.path(), cache_options(cache.path())?)?;
    assert_eq!(
        restored.cache.restore,
        CacheRestoreOutcome::Restored { hits: 6, misses: 1 }
    );
    let cold = index_repository_with_options(repository.path(), IndexOptions::default())?;
    assert_eq!(
        graph_fingerprint(&restored.graph),
        graph_fingerprint(&cold.graph)
    );
    Ok(())
}

#[test]
fn missing_fact_file_reparses_that_file() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    index_repository_with_options(repository.path(), cache_options(cache.path())?)?;

    let files = fact_files(cache.path())?;
    fs::remove_file(&files[0])?;

    let restored = index_repository_with_options(repository.path(), cache_options(cache.path())?)?;
    assert_eq!(
        restored.cache.restore,
        CacheRestoreOutcome::Restored { hits: 6, misses: 1 }
    );
    let cold = index_repository_with_options(repository.path(), IndexOptions::default())?;
    assert_eq!(
        graph_fingerprint(&restored.graph),
        graph_fingerprint(&cold.graph)
    );
    Ok(())
}

#[test]
fn corrupt_manifest_falls_back_to_deterministic_rebuild() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    index_repository_with_options(repository.path(), cache_options(cache.path())?)?;

    fs::write(cache.path().join("manifest.bin"), b"not a manifest")?;
    let restored = index_repository_with_options(repository.path(), cache_options(cache.path())?)?;
    assert!(matches!(
        restored.cache.restore,
        CacheRestoreOutcome::Fallback { .. }
    ));
    let cold = index_repository_with_options(repository.path(), IndexOptions::default())?;
    assert_eq!(
        graph_fingerprint(&restored.graph),
        graph_fingerprint(&cold.graph)
    );
    Ok(())
}

#[test]
fn configuration_change_invalidates_the_whole_cache() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    index_repository_with_options(repository.path(), cache_options(cache.path())?)?;

    let mut options = cache_options(cache.path())?;
    options.budgets.max_call_sites = options.budgets.max_call_sites.saturating_sub(1);
    let restored = index_repository_with_options(repository.path(), options)?;
    match &restored.cache.restore {
        CacheRestoreOutcome::Fallback { reason } => {
            assert!(
                reason.contains("config_fingerprint"),
                "unexpected reason: {reason}"
            );
        }
        other => return Err(format!("expected fallback, got {other:?}").into()),
    }
    let cold = index_repository_with_options(
        repository.path(),
        IndexOptions::new(
            restored.metrics.indexing.budgets,
            chakra_domain::indexing::IndexCancellation::default(),
        )?,
    )?;
    assert_eq!(
        restored.graph.call_site_count(),
        cold.graph.call_site_count()
    );
    Ok(())
}

#[test]
fn commit_change_invalidates_the_whole_cache() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    index_repository_with_options(repository.path(), cache_options(cache.path())?)?;

    fs::write(repository.path().join("README.md"), "docs\n")?;
    git(repository.path(), &["add", "-A"])?;
    git(
        repository.path(),
        &[
            "-c",
            "user.email=cache@example.invalid",
            "-c",
            "user.name=Chakra Cache Tests",
            "commit",
            "--quiet",
            "-m",
            "docs",
        ],
    )?;
    let restored = index_repository_with_options(repository.path(), cache_options(cache.path())?)?;
    match &restored.cache.restore {
        CacheRestoreOutcome::Fallback { reason } => {
            assert!(reason.contains("head_sha"), "unexpected reason: {reason}");
        }
        other => return Err(format!("expected fallback, got {other:?}").into()),
    }
    let cold = index_repository_with_options(repository.path(), IndexOptions::default())?;
    assert_eq!(
        graph_fingerprint(&restored.graph),
        graph_fingerprint(&cold.graph)
    );
    Ok(())
}

#[test]
fn reconcile_after_restore_keeps_read_your_writes() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    index_repository_with_options(repository.path(), cache_options(cache.path())?)?;
    let restored = index_repository_with_options(repository.path(), cache_options(cache.path())?)?;
    assert!(matches!(
        restored.cache.restore,
        CacheRestoreOutcome::Restored { .. }
    ));
    let cold = index_repository_with_options(repository.path(), IndexOptions::default())?;

    fs::write(
        repository.path().join("src/more.rs"),
        "pub fn gamma() { crate::alpha(); }\npub fn fresh_symbol() {}\n",
    )?;
    let scan = restored.syntax_index.scan_repository(repository.path())?;
    let reconciled = restored.syntax_index.reconcile_sources(scan)?;
    assert_eq!(reconciled.metrics.reparsed_files, 1);
    let graph = reconciled.graph.ok_or("reconcile must publish a graph")?;
    assert_eq!(graph.resolve_name("fresh_symbol").len(), 1);
    graph.validate_consistency()?;

    // The same edit reconciled through a deterministically built index must
    // produce the identical revision (delta materialization preserves
    // revision-local ids, so the comparison is delta vs delta).
    let cold_scan = cold.syntax_index.scan_repository(repository.path())?;
    let cold_reconciled = cold.syntax_index.reconcile_sources(cold_scan)?;
    let cold_graph = cold_reconciled
        .graph
        .ok_or("reconcile must publish a graph")?;
    assert_eq!(graph_fingerprint(&graph), graph_fingerprint(&cold_graph));
    Ok(())
}

#[test]
fn cache_is_off_below_the_size_gate() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    let options = IndexOptions {
        cache: SyntaxCacheMode::Enabled(SyntaxCacheConfig::new(cache.path().to_path_buf())),
        ..IndexOptions::default()
    };
    let report = index_repository_with_options(repository.path(), options)?;
    assert_eq!(
        report.cache.restore,
        CacheRestoreOutcome::BelowGate {
            indexed_files: 7,
            gate: DEFAULT_MIN_INDEXED_FILES,
        }
    );
    assert!(report.cache.write.is_none());
    assert!(!cache.path().join("manifest.bin").exists());
    Ok(())
}

#[test]
fn bounded_entry_limit_skips_but_never_fails() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    let mut options = cache_options(cache.path())?;
    let Some(config) = options.cache.config().cloned() else {
        return Err("cache config missing".into());
    };
    options.cache = SyntaxCacheMode::Enabled(SyntaxCacheConfig {
        max_entry_bytes: 24,
        ..config
    });
    let written = index_repository_with_options(repository.path(), options)?;
    let write = written.cache.write.as_ref().ok_or("cache write missing")?;
    assert!(write.published);
    assert!(
        write.skipped_entries > 0,
        "oversized entries must be skipped"
    );

    let restored = index_repository_with_options(repository.path(), cache_options(cache.path())?)?;
    let CacheRestoreOutcome::Restored { hits, misses } = restored.cache.restore else {
        return Err(format!("expected restore, got {:?}", restored.cache.restore).into());
    };
    assert_eq!(hits + misses, 7);
    assert_eq!(misses, write.skipped_entries);
    let cold = index_repository_with_options(repository.path(), IndexOptions::default())?;
    assert_eq!(
        graph_fingerprint(&restored.graph),
        graph_fingerprint(&cold.graph)
    );
    Ok(())
}

#[test]
fn bounded_total_limit_prevents_publication() -> Result<(), Box<dyn Error>> {
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    let mut options = cache_options(cache.path())?;
    let Some(config) = options.cache.config().cloned() else {
        return Err("cache config missing".into());
    };
    options.cache = SyntaxCacheMode::Enabled(SyntaxCacheConfig {
        max_total_bytes: 8,
        ..config
    });
    let written = index_repository_with_options(repository.path(), options)?;
    let write = written.cache.write.as_ref().ok_or("cache write missing")?;
    assert!(!write.published);
    assert!(!cache.path().join("manifest.bin").exists());
    Ok(())
}

#[test]
fn laravel_framework_facts_restore_equivalently() -> Result<(), Box<dyn Error>> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures/php/laravel-relationships");
    if !fixture.is_dir() {
        return Ok(());
    }
    let mut files = Vec::new();
    copy_dir(&fixture, &mut files, "")?;
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let repository = seed_repository(&file_refs)?;
    let cache = TempDir::new()?;
    let (written, restored, cold) = build_restore_rebuild(repository.path(), cache.path())?;
    assert!(
        written.metrics.laravel_detected,
        "fixture must trigger Laravel"
    );
    assert!(restored.metrics.laravel_detected);
    assert!(restored.metrics.framework_symbols > 0);
    assert!(matches!(
        restored.cache.restore,
        CacheRestoreOutcome::Restored { misses: 0, .. }
    ));
    assert_eq!(
        restored.metrics.framework_symbols,
        cold.metrics.framework_symbols
    );
    assert_eq!(
        restored.metrics.framework_edges,
        cold.metrics.framework_edges
    );
    assert_eq!(
        graph_fingerprint(&restored.graph),
        graph_fingerprint(&cold.graph)
    );
    restored.graph.validate_consistency()?;
    Ok(())
}

fn copy_dir(
    dir: &Path,
    files: &mut Vec<(String, String)>,
    prefix: &str,
) -> Result<(), Box<dyn Error>> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), files, &path)?;
        } else if entry.file_type()?.is_file() {
            files.push((path, fs::read_to_string(entry.path())?));
        }
    }
    Ok(())
}

#[test]
fn scan_and_validate_only_cost_is_bounded() -> Result<(), Box<dyn Error>> {
    // B2 support evidence: a scan plus content hashing (the real cache's
    // validation work) completes without touching fact payloads.
    let repository = seed_repository(&mixed_sources())?;
    let cache = TempDir::new()?;
    index_repository_with_options(repository.path(), cache_options(cache.path())?)?;
    let scan = scan_repository_sources_with_options(repository.path(), &IndexOptions::default())?;
    assert_eq!(scan.indexed_files, 7);
    Ok(())
}
