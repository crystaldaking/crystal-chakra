//! Conformance scenario regression gate: the committed catalog must pass for
//! every fixture language, and the runner must report failures loudly.

use std::error::Error;

use chakra_conformance::{
    Manifest, ScenarioStatus, fixtures_root, languages, run_language, run_scenario,
    validate_manifest,
};

fn assert_language_passes(language: &str) -> Result<(), Box<dyn Error>> {
    let report = run_language(language)?;
    let failures: Vec<String> = report
        .scenarios
        .iter()
        .filter(|scenario| scenario.status == ScenarioStatus::Fail)
        .map(|scenario| format!("{}: {}", scenario.id, scenario.details))
        .collect();
    assert!(
        failures.is_empty(),
        "{language} conformance failures:\n{}",
        failures.join("\n")
    );
    assert_eq!(report.failed, 0);
    assert_eq!(report.passed, report.scenario_count);
    Ok(())
}

#[test]
fn rust_conformance_suite_passes() -> Result<(), Box<dyn Error>> {
    assert_language_passes("rust")
}

#[test]
fn php_conformance_suite_passes() -> Result<(), Box<dyn Error>> {
    assert_language_passes("php")
}

#[test]
fn java_conformance_suite_passes() -> Result<(), Box<dyn Error>> {
    assert_language_passes("java")
}

#[test]
fn every_fixture_language_declares_the_implemented_catalog() -> Result<(), Box<dyn Error>> {
    let discovered = languages()?;
    assert!(discovered.contains(&"rust".to_owned()));
    assert!(discovered.contains(&"php".to_owned()));
    for language in discovered {
        let manifest = Manifest::load(&fixtures_root().join(&language))?;
        validate_manifest(&manifest)?;
    }
    Ok(())
}

#[test]
fn unmet_expectations_fail_loudly() -> Result<(), Box<dyn Error>> {
    let mut manifest = Manifest::load(&fixtures_root().join("rust"))?;
    manifest.expectations.ambiguous_name = "definitely_not_a_fixture_symbol".to_owned();
    let report = run_scenario(&manifest, "ambiguity")?;
    assert_eq!(report.status, ScenarioStatus::Fail);
    assert!(!report.details.is_empty());
    Ok(())
}

#[test]
fn unknown_scenario_ids_are_rejected() -> Result<(), Box<dyn Error>> {
    let manifest = Manifest::load(&fixtures_root().join("rust"))?;
    assert!(run_scenario(&manifest, "no-such-scenario").is_err());
    Ok(())
}
