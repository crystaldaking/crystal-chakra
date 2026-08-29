//! Real syntax fact cache benchmark (issue #39): budgets B1–B6 measured
//! against the production cache in `chakra-language::cache`, including real
//! graph reassembly, on the same targets as the issue #38 model.
//!
//! Per run the pipeline measures:
//!
//! 1. **cold rebuild** — `index_repository_with_options` with the cache
//!    disabled (the deterministic baseline);
//! 2. **cache write** — the same call with the cache enabled on an empty
//!    directory (build plus atomic publication);
//! 3. **warm restore** — the same call on the populated cache: key check,
//!    content-hash validation, fact reads, reassembly through the bounded
//!    build pipeline, merge, audit, and atomic publication. The restored
//!    graph's fingerprint must equal the cold rebuild's;
//! 4. **validation only** — scan, key resolution, manifest read, and
//!    content hashing, without fact reads or reassembly (budget B2);
//! 5. **one-file refresh** — one appended declaration, then the cache-enabled
//!    call reparses exactly the edited file inside the restore path (budget
//!    B3), verified against a deterministic rebuild of the edited tree;
//! 6. **restore-only child** — a separate process restores the cache and
//!    reports its indexer peak RSS (budget B6).
//!
//! The refresh edit is always reverted and the checkout verified clean.
//! Nothing here touches the network.

use std::collections::HashMap;
use std::fs;
use std::hash::Hasher;
use std::path::Path;
use std::process::Command;

use chakra_domain::indexing::IndexBudgets;
use chakra_language::cache::{
    CacheRestoreOutcome, CacheStore, CompatibilityKey, SyntaxCacheConfig, SyntaxCacheMode,
    content_hash,
};
use chakra_language::{
    IndexOptions, default_adapters, index_repository_with_options,
    scan_repository_sources_with_options,
};
use serde::{Deserialize, Serialize};

use super::model::PhaseTimer;
use super::report::{ColdRebuildReport, MachineContext, PhaseReport, TargetKind, TargetStatus};
use super::runner::{
    PersistenceTarget, TargetCheckout, probe_declaration, resolve_target_checkout,
    verify_clean_checkout,
};
use crate::{Check, ensure, failure};

/// Schema identity and version of the real-cache artifacts.
pub const REAL_SCHEMA: &str = "chakra.persistence-real";
pub const REAL_SCHEMA_VERSION: u32 = 1;

/// Budget B1 gate: persistence is only justified above 1,000 indexed files.
const B1_GATE_FILES: u64 = 1_000;
/// Budget B1: restore (including reassembly) must be at least this much
/// faster than the cold rebuild.
const B1_MIN_SPEEDUP: f64 = 5.0;
/// Budget B2: validation may cost at most 5% of the cold rebuild.
const B2_MAX_FRACTION: f64 = 0.05;
/// Budget B3: one-file refresh may cost at most 25% of the cold rebuild.
const B3_MAX_FRACTION: f64 = 0.25;
/// Budget B4: persisted facts must fit within 2x retained source bytes.
const B4_MAX_RATIO: f64 = 2.0;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealCacheWriteReport {
    #[serde(flatten)]
    pub phase: PhaseReport,
    pub published: bool,
    pub entries: u64,
    pub skipped_entries: u64,
    pub bytes_written: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealRestoreReport {
    #[serde(flatten)]
    pub phase: PhaseReport,
    pub hits: u64,
    pub misses: u64,
    pub cache_bytes_read: u64,
    /// Restored graph fingerprint equals the cold rebuild fingerprint.
    pub fingerprint_match: bool,
    /// Cold rebuild wall / restore wall, in per-mille.
    pub speedup_per_mille: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealValidationReport {
    #[serde(flatten)]
    pub phase: PhaseReport,
    /// Source scan wall (context: shared by the cold and restore paths).
    pub scan_wall_micros: u64,
    pub compatible: bool,
    pub hits: u64,
    pub misses: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealRefreshReport {
    pub edited_file: String,
    #[serde(flatten)]
    pub phase: PhaseReport,
    pub misses: u64,
    /// Refreshed graph fingerprint equals a deterministic rebuild of the
    /// edited tree.
    pub fingerprint_match: bool,
}

/// Per-budget PASS/FAIL; `None` means not applicable to this target
/// (below-gate repository for B1/B3, or no restore-only sample for B6).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetOutcomes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b1_restore_gate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b2_validation_overhead: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b3_refresh_budget: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b4_bytes_budget: Option<bool>,
    pub b5_correctness: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b6_memory: Option<bool>,
}

impl BudgetOutcomes {
    pub fn all_pass(&self) -> bool {
        self.b1_restore_gate.unwrap_or(true)
            && self.b2_validation_overhead.unwrap_or(true)
            && self.b3_refresh_budget.unwrap_or(true)
            && self.b4_bytes_budget.unwrap_or(true)
            && self.b5_correctness
            && self.b6_memory.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealRunReport {
    pub run: u32,
    pub source_bytes: u64,
    /// `true` when the cache stayed off because the target is below the B1
    /// gate (the production default for small repositories).
    pub below_gate: bool,
    pub cold_rebuild: ColdRebuildReport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<RealCacheWriteReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_restore: Option<RealRestoreReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_only: Option<RealValidationReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_file_refresh: Option<RealRefreshReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_only_peak_rss_bytes: Option<u64>,
    pub budgets: BudgetOutcomes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealPersistenceReport {
    pub schema: String,
    pub schema_version: u32,
    pub target: String,
    pub kind: TargetKind,
    pub status: TargetStatus,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub skip_reason: String,
    pub sha: String,
    pub machine: MachineContext,
    pub indexed_files: u64,
    pub source_bytes: u64,
    /// Final-run budget outcomes; see the evaluation document.
    pub budgets: BudgetOutcomes,
    pub runs: Vec<RealRunReport>,
}

impl RealPersistenceReport {
    pub fn file_name(target: &str) -> String {
        format!("persistence-real-{}.json", target.replace('/', "__"))
    }

    pub fn render(&self) -> Check<String> {
        Ok(format!("{}\n", serde_json::to_string_pretty(self)?))
    }
}

fn cache_options(cache_dir: &Path) -> IndexOptions {
    // Production gate semantics: the default 1,000-file minimum. Below it
    // the run records `below_gate` instead of restore phases.
    IndexOptions {
        cache: SyntaxCacheMode::Enabled(SyntaxCacheConfig::new(cache_dir.to_path_buf())),
        ..IndexOptions::default()
    }
}

/// Evaluates one target against the real cache and returns its artifact.
pub fn evaluate_real_target(
    target: &PersistenceTarget,
    runs: u32,
    spool_dir: &Path,
    executable: &Path,
) -> Check<RealPersistenceReport> {
    if runs == 0 {
        return Err(failure("persistence benchmark needs at least one run").into());
    }
    let skipped = |reason: String| RealPersistenceReport {
        schema: REAL_SCHEMA.to_owned(),
        schema_version: REAL_SCHEMA_VERSION,
        target: target.name.clone(),
        kind: target.kind,
        status: TargetStatus::Skipped,
        skip_reason: reason,
        sha: target.sha.clone(),
        machine: MachineContext::current(),
        indexed_files: 0,
        source_bytes: 0,
        budgets: BudgetOutcomes::default(),
        runs: Vec::new(),
    };
    if probe_declaration(&target.language).is_none() {
        return Ok(skipped(format!(
            "unsupported language: no persistence probe for `{}`",
            target.language
        )));
    }
    let (checkout, sha, fixture) = match resolve_target_checkout(target)? {
        TargetCheckout::Resolved {
            checkout,
            sha,
            fixture,
        } => (checkout, sha, fixture),
        TargetCheckout::Skipped(reason) => return Ok(skipped(reason)),
    };

    let mut reports = Vec::new();
    let mut indexed_files = 0_u64;
    let mut source_bytes = 0_u64;
    for run in 1..=runs {
        let cache_dir = spool_dir.join(format!("real-{}-run{run}", target.name.replace('/', "__")));
        if cache_dir.exists() {
            fs::remove_dir_all(&cache_dir)?;
        }
        let report = evaluate_real_run(run, target, &checkout, &cache_dir, executable)?;
        indexed_files = report.cold_rebuild.parsed_files;
        source_bytes = report.source_bytes;
        reports.push(report);
        if cache_dir.exists() {
            fs::remove_dir_all(&cache_dir)?;
        }
    }
    drop(fixture);
    let budgets = reports
        .last()
        .map(|report| report.budgets.clone())
        .unwrap_or_default();
    Ok(RealPersistenceReport {
        schema: REAL_SCHEMA.to_owned(),
        schema_version: REAL_SCHEMA_VERSION,
        target: target.name.clone(),
        kind: target.kind,
        status: TargetStatus::Measured,
        skip_reason: String::new(),
        sha,
        machine: MachineContext::current(),
        indexed_files,
        source_bytes,
        budgets,
        runs: reports,
    })
}

/// One full measurement run against `checkout`.
fn evaluate_real_run(
    run: u32,
    target: &PersistenceTarget,
    checkout: &Path,
    cache_dir: &Path,
    executable: &Path,
) -> Check<RealRunReport> {
    // --- cold rebuild (deterministic baseline) ----------------------------
    let timer = PhaseTimer::start();
    let cold = index_repository_with_options(checkout, IndexOptions::default())?;
    let cold_phase = timer.finish();
    let cold_fingerprint = graph_fingerprint(&cold.graph);
    let cold_wall_micros = cold_phase.wall_micros;
    let source_bytes = cold.metrics.indexing.coverage.source_bytes;
    let indexed_files = cold.metrics.parsed_files;
    let cold_report = ColdRebuildReport {
        phase: PhaseReport::from_measurement(&cold_phase),
        parsed_files: cold.metrics.parsed_files,
        symbols: cold.metrics.symbols,
        edges: cold.metrics.edges,
        call_sites: cold.metrics.call_sites,
        degraded: cold.metrics.indexing.is_degraded(),
        indexer_phase_peak_rss_bytes: cold.metrics.indexing.memory.observed_phase_peak_rss_bytes,
    };

    let below_gate = indexed_files <= B1_GATE_FILES;
    let mut budgets = BudgetOutcomes::default();

    // --- cache write -------------------------------------------------------
    let timer = PhaseTimer::start();
    let written = index_repository_with_options(checkout, cache_options(cache_dir))?;
    let write_phase = timer.finish();
    let write_outcome = written.cache.write.clone();
    let cache_write = match &write_outcome {
        Some(outcome) => {
            budgets.b4_bytes_budget =
                Some((outcome.total_bytes as f64) <= B4_MAX_RATIO * source_bytes as f64);
            Some(RealCacheWriteReport {
                phase: PhaseReport::from_measurement(&write_phase),
                published: outcome.published,
                entries: outcome.entries,
                skipped_entries: outcome.skipped_entries,
                bytes_written: outcome.bytes_written,
                total_bytes: outcome.total_bytes,
            })
        }
        None => None,
    };

    // --- warm restore ------------------------------------------------------
    let timer = PhaseTimer::start();
    let restored = index_repository_with_options(checkout, cache_options(cache_dir))?;
    let restore_phase = timer.finish();
    let warm_restore = match restored.cache.restore {
        CacheRestoreOutcome::Restored { hits, misses } => {
            let fingerprint_match = graph_fingerprint(&restored.graph) == cold_fingerprint;
            let speedup_per_mille = speedup_per_mille(cold_wall_micros, restore_phase.wall_micros);
            if !below_gate {
                budgets.b1_restore_gate = Some(
                    (restore_phase.wall_micros as f64) * B1_MIN_SPEEDUP <= cold_wall_micros as f64,
                );
            }
            budgets.b5_correctness = misses == 0
                && hits == indexed_files
                && fingerprint_match
                && restored.cache.write.is_none();
            Some(RealRestoreReport {
                phase: PhaseReport::from_measurement(&restore_phase),
                hits,
                misses,
                cache_bytes_read: restored.cache.bytes_read,
                fingerprint_match,
                speedup_per_mille,
            })
        }
        CacheRestoreOutcome::BelowGate { .. } => {
            budgets.b5_correctness = below_gate;
            None
        }
        CacheRestoreOutcome::Fallback { ref reason } if below_gate => {
            return Err(failure(format!(
                "below-gate target unexpectedly attempted a cache restore: {reason}"
            ))
            .into());
        }
        CacheRestoreOutcome::Fallback { ref reason } => {
            return Err(failure(format!("warm restore fell back to a rebuild: {reason}")).into());
        }
        CacheRestoreOutcome::Disabled => {
            return Err(failure("cache unexpectedly disabled during measurement").into());
        }
    };

    // --- validation only ---------------------------------------------------
    let validation = if below_gate {
        None
    } else {
        let validation = measure_validation(checkout, cache_dir)?;
        budgets.b2_validation_overhead = Some(
            (validation.phase.wall_micros as f64) <= B2_MAX_FRACTION * cold_wall_micros as f64,
        );
        Some(validation)
    };

    // --- one-file refresh --------------------------------------------------
    let one_file_refresh = if below_gate {
        None
    } else {
        let refresh = measure_real_refresh(target, checkout, cache_dir, &restored)?;
        budgets.b3_refresh_budget = Some(
            (refresh.phase.wall_micros as f64) <= B3_MAX_FRACTION * cold_wall_micros as f64
                && refresh.misses == 1
                && refresh.fingerprint_match,
        );
        budgets.b5_correctness =
            budgets.b5_correctness && refresh.misses == 1 && refresh.fingerprint_match;
        Some(refresh)
    };

    // --- restore-only child (B6) -------------------------------------------
    let restore_only_peak_rss_bytes = if below_gate {
        None
    } else {
        let peak = measure_restore_only(target, checkout, cache_dir, executable)?;
        budgets.b6_memory = match (peak, cold_report.indexer_phase_peak_rss_bytes) {
            (Some(peak), Some(cold_peak)) => Some(peak < cold_peak),
            _ => None,
        };
        peak
    };

    Ok(RealRunReport {
        run,
        source_bytes,
        below_gate,
        cold_rebuild: cold_report,
        cache_write,
        warm_restore,
        validation_only: validation,
        one_file_refresh,
        restore_only_peak_rss_bytes,
        budgets,
    })
}

fn speedup_per_mille(baseline_micros: u64, measured_micros: u64) -> u64 {
    if measured_micros == 0 {
        return u64::MAX;
    }
    baseline_micros.saturating_mul(1_000) / measured_micros.max(1)
}

/// B2: the marginal cost of cache validation on an unchanged worktree —
/// key resolution, manifest read, and content hashing. The source scan is
/// timed separately for context: cold rebuild and restore both pay it, so
/// the budget applies to the cache-specific validation work only.
fn measure_validation(checkout: &Path, cache_dir: &Path) -> Check<RealValidationReport> {
    let scan_timer = PhaseTimer::start();
    let scan = scan_repository_sources_with_options(checkout, &IndexOptions::default())?;
    let scan_phase = scan_timer.finish();
    let timer = PhaseTimer::start();
    let extractors = default_adapters()
        .iter()
        .map(|adapter| (adapter.language(), adapter.extractor_version()))
        .collect();
    let key = CompatibilityKey::resolve(checkout, &IndexBudgets::default(), extractors)
        .map_err(|error| failure(format!("cache key resolution failed: {error}")))?;
    let store = CacheStore::new(SyntaxCacheConfig::new(cache_dir.to_path_buf()));
    let (manifest, _) = store.read_compatible_manifest(&key);
    let Some((entries, _)) = manifest else {
        return Ok(RealValidationReport {
            phase: PhaseReport::from_measurement(&timer.finish()),
            scan_wall_micros: scan_phase.wall_micros,
            compatible: false,
            hits: 0,
            misses: 0,
        });
    };
    let entries: HashMap<_, _> = entries.iter().map(|entry| (&entry.path, entry)).collect();
    let mut hits = 0_u64;
    let mut misses = 0_u64;
    for language in scan.sources.languages() {
        for (path, source) in &language.sources.files {
            match entries.get(path) {
                Some(entry) if entry.content_hash == content_hash(source) => hits += 1,
                _ => misses += 1,
            }
        }
    }
    Ok(RealValidationReport {
        phase: PhaseReport::from_measurement(&timer.finish()),
        scan_wall_micros: scan_phase.wall_micros,
        compatible: true,
        hits,
        misses,
    })
}

/// B3: append one probe declaration and run the cache-enabled index — the
/// restore path validates, restores the hits, and reparses exactly the
/// edited file. The edit is always reverted.
fn measure_real_refresh(
    target: &PersistenceTarget,
    checkout: &Path,
    cache_dir: &Path,
    restored: &chakra_language::IndexReport,
) -> Check<RealRefreshReport> {
    let (extensions, declaration) = probe_declaration(&target.language)
        .ok_or_else(|| failure(format!("no probe for `{}`", target.language)))?;
    let mut probe_file = None;
    for path in restored.syntax_index.paths() {
        if !extensions
            .iter()
            .any(|extension| path.as_str().ends_with(&format!(".{extension}")))
        {
            continue;
        }
        if target.language == "php" {
            let content = restored.graph.file_source(&path).unwrap_or_default();
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

    let refresh = (|| -> Check<RealRefreshReport> {
        let mut edited = original.clone();
        edited.extend_from_slice(declaration.as_bytes());
        fs::write(&absolute, edited)?;

        let timer = PhaseTimer::start();
        let refreshed = index_repository_with_options(checkout, cache_options(cache_dir))?;
        let phase = timer.finish();
        let misses = match refreshed.cache.restore {
            CacheRestoreOutcome::Restored { misses, .. } => misses,
            ref other => {
                return Err(failure(format!("refresh did not restore: {other:?}")).into());
            }
        };
        let cold = index_repository_with_options(checkout, IndexOptions::default())?;
        let fingerprint_match =
            graph_fingerprint(&refreshed.graph) == graph_fingerprint(&cold.graph);
        Ok(RealRefreshReport {
            edited_file: probe_file.clone(),
            phase: PhaseReport::from_measurement(&phase),
            misses,
            fingerprint_match,
        })
    })();

    fs::write(&absolute, &original)?;
    verify_clean_checkout(checkout)?;
    refresh
}

/// B6: a separate process restores the populated cache and reports its
/// indexer peak RSS, so the restore peak is measured without the cold
/// rebuild inflating the process high-water mark.
fn measure_restore_only(
    _target: &PersistenceTarget,
    checkout: &Path,
    cache_dir: &Path,
    executable: &Path,
) -> Check<Option<u64>> {
    let output = Command::new(executable)
        .arg("persistence")
        .arg("--real-restore-child")
        .arg("--checkout")
        .arg(checkout)
        .arg("--cache-dir")
        .arg(cache_dir)
        .output()?;
    ensure(
        output.status.success(),
        format!(
            "restore-only child failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(value) = line.strip_prefix("CHAKRA_RESTORE_CHILD peak_rss_bytes=") {
            let peak = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok());
            return Ok(peak);
        }
    }
    Ok(None)
}

/// Hidden `--real-restore-child` entry point: restore the cache in this
/// process and print the indexer peak RSS. Used for budget B6 isolation.
pub fn restore_only_child(checkout: &Path, cache_dir: &Path) -> Check<()> {
    let report = index_repository_with_options(checkout, cache_options(cache_dir))?;
    let hits_misses = match report.cache.restore {
        CacheRestoreOutcome::Restored { hits, misses } => (hits, misses),
        ref other => {
            return Err(failure(format!("restore-only child did not restore: {other:?}")).into());
        }
    };
    let peak = report
        .metrics
        .indexing
        .memory
        .observed_phase_peak_rss_bytes
        .unwrap_or(0);
    println!(
        "CHAKRA_RESTORE_CHILD peak_rss_bytes={peak} hits={} misses={}",
        hits_misses.0, hits_misses.1
    );
    Ok(())
}

/// One-line human summary of a finished real-cache report.
pub fn summarize_real(report: &RealPersistenceReport) -> String {
    if report.status == TargetStatus::Skipped {
        return format!("{}: skipped ({})", report.target, report.skip_reason);
    }
    let budgets = &report.budgets;
    let verdict = if budgets.all_pass() { "PASS" } else { "FAIL" };
    let summaries: Vec<String> = report
        .runs
        .iter()
        .map(|run| {
            let cold = run.cold_rebuild.phase.wall_micros as f64 / 1e6;
            match &run.warm_restore {
                Some(restore) => {
                    let restore_wall = restore.phase.wall_micros as f64 / 1e6;
                    format!(
                        "run {}: cold {cold:.2}s restore {restore_wall:.2}s ({:.1}x) hits {} misses {}",
                        run.run,
                        restore.speedup_per_mille as f64 / 1_000.0,
                        restore.hits,
                        restore.misses,
                    )
                }
                None => format!("run {}: cold {cold:.2}s below-gate", run.run),
            }
        })
        .collect();
    let mark = |budget: Option<bool>| match budget {
        Some(true) => "PASS",
        Some(false) => "FAIL",
        None => "n/a",
    };
    format!(
        "{}: {} — B1 {} B2 {} B3 {} B4 {} B5 {} B6 {} — {}",
        report.target,
        verdict,
        mark(budgets.b1_restore_gate),
        mark(budgets.b2_validation_overhead),
        mark(budgets.b3_refresh_budget),
        mark(budgets.b4_bytes_budget),
        if budgets.b5_correctness {
            "PASS"
        } else {
            "FAIL"
        },
        mark(budgets.b6_memory),
        summaries.join("; "),
    )
}
