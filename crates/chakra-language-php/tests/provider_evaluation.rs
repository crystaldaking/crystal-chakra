use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chakra_domain::symbol::CallResolution;
use chakra_language_php::{index_repository, scan_repository_sources};
use serde_json::Value;
use tempfile::TempDir;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures/php/provider-evaluation")
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&destination)?;
            copy_tree(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

fn repository() -> Result<TempDir, Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    copy_tree(&fixture_root(), repository.path())?;
    Ok(repository)
}

#[test]
fn syntax_baseline_is_anchored_to_the_provider_ground_truth() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let truth: Value = serde_json::from_str(&fs::read_to_string(
        repository.path().join("ground-truth.json"),
    )?)?;
    let calls = truth
        .get("calls")
        .and_then(Value::as_array)
        .ok_or("ground-truth calls must be an array")?;
    let report = index_repository(repository.path())?;
    assert_eq!(report.metrics.discovered_files, 17);
    assert_eq!(report.metrics.syntax_error_files, 0);

    let mut true_positives = 0;
    let mut false_positives = 0;
    let mut false_negatives = 0;
    let mut true_negatives = 0;
    for case in calls {
        let caller_name = case["caller"].as_str().ok_or("caller must be a string")?;
        let call_name = case["name"].as_str().ok_or("name must be a string")?;
        let expected = case["expected_target"].as_str();
        let callers = report.graph.resolve_name(caller_name);
        assert_eq!(callers.len(), 1, "missing corpus caller {caller_name}");
        let sites: Vec<_> = report
            .graph
            .call_sites_from(callers[0])
            .filter(|site| site.name == call_name)
            .collect();
        assert_eq!(
            sites.len(),
            1,
            "missing corpus call {caller_name}::{call_name}"
        );
        let actual = match sites[0].resolution {
            CallResolution::Resolved { target } => report
                .graph
                .symbol(target)
                .map(|symbol| symbol.key.qualified_name.as_str()),
            CallResolution::Ambiguous { .. } | CallResolution::Unresolved => None,
        };
        match (expected, actual) {
            (Some(expected), Some(actual)) if expected == actual => true_positives += 1,
            (Some(_), Some(_)) => {
                false_positives += 1;
                false_negatives += 1;
            }
            (Some(_), None) => false_negatives += 1,
            (None, Some(_)) => false_positives += 1,
            (None, None) => true_negatives += 1,
        }
    }
    assert_eq!((true_positives, false_positives), (5, 0));
    assert_eq!((false_negatives, true_negatives), (3, 1));
    Ok(())
}

#[test]
fn provider_corpus_edit_uses_the_incremental_syntax_path() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let report = index_repository(repository.path())?;
    let controller = repository
        .path()
        .join("app/Http/Controllers/ReportController.php");
    let current = fs::read_to_string(&controller)?;
    let edited = current.replacen(
        "$this->service->generate()",
        "$this->service->missingAfterEdit()",
        1,
    );
    assert_ne!(edited, current);
    fs::write(controller, edited)?;

    let sources = scan_repository_sources(repository.path())?;
    let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
    assert_eq!(reconciled.metrics.reparsed_files, 1);
    assert_eq!(reconciled.metrics.framework_files_reparsed, 1);
    assert_eq!(reconciled.metrics.relationship_files_recomputed, 1);
    assert_eq!(
        reconciled.metrics.framework_relationship_files_recomputed, 1,
        "stable symbol ids keep unchanged route relationships reusable"
    );
    assert!(reconciled.metrics.publication.structurally_incremental);
    reconciled
        .graph
        .as_ref()
        .ok_or("incremental graph missing")?
        .validate_consistency()?;
    Ok(())
}
