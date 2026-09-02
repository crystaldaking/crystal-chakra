//! Complete compatible commit-snapshot evaluation (issue #50).
//!
//! This harness measures the production codec/store from issue #49. It never
//! fetches a repository and never enables snapshot import in the product.

mod report;
mod runner;

use std::path::PathBuf;

pub use report::{GateReport, SHARED_INDEX_SCHEMA_VERSION, SharedIndexReport, TargetStatus};
pub use runner::evaluate_target;

pub fn default_emit_dir() -> PathBuf {
    super::corpus::workspace_root().join("target/shared-indexes")
}

pub fn default_spool_dir() -> PathBuf {
    super::corpus::workspace_root().join("target/tmp/shared-indexes")
}

pub fn summarize(report: &SharedIndexReport) -> String {
    if report.status == TargetStatus::Skipped {
        return format!("{}: skipped ({})", report.target, report.skip_reason);
    }
    let runs = report
        .runs
        .iter()
        .map(|run| {
            let cold = run.cold_rebuild.phase.wall_micros as f64 / 1e6;
            let local = run
                .local_restore
                .as_ref()
                .map(|restore| restore.phase.wall_micros as f64 / 1e6);
            let prebuilt = run
                .prebuilt_restore
                .as_ref()
                .map(|restore| restore.phase.wall_micros as f64 / 1e6);
            format!(
                "run {}: cold {cold:.3}s local {} prebuilt {} size {} approved local={} prebuilt={}",
                run.run,
                local.map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.3}s")),
                prebuilt
                    .map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.3}s")),
                run.cache_population.artifact_bytes.map_or_else(
                    || "unavailable".to_owned(),
                    |bytes| format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
                ),
                run.gates.approved_for_default_local_restore,
                run.gates.approved_for_prebuilt_import,
            )
        })
        .collect::<Vec<_>>();
    format!("{}: measured — {}", report.target, runs.join("; "))
}
