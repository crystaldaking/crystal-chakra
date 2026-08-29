//! Persistence acceptance benchmark (issue #38).
//!
//! Measures, per target repository, whether restoring persistent syntax facts
//! beats a deterministic rebuild by enough to justify cache complexity
//! (SPEC §14: "Cache existence must be justified by benchmarks"). The
//! synthetic per-file cache **model** lives in `model`; it serializes the
//! benchmark projection from `projection` but deliberately does **not**
//! implement graph restoration — restore timings therefore cover read,
//! compatibility validation, and deserialization, and the speedups recorded
//! in `docs/evaluation/v0.2.0-persistence-acceptance.md` are upper bounds.
//!
//! Like the corpus runner, this tool never fetches: missing or
//! SHA-mismatched checkouts are skipped repositories. Unlike the corpus
//! runner it writes its artifacts under `target/persistence/` (git-ignored);
//! nothing in `docs/support/corpus/results/` is touched.
//!
//! The `--real` mode (issue #39, [`real`]) measures the same phases against
//! the production per-file syntax fact cache in `chakra-language::cache`,
//! including real graph reassembly, and evaluates the B1–B6 budgets; see
//! `docs/evaluation/v0.2.0-syntax-fact-cache.md`.

mod model;
mod projection;
mod real;
mod report;
mod runner;

use std::path::PathBuf;

pub use model::{CompatibilityKey, PhaseMeasurement, PhaseTimer, RestoreOutcome};
pub use projection::{FileFacts, MODEL_FORMAT_VERSION};
pub use real::{RealPersistenceReport, evaluate_real_target, restore_only_child, summarize_real};
pub use report::{
    MachineContext, PERSISTENCE_SCHEMA_VERSION, PersistenceReport, TargetKind, TargetStatus,
};
pub use runner::{PersistenceTarget, corpus_targets, evaluate_target, fixture_targets};

/// Default artifact directory (git-ignored).
pub fn default_emit_dir() -> PathBuf {
    super::corpus::workspace_root().join("target/persistence")
}

/// Default spool directory for model-cache writes (git-ignored, real disk —
/// writing gigabyte-scale model caches to tmpfs would flatter the write and
/// restore timings).
pub fn default_spool_dir() -> PathBuf {
    super::corpus::workspace_root().join("target/tmp/persistence")
}

/// One-line human summary of a finished report.
pub fn summarize(report: &PersistenceReport) -> String {
    if report.status == TargetStatus::Skipped {
        return format!("{}: skipped ({})", report.target, report.skip_reason);
    }
    let summaries: Vec<String> = report
        .runs
        .iter()
        .map(|run| {
            let cold = run.cold_rebuild.phase.wall_micros as f64 / 1e6;
            let restore = run.warm_restore.phase.wall_micros as f64 / 1e6;
            let validate = run.validation_only.phase.wall_micros as f64 / 1e6;
            let refresh = run.one_file_refresh.total_wall_micros as f64 / 1e6;
            let speedup = if restore > 0.0 { cold / restore } else { 0.0 };
            format!(
                "run {}: cold {cold:.2}s restore {restore:.2}s ({speedup:.1}x) validate {validate:.2}s refresh {refresh:.3}s hit {}/1000",
                run.run, run.warm_restore.hit_ratio_per_mille
            )
        })
        .collect();
    format!("{}: measured — {}", report.target, summaries.join("; "))
}
