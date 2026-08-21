//! Machine-readable corpus evaluation artifacts.
//!
//! `chakra-conformance corpus --emit <dir>` writes one
//! `<language>-<owner>__<repo>.json` per repository plus a human-readable
//! `RESULTS.md`. JSON structure is deterministic (fixed field order, fixed
//! scenario catalog order, sorted measurement keys); measured values vary by
//! machine, so CI validates artifacts with `--verify` instead of diffing
//! them. Schema documented in `docs/support/corpus/README.md`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::manifest::CorpusManifest;
use super::{Check, SCENARIO_IDS};
use crate::failure;

/// Current corpus result-file schema version.
pub const CORPUS_SCHEMA_VERSION: u32 = 2;

/// Repository-level outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoStatus {
    /// The scenario catalog ran against the pinned checkout.
    Evaluated,
    /// Not evaluated; `skip_reason` explains (unsupported language, cache
    /// miss, or pinned-SHA mismatch).
    Skipped,
}

/// Outcome of one corpus scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorpusScenarioStatus {
    Pass,
    Fail,
    Skipped,
}

/// Verdict of one budget comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStatus {
    Pass,
    Fail,
}

/// Wall time of one scenario phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseTiming {
    pub name: String,
    pub wall_micros: u64,
}

/// One budget comparison with its verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetVerdict {
    pub budget: String,
    pub observed: u64,
    pub limit: u64,
    pub status: BudgetStatus,
}

/// Per-scenario record inside a repository result file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusScenarioReport {
    pub id: String,
    pub status: CorpusScenarioStatus,
    /// Assertion notes on pass; the failure message on fail; the reason on
    /// skip.
    pub details: String,
    pub phases: Vec<PhaseTiming>,
    /// Scenario-specific measurements; keys sorted, values vary by machine.
    #[serde(default)]
    pub measurements: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub budget_verdicts: Vec<BudgetVerdict>,
}

/// One emitted `<language>-<owner>__<repo>.json` result file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusRepoReport {
    pub schema_version: u32,
    pub language: String,
    pub repository: String,
    pub sha: String,
    pub status: RepoStatus,
    /// Empty unless `status` is `skipped`.
    pub skip_reason: String,
    /// Precise-provider phase coverage. Evaluated repositories use a
    /// hermetic double so the corpus runner never requires a language server.
    pub provider_phase: String,
    pub scenario_count: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub scenarios: Vec<CorpusScenarioReport>,
}

impl CorpusRepoReport {
    /// Aggregates finished scenario reports into the repository artifact.
    pub fn new(
        language: &str,
        repository: &str,
        sha: &str,
        status: RepoStatus,
        skip_reason: String,
        scenarios: Vec<CorpusScenarioReport>,
    ) -> Self {
        let count = |wanted: CorpusScenarioStatus| {
            scenarios
                .iter()
                .filter(|scenario| scenario.status == wanted)
                .count()
        };
        Self {
            schema_version: CORPUS_SCHEMA_VERSION,
            language: language.to_owned(),
            repository: repository.to_owned(),
            sha: sha.to_owned(),
            status,
            skip_reason,
            provider_phase: match status {
                RepoStatus::Evaluated => "hermetic-startup-failure-restart".to_owned(),
                RepoStatus::Skipped => "not-run".to_owned(),
            },
            scenario_count: scenarios.len(),
            passed: count(CorpusScenarioStatus::Pass),
            failed: count(CorpusScenarioStatus::Fail),
            skipped: count(CorpusScenarioStatus::Skipped),
            scenarios,
        }
    }

    /// Deterministic JSON rendering (pretty, trailing newline).
    pub fn render(&self) -> Check<String> {
        Ok(format!("{}\n", serde_json::to_string_pretty(self)?))
    }

    /// Result file name for this repository (`<language>-<slug>.json`).
    pub fn file_name(&self) -> String {
        format!(
            "{}-{}.json",
            self.language,
            self.repository.replace('/', "__")
        )
    }

    /// Looks up one scenario report by id.
    pub fn scenario(&self, id: &str) -> Option<&CorpusScenarioReport> {
        self.scenarios.iter().find(|scenario| scenario.id == id)
    }
}

/// Validates every committed result file in `results_dir` against the
/// manifest: JSON parses into the current schema, language/repository/SHA are
/// manifest-consistent, and an evaluated repository reports exactly the
/// scenario catalog in catalog order. Returns the sorted list of problems;
/// an empty list means the artifacts are well-formed.
pub fn verify_results(results_dir: &Path, manifest: &CorpusManifest) -> Check<Vec<String>> {
    let mut problems = Vec::new();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(results_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            problems.push(format!("{}: non-UTF-8 file name", entry.path().display()));
            continue;
        };
        if name.ends_with(".json") {
            files.push(entry.path());
        }
    }
    files.sort();
    if files.is_empty() {
        problems.push(format!(
            "{}: no corpus result files (*.json) found",
            results_dir.display()
        ));
    }
    for file in files {
        verify_one(&file, manifest, &mut problems);
    }
    problems.sort();
    Ok(problems)
}

fn verify_one(path: &Path, manifest: &CorpusManifest, problems: &mut Vec<String>) {
    let label = || path.display().to_string();
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            problems.push(format!("{}: unreadable: {error}", label()));
            return;
        }
    };
    let report: CorpusRepoReport = match serde_json::from_str(&raw) {
        Ok(report) => report,
        Err(error) => {
            problems.push(format!(
                "{}: does not parse as a corpus result: {error}",
                label()
            ));
            return;
        }
    };
    if report.schema_version != CORPUS_SCHEMA_VERSION {
        problems.push(format!(
            "{}: schema_version {} != {CORPUS_SCHEMA_VERSION}",
            label(),
            report.schema_version
        ));
    }
    let expected_name = report.file_name();
    let actual_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if actual_name != expected_name {
        problems.push(format!(
            "{}: file name does not match contents (expected {expected_name})",
            label()
        ));
    }
    let Some(language) = manifest.languages.get(&report.language) else {
        problems.push(format!(
            "{}: language `{}` is not in the corpus manifest",
            label(),
            report.language
        ));
        return;
    };
    let Some(repository) = language
        .repositories
        .iter()
        .find(|repository| repository.name == report.repository)
    else {
        problems.push(format!(
            "{}: repository `{}` is not in the corpus manifest",
            label(),
            report.repository
        ));
        return;
    };
    if report.sha != repository.sha {
        problems.push(format!(
            "{}: sha {} does not match the pinned manifest sha {}",
            label(),
            report.sha,
            repository.sha
        ));
    }
    if report.status == RepoStatus::Skipped && report.skip_reason.is_empty() {
        problems.push(format!("{}: skipped without a skip_reason", label()));
    }
    let expected_provider_phase = match report.status {
        RepoStatus::Evaluated => "hermetic-startup-failure-restart",
        RepoStatus::Skipped => "not-run",
    };
    if report.provider_phase != expected_provider_phase {
        problems.push(format!(
            "{}: provider_phase {:?} != {expected_provider_phase:?}",
            label(),
            report.provider_phase
        ));
    }
    if report.status == RepoStatus::Evaluated {
        let ids: Vec<&str> = report
            .scenarios
            .iter()
            .map(|scenario| scenario.id.as_str())
            .collect();
        if ids != SCENARIO_IDS {
            problems.push(format!(
                "{}: scenario ids {ids:?} do not match the catalog {SCENARIO_IDS:?}",
                label()
            ));
        }
    }
    let passed = report
        .scenarios
        .iter()
        .filter(|scenario| scenario.status == CorpusScenarioStatus::Pass)
        .count();
    let failed = report
        .scenarios
        .iter()
        .filter(|scenario| scenario.status == CorpusScenarioStatus::Fail)
        .count();
    let skipped = report
        .scenarios
        .iter()
        .filter(|scenario| scenario.status == CorpusScenarioStatus::Skipped)
        .count();
    if passed != report.passed
        || failed != report.failed
        || skipped != report.skipped
        || report.scenario_count != report.scenarios.len()
    {
        problems.push(format!(
            "{}: aggregate counts do not match scenarios",
            label()
        ));
    }
    for scenario in &report.scenarios {
        for verdict in &scenario.budget_verdicts {
            let failed_verdict = verdict.observed > verdict.limit;
            if failed_verdict != (verdict.status == BudgetStatus::Fail) {
                problems.push(format!(
                    "{}: scenario {} budget {} verdict disagrees with observed/limit",
                    label(),
                    scenario.id,
                    verdict.budget
                ));
            }
        }
    }
}

/// Loads every result file in a directory (used to regenerate RESULTS.md).
pub fn load_results(results_dir: &Path) -> Check<Vec<CorpusRepoReport>> {
    let mut reports: Vec<CorpusRepoReport> = Vec::new();
    for entry in std::fs::read_dir(results_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| name.ends_with(".json")) {
            let raw = std::fs::read_to_string(entry.path())?;
            reports.push(serde_json::from_str(&raw)?);
        }
    }
    reports.sort_by(|left, right| {
        (&left.language, &left.repository).cmp(&(&right.language, &right.repository))
    });
    Ok(reports)
}

/// Renders the human summary `RESULTS.md` from all committed result files.
pub fn render_results_md(reports: &[CorpusRepoReport], machine: &str, date: &str) -> String {
    let mut page = String::new();
    page.push_str("# Public corpus evaluation results (issue #25)\n\n");
    page.push_str(&format!(
        "Produced by `cargo run --release -p chakra-conformance -- corpus --emit docs/support/corpus/results` \
         on {machine}, {date}.\n\n"
    ));
    page.push_str(
        "Measured values vary by machine and run; these artifacts are committed deliberately and \
         are **not** diffed in CI. CI runs `chakra-conformance corpus --verify`, which checks \
         artifact structure and manifest consistency only. Budgets live in `budgets.json`; \
         refreshing budgets or baselines requires review.\n\n",
    );
    page.push_str(
        "| Language | Repository | SHA | Status | Cold index (s) | Peak RSS (MiB) | Symbols | Edges | Warm no-op (ms) | Scenarios failed |\n",
    );
    page.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
    for report in reports {
        let cold = report.scenario("cold-index");
        let warm = report.scenario("warm-noop");
        let cold_secs = measurement_f64(cold, "wall_micros")
            .map_or("—".to_owned(), |micros| format!("{:.2}", micros / 1e6));
        let rss = match measurement_f64(cold, "peak_rss_bytes") {
            Some(bytes) => format!("{:.0}", bytes / (1024.0 * 1024.0)),
            None => "unavailable".to_owned(),
        };
        let symbols = measurement_f64(cold, "symbols")
            .map_or("—".to_owned(), |value| format!("{value:.0}"));
        let edges =
            measurement_f64(cold, "edges").map_or("—".to_owned(), |value| format!("{value:.0}"));
        let warm_ms = measurement_f64(warm, "wall_micros")
            .map_or("—".to_owned(), |micros| format!("{:.0}", micros / 1e3));
        let status = match report.status {
            RepoStatus::Evaluated => {
                if report.failed == 0 {
                    "pass".to_owned()
                } else {
                    "fail".to_owned()
                }
            }
            RepoStatus::Skipped => format!("skipped ({})", report.skip_reason),
        };
        page.push_str(&format!(
            "| {} | {} | `{}` | {} | {} | {} | {} | {} | {} | {} |\n",
            report.language,
            report.repository,
            &report.sha[..report.sha.len().min(12)],
            status,
            cold_secs,
            rss,
            symbols,
            edges,
            warm_ms,
            report.failed,
        ));
    }
    page
}

fn measurement_f64(scenario: Option<&CorpusScenarioReport>, key: &str) -> Option<f64> {
    scenario?.measurements.get(key)?.as_f64()
}

/// Fails when `condition` does not hold (scenario assertion helper).
pub(super) fn reject(condition: bool, message: impl Into<String>) -> Check<()> {
    if condition {
        Ok(())
    } else {
        Err(failure(message).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_scenario(id: &str, status: CorpusScenarioStatus) -> CorpusScenarioReport {
        CorpusScenarioReport {
            id: id.to_owned(),
            status,
            details: String::new(),
            phases: vec![PhaseTiming {
                name: "run".to_owned(),
                wall_micros: 42,
            }],
            measurements: BTreeMap::from([("symbols".to_owned(), serde_json::Value::from(7_u64))]),
            budget_verdicts: vec![BudgetVerdict {
                budget: "cold_index_wall_micros".to_owned(),
                observed: 42,
                limit: 100,
                status: BudgetStatus::Pass,
            }],
        }
    }

    fn sample_report() -> CorpusRepoReport {
        CorpusRepoReport::new(
            "rust",
            "tokio-rs/tokio",
            "625954f365727668cb02d04172b34f1149637728",
            RepoStatus::Evaluated,
            String::new(),
            SCENARIO_IDS
                .iter()
                .map(|id| sample_scenario(id, CorpusScenarioStatus::Pass))
                .collect(),
        )
    }

    #[test]
    fn render_is_deterministic_and_counts() -> Check<()> {
        let report = sample_report();
        assert_eq!(report.render()?, report.render()?);
        assert_eq!(report.scenario_count, SCENARIO_IDS.len());
        assert_eq!(report.passed, SCENARIO_IDS.len());
        assert_eq!(report.failed, 0);
        assert_eq!(report.provider_phase, "hermetic-startup-failure-restart");
        assert_eq!(report.file_name(), "rust-tokio-rs__tokio.json");
        Ok(())
    }

    #[test]
    fn verify_accepts_a_consistent_result_and_rejects_drift() -> Check<()> {
        let manifest = CorpusManifest::load(&crate::corpus::default_manifest_path())?;
        let directory = tempfile::TempDir::new()?;
        let path = directory.path().join(sample_report().file_name());
        std::fs::write(&path, sample_report().render()?)?;
        assert!(verify_results(directory.path(), &manifest)?.is_empty());

        let mut drifted = sample_report();
        drifted.sha = "0".repeat(40);
        std::fs::write(&path, drifted.render()?)?;
        let problems = verify_results(directory.path(), &manifest)?;
        assert!(problems.iter().any(|problem| problem.contains("sha")));

        let mut missing = sample_report();
        missing.scenarios.remove(0);
        std::fs::write(&path, missing.render()?)?;
        let problems = verify_results(directory.path(), &manifest)?;
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("scenario ids"))
        );

        std::fs::remove_file(&path)?;
        let problems = verify_results(directory.path(), &manifest)?;
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("no corpus result"))
        );
        Ok(())
    }

    #[test]
    fn results_md_lists_every_report() {
        let page = render_results_md(
            &[sample_report()],
            "test-os/test-arch (8 CPUs)",
            "2026-08-18",
        );
        assert!(page.contains("tokio-rs/tokio"));
        assert!(page.contains("2026-08-18"));
        assert!(page.contains("625954f36572"));
    }
}
