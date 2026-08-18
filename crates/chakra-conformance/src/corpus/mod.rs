//! Public-corpus evaluation runner (issue #25).
//!
//! Evaluates the pinned public corpus (`docs/support/corpus/manifest.json`,
//! fetched opt-in via `tools/fetch_corpus.py` into `target/corpus/`) against
//! the live engine/query stack, measuring the scenario catalog in
//! [`SCENARIO_IDS`]. The runner never touches the network: missing or
//! SHA-mismatched checkouts are recorded as skipped repositories. Edit
//! scenarios mutate the cached checkout and always restore it afterwards;
//! the `cache-restore` scenario proves the cache still matches the pinned
//! SHA with a clean worktree.

mod manifest;
mod report;
mod scenarios;

use std::path::PathBuf;

pub use manifest::{
    CorpusBudgets, CorpusLanguage, CorpusManifest, CorpusRepository, LanguageBudgets,
};
pub use report::{
    BudgetStatus, BudgetVerdict, CORPUS_SCHEMA_VERSION, CorpusRepoReport, CorpusScenarioReport,
    CorpusScenarioStatus, PhaseTiming, RepoStatus, load_results, render_results_md, verify_results,
};
pub use scenarios::evaluate_language;

use crate::Check;

/// The corpus scenario catalog, in emitted-report order. `cache-restore`
/// closes the catalog: edit scenarios mutate the cached checkout, and this
/// scenario proves the mutation was fully reverted.
pub const SCENARIO_IDS: &[&str] = &[
    "cold-index",
    "warm-noop",
    "fingerprint",
    "one-file-edit",
    "atomic-replace",
    "rename-delete",
    "syntax-error",
    "diff-context",
    "queries",
    "cancellation",
    "cache-restore",
];

/// Languages with a `chakra-language` adapter. Repositories of any other
/// manifest language are recorded as `skipped` with an
/// "unsupported language" reason until their language issue lands.
pub fn supported_languages() -> &'static [&'static str] {
    &["php", "rust"]
}

/// Absolute path of the repository/workspace root.
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// Default corpus manifest path.
pub fn default_manifest_path() -> PathBuf {
    workspace_root().join("docs/support/corpus/manifest.json")
}

/// Default budgets path.
pub fn default_budgets_path() -> PathBuf {
    workspace_root().join("docs/support/corpus/budgets.json")
}

/// Default local corpus cache root (written by `tools/fetch_corpus.py`).
pub fn default_cache_root() -> PathBuf {
    workspace_root().join("target/corpus")
}

/// Default committed results directory.
pub fn default_results_dir() -> PathBuf {
    workspace_root().join("docs/support/corpus/results")
}

/// Machine description recorded in RESULTS.md (OS, arch, logical CPUs).
pub fn machine_description() -> String {
    let cpus = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    format!(
        "{}/{} ({cpus} logical CPUs)",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Current UTC date as `YYYY-MM-DD`, without pulling in a date dependency.
pub fn today_utc() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() / 86_400)
        .unwrap_or(0);
    civil_from_days(days)
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a
/// proleptic Gregorian `YYYY-MM-DD`.
fn civil_from_days(days: u64) -> String {
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    if month <= 2 {
        year += 1;
    }
    format!("{year:04}-{month:02}-{day:02}")
}

/// Ensures every catalog id is unique and non-empty (test helper).
#[cfg(test)]
fn catalog_is_well_formed() -> Check<()> {
    let mut sorted = SCENARIO_IDS.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    crate::ensure(
        sorted.len() == SCENARIO_IDS.len() && sorted.iter().all(|id| !id.is_empty()),
        "corpus scenario catalog has duplicate or empty ids",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_are_unique() -> Check<()> {
        catalog_is_well_formed()
    }

    #[test]
    fn civil_dates_match_known_references() {
        assert_eq!(civil_from_days(0), "1970-01-01");
        assert_eq!(civil_from_days(11_016), "2000-02-29");
        assert_eq!(civil_from_days(19_723), "2024-01-01");
        assert_eq!(civil_from_days(20_683), "2026-08-18");
    }

    #[test]
    fn today_is_plausible() {
        let today = today_utc();
        assert_eq!(today.len(), 10);
        assert!(today.starts_with("20"));
    }
}
