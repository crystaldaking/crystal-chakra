//! Persistence benchmark pipeline (issue #38): one target at a time.
//!
//! A *target* is either a small in-repository fixture (seeded into a
//! temporary Git worktree, so the harness stays hermetic) or a pinned public
//! corpus checkout under `target/corpus`. For every run the pipeline
//! measures:
//!
//! 1. **cold rebuild** — `index_repository_with_options` from scratch;
//! 2. **cache write** — projection build plus model-cache serialization;
//! 3. **warm restore** — compatibility check, source-hash validation, and
//!    deserialization of every hit;
//! 4. **validation only** — compatibility check plus source hashes, no fact
//!    reads;
//! 5. **one-file refresh** — one appended declaration, then restore of the
//!    hits plus a targeted `reconcile_sources` reparse of the miss, compared
//!    against the cold rebuild.
//!
//! The refresh edit is always reverted; corpus checkouts are verified clean
//! afterwards (the fetch tool's `.chakra-corpus.json` metadata excepted),
//! same as the corpus runner. Nothing here touches the network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chakra_language::{IndexOptions, index_repository_with_options};
use tempfile::TempDir;

use super::model::{
    CompatibilityKey, PhaseTimer, RestoreOutcome, restore_cache, validate_cache, write_cache,
};
use super::projection::{MODEL_FORMAT_VERSION, build_projection};
use super::report::{
    CacheWriteReport, ColdRebuildReport, CorpusFingerprint, IndexConfigContext,
    OneFileRefreshReport, PersistenceReport, PhaseReport, RestorePhaseReport, RunReport,
    TargetKind,
};
use crate::fixture::copy_fixture_tree;
use crate::{Check, failure};

/// Untracked metadata file written by `tools/fetch_corpus.py`; excluded from
/// the clean-worktree assertion.
const CACHE_METADATA_FILE: &str = ".chakra-corpus.json";

/// One benchmark target: a fixture tree or a pinned corpus checkout.
#[derive(Debug, Clone)]
pub struct PersistenceTarget {
    /// Stable name, e.g. `fixture/rust/controller-service-provider` or
    /// `corpus/php/laravel/framework`.
    pub name: String,
    pub kind: TargetKind,
    pub language: String,
    /// Fixture source tree (fixture targets only).
    pub fixture_dir: Option<PathBuf>,
    /// Cached checkout (corpus targets only).
    pub checkout: Option<PathBuf>,
    /// Pinned SHA for corpus targets; empty for fixtures (the seed commit is
    /// recorded at evaluation time).
    pub sha: String,
}

/// Fixture targets: every `fixtures/{rust,php}/<name>` tree.
pub fn fixture_targets(workspace_root: &Path) -> Check<Vec<PersistenceTarget>> {
    let mut targets = Vec::new();
    for language in ["rust", "php"] {
        let root = workspace_root.join("fixtures").join(language);
        if !root.is_dir() {
            continue;
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                entries.push(entry.path());
            }
        }
        entries.sort();
        for dir in entries {
            let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            targets.push(PersistenceTarget {
                name: format!("fixture/{language}/{name}"),
                kind: TargetKind::Fixture,
                language: language.to_owned(),
                fixture_dir: Some(dir),
                checkout: None,
                sha: String::new(),
            });
        }
    }
    Ok(targets)
}

/// Corpus targets: every manifest repository of the selected languages.
pub fn corpus_targets(
    manifest: &crate::corpus::CorpusManifest,
    languages: &[String],
    cache_root: &Path,
) -> Vec<PersistenceTarget> {
    let mut targets = Vec::new();
    for language in languages {
        let Some(entry) = manifest.languages.get(language) else {
            continue;
        };
        for repo in &entry.repositories {
            targets.push(PersistenceTarget {
                name: format!("corpus/{language}/{}", repo.name),
                kind: TargetKind::Corpus,
                language: language.clone(),
                fixture_dir: None,
                checkout: Some(cache_root.join(repo.slug())),
                sha: repo.sha.clone(),
            });
        }
    }
    targets
}

/// Languages the refresh probe can edit. Other languages are measured for
/// rebuild/write/restore only when a probe plan exists; today that is Rust
/// and PHP (the v0.1 baseline languages).
pub(crate) fn probe_declaration(language: &str) -> Option<(&'static [&'static str], &'static str)> {
    match language {
        "rust" => Some((&["rs"], "\npub fn chakra_persistence_probe() {}\n")),
        "php" => Some((&["php"], "\nfunction chakra_persistence_probe(): void {}\n")),
        _ => None,
    }
}

/// Resolved worktree of one target, or the reason it cannot be evaluated.
pub(crate) enum TargetCheckout {
    Resolved {
        checkout: PathBuf,
        sha: String,
        /// Keeps the temporary fixture repository alive.
        fixture: Option<TempDir>,
    },
    Skipped(String),
}

/// Resolves the worktree of one target: corpus checkouts are used in place
/// (SHA-verified); fixtures are seeded into a temporary Git repository.
pub(crate) fn resolve_target_checkout(target: &PersistenceTarget) -> Check<TargetCheckout> {
    match target.kind {
        TargetKind::Corpus => {
            let checkout = target
                .checkout
                .clone()
                .ok_or_else(|| failure("corpus target without a checkout path"))?;
            if !checkout.is_dir() {
                return Ok(TargetCheckout::Skipped(format!(
                    "checkout not cached at {}; fetch with `python3 tools/fetch_corpus.py`",
                    checkout.display()
                )));
            }
            let head = git(&checkout, &["rev-parse", "HEAD"])?;
            if head != target.sha {
                return Ok(TargetCheckout::Skipped(format!(
                    "checkout HEAD {head} does not match pinned SHA {}; refusing to evaluate",
                    target.sha
                )));
            }
            Ok(TargetCheckout::Resolved {
                checkout,
                sha: target.sha.clone(),
                fixture: None,
            })
        }
        TargetKind::Fixture => {
            let fixture_dir = target
                .fixture_dir
                .clone()
                .ok_or_else(|| failure("fixture target without a fixture directory"))?;
            let seeded = seed_fixture(&fixture_dir)?;
            let sha = git(seeded.path(), &["rev-parse", "HEAD"])?;
            let checkout = seeded.path().to_path_buf();
            Ok(TargetCheckout::Resolved {
                checkout,
                sha,
                fixture: Some(seeded),
            })
        }
    }
}

/// Evaluates one target and returns its artifact report. Missing or
/// SHA-mismatched corpus checkouts and unsupported languages produce skipped
/// reports, never errors.
pub fn evaluate_target(
    target: &PersistenceTarget,
    runs: u32,
    spool_dir: &Path,
) -> Check<PersistenceReport> {
    if runs == 0 {
        return Err(failure("persistence benchmark needs at least one run").into());
    }
    if probe_declaration(&target.language).is_none() {
        return Ok(PersistenceReport::skipped(
            &target.name,
            target.kind,
            &target.sha,
            format!(
                "unsupported language: no persistence probe for `{}`",
                target.language
            ),
        ));
    }

    // Resolve the worktree: corpus checkouts are used in place; fixtures are
    // seeded into a temporary Git repository kept alive for the evaluation.
    let (checkout, sha, fixture_repo) = match resolve_target_checkout(target)? {
        TargetCheckout::Resolved {
            checkout,
            sha,
            fixture,
        } => (checkout, sha, fixture),
        TargetCheckout::Skipped(reason) => {
            return Ok(PersistenceReport::skipped(
                &target.name,
                target.kind,
                &target.sha,
                reason,
            ));
        }
    };

    let mut reports = Vec::new();
    let mut context: Option<(CorpusFingerprint, IndexConfigContext)> = None;
    for run in 1..=runs {
        let (run_report, run_context) = evaluate_run(run, target, &checkout, &sha, spool_dir)?;
        if context.is_none() {
            context = Some(run_context);
        }
        reports.push(run_report);
    }
    drop(fixture_repo);
    let (corpus, index_config) =
        context.ok_or_else(|| failure("persistence benchmark produced no runs"))?;
    Ok(PersistenceReport::measured(
        &target.name,
        target.kind,
        &sha,
        corpus,
        index_config,
        reports,
    ))
}

/// One full measurement run against `checkout`.
fn evaluate_run(
    run: u32,
    target: &PersistenceTarget,
    checkout: &Path,
    sha: &str,
    spool_dir: &Path,
) -> Check<(RunReport, (CorpusFingerprint, IndexConfigContext))> {
    // --- cold rebuild -----------------------------------------------------
    let timer = PhaseTimer::start();
    let report = index_repository_with_options(checkout, IndexOptions::default())?;
    let cold_phase = timer.finish();
    let cold_rebuild = ColdRebuildReport {
        phase: PhaseReport::from_measurement(&cold_phase),
        parsed_files: report.metrics.parsed_files,
        symbols: report.metrics.symbols,
        edges: report.metrics.edges,
        call_sites: report.metrics.call_sites,
        degraded: report.metrics.indexing.is_degraded(),
        indexer_phase_peak_rss_bytes: report.metrics.indexing.memory.observed_phase_peak_rss_bytes,
    };

    // --- cache write ------------------------------------------------------
    let projection_timer = PhaseTimer::start();
    let facts = build_projection(&report.graph)?;
    let projection = projection_timer.finish();
    let key = CompatibilityKey::new(&target.name, sha, &IndexOptions::default().budgets, &facts)?;
    let cache_dir = TempDir::new_in(spool_dir)?;
    let written = write_cache(cache_dir.path(), &key, &facts)?;
    let cache_write = CacheWriteReport {
        phase: PhaseReport::from_measurement(&written.measurement),
        projection_wall_micros: projection.wall_micros,
        projection_cpu_micros: projection.cpu_micros,
        fact_files: u64::try_from(facts.len()).unwrap_or(u64::MAX),
        declarations: facts
            .iter()
            .map(|file| u64::try_from(file.declarations.len()).unwrap_or(u64::MAX))
            .sum(),
        relationships: facts
            .iter()
            .map(|file| u64::try_from(file.relationships.len()).unwrap_or(u64::MAX))
            .sum(),
        call_candidates: facts
            .iter()
            .map(|file| u64::try_from(file.call_candidates.len()).unwrap_or(u64::MAX))
            .sum(),
        omitted_facts: facts.iter().map(|f| f.omitted_facts()).sum(),
    };

    // --- warm restore / validation only ------------------------------------
    let warm_restore = restore_report(restore_cache(cache_dir.path(), &key, checkout)?);
    let validation_only = restore_report(validate_cache(cache_dir.path(), &key, checkout)?);

    // --- one-file refresh ---------------------------------------------------
    let one_file_refresh = measure_one_file_refresh(target, checkout, &report, &key, &cache_dir)?;

    let context = (
        CorpusFingerprint {
            files: report.graph.file_count(),
            source_bytes: report.metrics.indexing.coverage.source_bytes,
            content_fingerprint: key.content_fingerprint.clone(),
        },
        IndexConfigContext {
            model_format_version: MODEL_FORMAT_VERSION,
            budgets: IndexOptions::default().budgets,
            compatibility_key: key.fingerprint(),
        },
    );
    Ok((
        RunReport {
            run,
            cold_rebuild,
            cache_write,
            warm_restore,
            validation_only,
            one_file_refresh,
        },
        context,
    ))
}

fn restore_report(outcome: RestoreOutcome) -> RestorePhaseReport {
    RestorePhaseReport {
        phase: PhaseReport::from_measurement(&outcome.measurement),
        compatible: outcome.compatible,
        hits: outcome.hits,
        misses: outcome.misses,
        hit_ratio_per_mille: outcome.hit_ratio_per_mille(),
    }
}

/// Appends one probe declaration, restores the hits, reparses the miss via
/// the public reconciliation path, then reverts the edit. Corpus checkouts
/// are verified clean afterwards.
fn measure_one_file_refresh(
    target: &PersistenceTarget,
    checkout: &Path,
    report: &chakra_language::IndexReport,
    key: &CompatibilityKey,
    cache_dir: &TempDir,
) -> Check<OneFileRefreshReport> {
    let (extensions, declaration) = probe_declaration(&target.language)
        .ok_or_else(|| failure(format!("no probe for `{}`", target.language)))?;
    let mut probe_file = None;
    for path in report.syntax_index.paths() {
        if !extensions
            .iter()
            .any(|extension| path.as_str().ends_with(&format!(".{extension}")))
        {
            continue;
        }
        if target.language == "php" {
            let content = report.graph.file_source(&path).unwrap_or_default();
            if content.trim_end().ends_with("?>") {
                continue;
            }
        }
        probe_file = Some(path.as_str().to_owned());
        break;
    }
    let probe_file =
        probe_file.ok_or_else(|| failure("no indexable probe file for the refresh edit"))?;
    let absolute = checkout.join(&probe_file);
    let original = fs::read(&absolute)?;

    let refresh = (|| -> Check<OneFileRefreshReport> {
        let mut edited = original.clone();
        edited.extend_from_slice(declaration.as_bytes());
        fs::write(&absolute, edited)?;

        // Restore after the edit: every hit is deserialized, the edited file
        // is the one miss a real cache would reparse.
        let restore = restore_report(restore_cache(cache_dir.path(), key, checkout)?);

        let reconcile_timer = PhaseTimer::start();
        let scan = report.syntax_index.scan_repository(checkout)?;
        let reconciled = report.syntax_index.reconcile_sources(scan)?;
        let reconcile = reconcile_timer.finish();

        Ok(OneFileRefreshReport {
            edited_file: probe_file.clone(),
            total_wall_micros: restore
                .phase
                .wall_micros
                .saturating_add(reconcile.wall_micros),
            restore,
            reconcile_wall_micros: reconcile.wall_micros,
            reconcile_cpu_micros: reconcile.cpu_micros,
            scanned_files: reconciled.metrics.scanned_files,
            files_reparsed: reconciled.metrics.reparsed_files,
            framework_files_reparsed: reconciled.metrics.framework_files_reparsed,
        })
    })();

    // The edit is always reverted, even when the measurement failed; a pinned
    // corpus checkout must never be left dirty.
    fs::write(&absolute, &original)?;
    verify_clean_checkout(checkout)?;
    refresh
}

/// Copies a fixture tree into a fresh Git repository inside the system temp
/// directory and commits it with a fixed identity.
pub(crate) fn seed_fixture(fixture_dir: &Path) -> Check<TempDir> {
    let seeded = TempDir::new()?;
    copy_fixture_tree(fixture_dir, seeded.path())?;
    git(seeded.path(), &["init", "--quiet"])?;
    git(seeded.path(), &["add", "-A"])?;
    git(
        seeded.path(),
        &[
            "-c",
            "user.email=persistence@example.invalid",
            "-c",
            "user.name=Chakra Persistence Benchmark",
            "commit",
            "--quiet",
            "-m",
            "persistence fixture seed",
        ],
    )?;
    Ok(seeded)
}

/// Asserts the checkout is clean after a refresh edit (the fetch tool's
/// `.chakra-corpus.json` metadata excepted).
pub(crate) fn verify_clean_checkout(checkout: &Path) -> Check<()> {
    git(checkout, &["checkout", "--", "."])?;
    let status = git(checkout, &["status", "--porcelain"])?;
    let dirty: Vec<&str> = status
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|line| line.trim_start_matches("?? ").trim() != CACHE_METADATA_FILE)
        .collect();
    if !dirty.is_empty() {
        return Err(failure(format!(
            "worktree is dirty after the refresh edit: {dirty:?}"
        ))
        .into());
    }
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Check<String> {
    let output = Command::new("git").current_dir(root).args(args).output()?;
    if !output.status.success() {
        return Err(failure(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::workspace_root;
    use crate::persistence::TargetStatus;

    /// A tiny Rust repository (three files, one high-degree callee) as a
    /// local "corpus" checkout inside a temporary cache root.
    fn tiny_checkout() -> Check<(TempDir, PersistenceTarget)> {
        let cache = TempDir::new()?;
        let checkout = cache.path().join("tiny__tiny");
        fs::create_dir_all(checkout.join("src"))?;
        fs::write(
            checkout.join("src/lib.rs"),
            "pub fn hot() {}\npub fn alpha() { hot(); }\n",
        )?;
        fs::write(
            checkout.join("src/more.rs"),
            "pub fn beta() { hot(); }\npub fn gamma() { hot(); }\n",
        )?;
        fs::write(checkout.join("src/extra.rs"), "pub fn delta() { hot(); }\n")?;
        git(&checkout, &["init", "--quiet"])?;
        git(&checkout, &["add", "-A"])?;
        git(
            &checkout,
            &[
                "-c",
                "user.email=persistence@example.invalid",
                "-c",
                "user.name=Chakra Persistence Benchmark",
                "commit",
                "--quiet",
                "-m",
                "tiny persistence target",
            ],
        )?;
        let sha = git(&checkout, &["rev-parse", "HEAD"])?;
        let target = PersistenceTarget {
            name: "corpus/rust/tiny/tiny".to_owned(),
            kind: TargetKind::Corpus,
            language: "rust".to_owned(),
            fixture_dir: None,
            checkout: Some(checkout),
            sha,
        };
        Ok((cache, target))
    }

    #[test]
    fn fixture_targets_cover_the_committed_fixtures() -> Check<()> {
        let targets = fixture_targets(&workspace_root())?;
        let names: Vec<&str> = targets.iter().map(|target| target.name.as_str()).collect();
        assert!(names.contains(&"fixture/rust/controller-service-provider"));
        assert!(names.contains(&"fixture/php/controller-service-provider"));
        assert!(targets.iter().all(|target| target.fixture_dir.is_some()));
        Ok(())
    }

    #[test]
    fn measures_a_tiny_rust_repository_end_to_end() -> Check<()> {
        let (_cache, target) = tiny_checkout()?;
        let spool = TempDir::new()?;
        let report = evaluate_target(&target, 2, spool.path())?;
        assert_eq!(report.status, TargetStatus::Measured);
        assert_eq!(report.runs.len(), 2);
        let corpus = report.corpus.as_ref().ok_or("corpus context missing")?;
        assert!(corpus.files >= 3);
        for run in &report.runs {
            assert!(run.cold_rebuild.symbols >= 5);
            assert!(run.cache_write.fact_files >= 3);
            assert!(run.warm_restore.compatible);
            assert_eq!(run.warm_restore.hit_ratio_per_mille, 1_000);
            assert_eq!(run.validation_only.hit_ratio_per_mille, 1_000);
            assert!(
                run.warm_restore.phase.bytes_read >= run.cache_write.phase.bytes_written,
                "restore reads the facts plus the validated sources"
            );
            let refresh = &run.one_file_refresh;
            assert_eq!(refresh.restore.misses, 1, "the edited file must miss");
            assert_eq!(refresh.files_reparsed, 1, "exactly one file reparsed");
            assert!(refresh.total_wall_micros >= refresh.reconcile_wall_micros);
        }
        // Determinism: both runs see the same corpus fingerprint.
        let first = &report.runs[0];
        let second = &report.runs[1];
        assert_eq!(first.cold_rebuild.symbols, second.cold_rebuild.symbols);
        // The checkout survived the refresh edits clean.
        let checkout = target.checkout.as_ref().ok_or("checkout missing")?;
        assert_eq!(git(checkout, &["status", "--porcelain"])?, "");
        Ok(())
    }

    #[test]
    fn missing_corpus_checkout_is_a_graceful_skip() -> Check<()> {
        let cache = TempDir::new()?;
        let target = PersistenceTarget {
            name: "corpus/rust/absent/absent".to_owned(),
            kind: TargetKind::Corpus,
            language: "rust".to_owned(),
            fixture_dir: None,
            checkout: Some(cache.path().join("absent__absent")),
            sha: "0".repeat(40),
        };
        let spool = TempDir::new()?;
        let report = evaluate_target(&target, 1, spool.path())?;
        assert_eq!(report.status, TargetStatus::Skipped);
        assert!(report.skip_reason.contains("not cached"));
        Ok(())
    }

    #[test]
    fn sha_mismatch_is_refused() -> Check<()> {
        let (_cache, mut target) = tiny_checkout()?;
        target.sha = "0".repeat(40);
        let spool = TempDir::new()?;
        let report = evaluate_target(&target, 1, spool.path())?;
        assert_eq!(report.status, TargetStatus::Skipped);
        assert!(report.skip_reason.contains("does not match pinned SHA"));
        Ok(())
    }

    #[test]
    fn unsupported_language_is_a_graceful_skip() -> Check<()> {
        let (_cache, mut target) = tiny_checkout()?;
        target.language = "cobol".to_owned();
        let spool = TempDir::new()?;
        let report = evaluate_target(&target, 1, spool.path())?;
        assert_eq!(report.status, TargetStatus::Skipped);
        assert!(report.skip_reason.contains("unsupported language"));
        Ok(())
    }

    #[test]
    fn zero_runs_is_rejected() -> Check<()> {
        let (_cache, target) = tiny_checkout()?;
        let spool = TempDir::new()?;
        assert!(evaluate_target(&target, 0, spool.path()).is_err());
        Ok(())
    }

    #[test]
    fn fixture_evaluation_seeds_a_temporary_repository() -> Check<()> {
        let root = workspace_root();
        let targets = fixture_targets(&root)?;
        let target = targets
            .iter()
            .find(|target| target.name == "fixture/rust/controller-service-provider")
            .ok_or("rust fixture target missing")?;
        let spool = TempDir::new()?;
        let report = evaluate_target(target, 1, spool.path())?;
        assert_eq!(report.status, TargetStatus::Measured);
        assert_eq!(report.sha.len(), 40);
        assert_eq!(report.runs.len(), 1);
        assert_eq!(report.runs[0].warm_restore.hit_ratio_per_mille, 1_000);
        Ok(())
    }
}
