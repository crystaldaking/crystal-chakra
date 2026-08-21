//! Scenario catalog execution and language discovery.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::manifest::{Manifest, ScenarioSpec};
use crate::report::{LanguageReport, ScenarioReport, ScenarioStatus};
use crate::scenarios::{SCENARIOS, ScenarioDef};
use crate::{Check, ensure, failure};

/// Absolute path of `fixtures/conformance/` in this repository.
pub fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("conformance")
}

/// Languages with a conformance fixture (directory holding `manifest.json`).
pub fn languages() -> Check<Vec<String>> {
    let mut languages = Vec::new();
    for entry in std::fs::read_dir(fixtures_root())? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.path().join("manifest.json").is_file() {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| failure("non-UTF-8 fixture directory name"))?;
            languages.push(name);
        }
    }
    languages.sort();
    Ok(languages)
}

/// Loads the manifest of one fixture language.
pub fn load_manifest(language: &str) -> Check<Manifest> {
    Manifest::load(&fixtures_root().join(language))
}

/// The manifest must declare exactly the implemented scenario catalog:
/// every implemented id declared, and no unknown ids.
pub fn validate_manifest(manifest: &Manifest) -> Check<()> {
    let implemented: BTreeSet<&str> = SCENARIOS.iter().map(|def| def.id).collect();
    for def in SCENARIOS {
        ensure(
            manifest.scenario(def.id).is_some(),
            format!(
                "manifest for {} is missing scenario {}",
                manifest.language, def.id
            ),
        )?;
    }
    for spec in &manifest.scenarios {
        ensure(
            implemented.contains(spec.id.as_str()),
            format!(
                "manifest for {} declares unknown scenario {}",
                manifest.language, spec.id
            ),
        )?;
    }
    Ok(())
}

/// Runs the full catalog for one language and aggregates the report.
pub fn run_language(language: &str) -> Check<LanguageReport> {
    let manifest = load_manifest(language)?;
    validate_manifest(&manifest)?;
    let scenarios = SCENARIOS
        .iter()
        .map(|def| run_def(&manifest, def))
        .collect();
    Ok(LanguageReport::new(&manifest.language, scenarios))
}

/// Runs one scenario by id against an explicit manifest (used by negative
/// tests with deliberately corrupted expectations).
pub fn run_scenario(manifest: &Manifest, id: &str) -> Check<ScenarioReport> {
    let def = SCENARIOS
        .iter()
        .find(|def| def.id == id)
        .ok_or_else(|| failure(format!("unknown scenario id {id}")))?;
    Ok(run_def(manifest, def))
}

fn run_def(manifest: &Manifest, def: &ScenarioDef) -> ScenarioReport {
    let spec = manifest
        .scenario(def.id)
        .cloned()
        .unwrap_or_else(|| ScenarioSpec {
            id: def.id.to_owned(),
            description: String::new(),
            capability_ids: Vec::new(),
        });
    match (def.run)(manifest) {
        Ok(assertions) => ScenarioReport {
            id: spec.id,
            description: spec.description,
            capability_ids: spec.capability_ids,
            status: ScenarioStatus::Pass,
            provenance_assertions: assertions,
            details: String::new(),
        },
        Err(error) => ScenarioReport {
            id: spec.id,
            description: spec.description,
            capability_ids: spec.capability_ids,
            status: ScenarioStatus::Fail,
            provenance_assertions: Vec::new(),
            details: error.to_string(),
        },
    }
}
