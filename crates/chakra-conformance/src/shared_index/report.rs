//! Machine-readable complete commit-snapshot benchmark reports (issue #50).

use chakra_domain::indexing::IndexBudgets;
use chakra_language::CommitSnapshotCompatibility;
use serde::{Deserialize, Serialize};

use crate::Check;
use crate::persistence::{MachineContext, TargetKind};

pub const SHARED_INDEX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetStatus {
    Measured,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseReport {
    pub wall_micros: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_peak_rss_bytes: Option<u64>,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

impl From<crate::persistence::PhaseMeasurement> for PhaseReport {
    fn from(measurement: crate::persistence::PhaseMeasurement) -> Self {
        Self {
            wall_micros: measurement.wall_micros,
            cpu_micros: measurement.cpu_micros,
            end_peak_rss_bytes: measurement.end_peak_rss_bytes,
            bytes_read: measurement.bytes_read,
            bytes_written: measurement.bytes_written,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSummary {
    pub files: u64,
    pub source_files: u64,
    pub source_bytes: u64,
    pub symbols: u64,
    pub edges: u64,
    pub call_sites: u64,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdRebuildReport {
    #[serde(flatten)]
    pub phase: PhaseReport,
    pub graph: GraphSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexer_phase_peak_rss_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachePopulationReport {
    #[serde(flatten)]
    pub phase: PhaseReport,
    pub artifact_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Lookup miss/rejection, replaced by the store rejection if population
    /// itself fails.
    pub lookup_or_store_rejection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_unavailable_detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreReport {
    #[serde(flatten)]
    pub phase: PhaseReport,
    pub origin: String,
    pub graph: GraphSummary,
    pub graph_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportReport {
    #[serde(flatten)]
    pub phase: PhaseReport,
    pub artifact_bytes: u64,
    pub artifact_blake3: String,
    pub digest_verified: bool,
}

/// Provenance recorded by the benchmark. Integrity is verified locally;
/// authenticity must come from an authenticated CI artifact channel and is
/// deliberately not inferred from BLAKE3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrebuiltProvenance {
    pub producer: String,
    pub trust_boundary: String,
    pub fact_scope: String,
    pub provider_enrichment_included: bool,
    pub repository: String,
    pub commit: String,
    pub compatibility: CommitSnapshotCompatibility,
    pub artifact_blake3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport {
    pub size_gate_files: u64,
    pub eligible_for_default_restore: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_to_source_per_mille: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_speedup_per_mille: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prebuilt_speedup_per_mille: Option<u64>,
    pub local_exact_graph_match: bool,
    pub prebuilt_exact_graph_match: bool,
    pub exact_graph_match: bool,
    pub local_restore_gate_passed: bool,
    pub prebuilt_restore_gate_passed: bool,
    pub size_gate_passed: bool,
    pub approved_for_default_local_restore: bool,
    pub approved_for_prebuilt_import: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub run: u32,
    pub cold_rebuild: ColdRebuildReport,
    pub cache_population: CachePopulationReport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_restore: Option<RestoreReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prebuilt_transport: Option<TransportReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prebuilt_restore: Option<RestoreReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prebuilt_provenance: Option<PrebuiltProvenance>,
    pub gates: GateReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedIndexReport {
    pub schema_version: u32,
    pub target: String,
    pub kind: TargetKind,
    pub status: TargetStatus,
    pub skip_reason: String,
    pub sha: String,
    pub machine: MachineContext,
    pub budgets: IndexBudgets,
    pub runs: Vec<RunReport>,
}

impl SharedIndexReport {
    pub fn measured(
        target: &str,
        kind: TargetKind,
        sha: &str,
        budgets: IndexBudgets,
        runs: Vec<RunReport>,
    ) -> Self {
        Self {
            schema_version: SHARED_INDEX_SCHEMA_VERSION,
            target: target.to_owned(),
            kind,
            status: TargetStatus::Measured,
            skip_reason: String::new(),
            sha: sha.to_owned(),
            machine: MachineContext::current(),
            budgets,
            runs,
        }
    }

    pub fn skipped(target: &str, kind: TargetKind, sha: &str, reason: String) -> Self {
        Self {
            schema_version: SHARED_INDEX_SCHEMA_VERSION,
            target: target.to_owned(),
            kind,
            status: TargetStatus::Skipped,
            skip_reason: reason,
            sha: sha.to_owned(),
            machine: MachineContext::current(),
            budgets: IndexBudgets::default(),
            runs: Vec::new(),
        }
    }

    pub fn file_name(target: &str) -> String {
        format!("shared-index-{}.json", target.replace('/', "__"))
    }

    pub fn render(&self) -> Check<String> {
        Ok(format!("{}\n", serde_json::to_string_pretty(self)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_report_roundtrips() -> Check<()> {
        let report = SharedIndexReport::skipped(
            "corpus/rust/absent/absent",
            TargetKind::Corpus,
            &"0".repeat(40),
            "checkout not cached".to_owned(),
        );
        assert_eq!(report.render()?, report.render()?);
        let decoded: SharedIndexReport = serde_json::from_str(&report.render()?)?;
        assert_eq!(decoded.schema_version, SHARED_INDEX_SCHEMA_VERSION);
        assert_eq!(
            SharedIndexReport::file_name(&report.target),
            "shared-index-corpus__rust__absent__absent.json"
        );
        Ok(())
    }
}
