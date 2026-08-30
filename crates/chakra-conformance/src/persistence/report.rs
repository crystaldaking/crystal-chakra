//! Machine-readable persistence benchmark artifacts (issue #38).
//!
//! One `persistence-<slug>.json` per measured target. JSON structure is
//! deterministic (fixed field order, integer ratios); measured values vary by
//! machine and run, so artifacts are written under `target/` and are **not**
//! committed or diffed in CI. The evaluation document in
//! `docs/evaluation/v0.2.0-persistence-acceptance.md` interprets them.

use serde::{Deserialize, Serialize};

use chakra_domain::indexing::IndexBudgets;

use super::model::PhaseMeasurement;
use crate::Check;

/// Current persistence artifact schema version.
pub const PERSISTENCE_SCHEMA_VERSION: u32 = 1;

/// Where the numbers came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// A small in-repository fixture seeded into a temporary Git worktree.
    Fixture,
    /// A pinned public corpus checkout under `target/corpus`.
    Corpus,
}

/// Repository-level outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetStatus {
    Measured,
    Skipped,
}

/// Machine context recorded with every artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineContext {
    pub os: String,
    pub arch: String,
    pub logical_cpus: u64,
}

impl MachineContext {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            logical_cpus: std::thread::available_parallelism()
                .map(|value| value.get() as u64)
                .unwrap_or(1),
        }
    }
}

/// Corpus fingerprint: what the benchmark actually indexed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusFingerprint {
    pub files: u64,
    pub source_bytes: u64,
    /// Model hash over all `(path, content_hash)` pairs (the compatibility
    /// key's content fingerprint).
    pub content_fingerprint: String,
}

/// Index format/configuration context of the measurement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfigContext {
    pub model_format_version: u32,
    pub budgets: IndexBudgets,
    /// Fingerprint of the full compatibility key used by the model cache.
    pub compatibility_key: String,
}

/// Serializable form of one phase measurement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseReport {
    pub wall_micros: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_micros: Option<u64>,
    /// Process high-water RSS at phase end (monotonic within the process).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_peak_rss_bytes: Option<u64>,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

impl PhaseReport {
    pub fn from_measurement(measurement: &PhaseMeasurement) -> Self {
        Self {
            wall_micros: measurement.wall_micros,
            cpu_micros: measurement.cpu_micros,
            end_peak_rss_bytes: measurement.end_peak_rss_bytes,
            bytes_read: measurement.bytes_read,
            bytes_written: measurement.bytes_written,
        }
    }
}

/// Cold rebuild: full deterministic syntax index from scratch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdRebuildReport {
    #[serde(flatten)]
    pub phase: PhaseReport,
    pub parsed_files: u64,
    pub symbols: u64,
    pub edges: u64,
    pub call_sites: u64,
    pub degraded: bool,
    /// Indexer phase-boundary RSS sampler value (not an OS peak claim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexer_phase_peak_rss_bytes: Option<u64>,
}

/// Cache write: projection build plus serialization of the model cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheWriteReport {
    #[serde(flatten)]
    pub phase: PhaseReport,
    /// Projection build (graph traversal) is measured separately from the
    /// serialization + I/O in `phase`.
    pub projection_wall_micros: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_cpu_micros: Option<u64>,
    pub fact_files: u64,
    pub declarations: u64,
    pub relationships: u64,
    pub call_candidates: u64,
    pub omitted_facts: u64,
}

/// Warm restore or validation-only pass over the model cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePhaseReport {
    #[serde(flatten)]
    pub phase: PhaseReport,
    /// `false` on compatibility-key mismatch (deterministic-rebuild fallback).
    pub compatible: bool,
    pub hits: u64,
    pub misses: u64,
    pub hit_ratio_per_mille: u64,
}

/// One-file refresh: edit one file, restore the hits, reparse the miss via
/// the live reconciliation path, then restore the worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OneFileRefreshReport {
    pub edited_file: String,
    /// Restore pass after the edit (hits are deserialized; the miss is not).
    pub restore: RestorePhaseReport,
    pub reconcile_wall_micros: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconcile_cpu_micros: Option<u64>,
    pub scanned_files: u64,
    pub files_reparsed: u64,
    pub framework_files_reparsed: u64,
    /// `restore.wall + reconcile.wall`: the model's refresh cost.
    pub total_wall_micros: u64,
}

/// All phases of one measurement run. Repeated runs estimate the spread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub run: u32,
    pub cold_rebuild: ColdRebuildReport,
    pub cache_write: CacheWriteReport,
    pub warm_restore: RestorePhaseReport,
    pub validation_only: RestorePhaseReport,
    pub one_file_refresh: OneFileRefreshReport,
}

/// One emitted `persistence-<slug>.json` artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceReport {
    pub schema_version: u32,
    pub target: String,
    pub kind: TargetKind,
    pub status: TargetStatus,
    /// Empty unless `status` is `skipped`.
    pub skip_reason: String,
    /// Pinned corpus SHA, or the temporary fixture seed commit.
    pub sha: String,
    pub machine: MachineContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus: Option<CorpusFingerprint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_config: Option<IndexConfigContext>,
    pub runs: Vec<RunReport>,
}

impl PersistenceReport {
    pub fn measured(
        target: &str,
        kind: TargetKind,
        sha: &str,
        corpus: CorpusFingerprint,
        index_config: IndexConfigContext,
        runs: Vec<RunReport>,
    ) -> Self {
        Self {
            schema_version: PERSISTENCE_SCHEMA_VERSION,
            target: target.to_owned(),
            kind,
            status: TargetStatus::Measured,
            skip_reason: String::new(),
            sha: sha.to_owned(),
            machine: MachineContext::current(),
            corpus: Some(corpus),
            index_config: Some(index_config),
            runs,
        }
    }

    pub fn skipped(target: &str, kind: TargetKind, sha: &str, reason: String) -> Self {
        Self {
            schema_version: PERSISTENCE_SCHEMA_VERSION,
            target: target.to_owned(),
            kind,
            status: TargetStatus::Skipped,
            skip_reason: reason,
            sha: sha.to_owned(),
            machine: MachineContext::current(),
            corpus: None,
            index_config: None,
            runs: Vec::new(),
        }
    }

    /// Artifact file name for this target (`persistence-<slug>.json`).
    pub fn file_name(target: &str) -> String {
        format!("persistence-{}.json", target.replace('/', "__"))
    }

    /// Deterministic JSON rendering (pretty, trailing newline).
    pub fn render(&self) -> Check<String> {
        Ok(format!("{}\n", serde_json::to_string_pretty(self)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_report_renders_deterministically() -> Check<()> {
        let report = PersistenceReport::skipped(
            "corpus/rust/tokio-rs/tokio",
            TargetKind::Corpus,
            "625954f365727668cb02d04172b34f1149637728",
            "checkout not cached".to_owned(),
        );
        assert_eq!(report.render()?, report.render()?);
        assert_eq!(report.status, TargetStatus::Skipped);
        assert_eq!(
            PersistenceReport::file_name(&report.target),
            "persistence-corpus__rust__tokio-rs__tokio.json"
        );
        let parsed: PersistenceReport = serde_json::from_str(&report.render()?)?;
        assert_eq!(parsed.schema_version, PERSISTENCE_SCHEMA_VERSION);
        Ok(())
    }
}
