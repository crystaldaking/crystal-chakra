//! Machine-readable conformance result files.
//!
//! Schema documented in the crate root (`lib.rs`) and in
//! `fixtures/conformance/README.md`. Serialization is deterministic: fixed
//! struct field order, scenario catalog order, no timestamps.

use serde::Serialize;

use crate::Check;

/// Current result-file schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Outcome of one scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioStatus {
    Pass,
    Fail,
}

/// Per-scenario record inside a language result file.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioReport {
    pub id: String,
    pub description: String,
    pub capability_ids: Vec<String>,
    pub status: ScenarioStatus,
    /// Provenance/precision checks the scenario performed (PROV-01 evidence).
    pub provenance_assertions: Vec<String>,
    /// Empty on pass; the failure message on fail.
    pub details: String,
}

/// One emitted `<language>.json` result file.
#[derive(Debug, Clone, Serialize)]
pub struct LanguageReport {
    pub schema_version: u32,
    pub language: String,
    pub scenario_count: usize,
    pub passed: usize,
    pub failed: usize,
    pub scenarios: Vec<ScenarioReport>,
}

impl LanguageReport {
    /// Builds the aggregate report for one language.
    pub fn new(language: &str, scenarios: Vec<ScenarioReport>) -> Self {
        let passed = scenarios
            .iter()
            .filter(|scenario| scenario.status == ScenarioStatus::Pass)
            .count();
        Self {
            schema_version: SCHEMA_VERSION,
            language: language.to_owned(),
            scenario_count: scenarios.len(),
            passed,
            failed: scenarios.len() - passed,
            scenarios,
        }
    }

    /// Deterministic JSON rendering (pretty, trailing newline).
    pub fn render(&self) -> Check<String> {
        Ok(format!("{}\n", serde_json::to_string_pretty(self)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LanguageReport {
        LanguageReport::new(
            "rust",
            vec![ScenarioReport {
                id: "ambiguity".to_owned(),
                description: "duplicate names are reported".to_owned(),
                capability_ids: vec!["AMBIG-01".to_owned()],
                status: ScenarioStatus::Pass,
                provenance_assertions: vec!["symbol facts: tree_sitter/syntax".to_owned()],
                details: String::new(),
            }],
        )
    }

    #[test]
    fn render_is_byte_identical_across_calls() -> Result<(), Box<dyn std::error::Error>> {
        let report = sample();
        assert_eq!(report.render()?, report.render()?);
        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 0);
        Ok(())
    }

    #[test]
    fn render_uses_the_documented_schema() -> Result<(), Box<dyn std::error::Error>> {
        let value: serde_json::Value = serde_json::from_str(&sample().render()?)?;
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["scenarios"][0]["status"], "pass");
        assert!(value["scenarios"][0]["provenance_assertions"].is_array());
        Ok(())
    }
}
