//! Reproducible syntax-tier measurements for the PHP provider evaluation corpus.
//!
//! Run with:
//! `cargo run --release -p chakra-language-php --example evaluate_provider_corpus`

use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use chakra_domain::symbol::CallResolution;
use chakra_engine::SymbolGraph;
use chakra_language_php::{index_repository, scan_repository_sources};
use serde_json::{Value, json};
use tempfile::TempDir;

const QUERY_ITERATIONS: usize = 500;

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

fn init_repository(source: &Path) -> Result<TempDir, Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    copy_tree(source, repository.path())?;
    Ok(repository)
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, Box<dyn Error>> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("ground-truth field `{field}` must be a string").into())
}

fn query_case(graph: &SymbolGraph, case: &Value) -> Result<Value, Box<dyn Error>> {
    let id = required_string(case, "id")?;
    let caller_name = required_string(case, "caller")?;
    let call_name = required_string(case, "name")?;
    let expected = case.get("expected_target").and_then(Value::as_str);
    let callers = graph.resolve_name(caller_name);
    let Some(caller) = (callers.len() == 1).then_some(callers[0]) else {
        return Ok(json!({
            "id": id,
            "caller": caller_name,
            "name": call_name,
            "expected_target": expected,
            "actual_target": null,
            "resolution": "caller_missing"
        }));
    };
    let call_sites: Vec<_> = graph
        .call_sites_from(caller)
        .filter(|call_site| call_site.name == call_name)
        .collect();
    let (actual, resolution) = match call_sites.as_slice() {
        [] => (None, "call_site_missing".to_owned()),
        [call_site] => match call_site.resolution {
            CallResolution::Resolved { target } => (
                graph
                    .symbol(target)
                    .map(|symbol| symbol.key.qualified_name.as_str()),
                "resolved".to_owned(),
            ),
            CallResolution::Ambiguous { candidates } => (None, format!("ambiguous:{candidates}")),
            CallResolution::Unresolved => (None, "unresolved".to_owned()),
        },
        _ => (None, format!("multiple_call_sites:{}", call_sites.len())),
    };
    Ok(json!({
        "id": id,
        "caller": caller_name,
        "name": call_name,
        "expected_target": expected,
        "actual_target": actual,
        "resolution": resolution
    }))
}

fn microseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn main() -> Result<(), Box<dyn Error>> {
    let fixture = fixture_root();
    let truth: Value =
        serde_json::from_str(&fs::read_to_string(fixture.join("ground-truth.json"))?)?;
    let calls = truth
        .get("calls")
        .and_then(Value::as_array)
        .ok_or("ground-truth `calls` must be an array")?;
    let repository = init_repository(&fixture)?;

    let startup = Instant::now();
    let report = index_repository(repository.path())?;
    let startup_elapsed = startup.elapsed();
    let case_results: Vec<_> = calls
        .iter()
        .map(|case| query_case(&report.graph, case))
        .collect::<Result<_, _>>()?;

    let mut true_positives = 0_u64;
    let mut false_positives = 0_u64;
    let mut false_negatives = 0_u64;
    let mut true_negatives = 0_u64;
    for result in &case_results {
        let expected = result.get("expected_target").and_then(Value::as_str);
        let actual = result.get("actual_target").and_then(Value::as_str);
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
    let precision_denominator = true_positives + false_positives;
    let recall_denominator = true_positives + false_negatives;
    let precision = if precision_denominator == 0 {
        1.0
    } else {
        true_positives as f64 / precision_denominator as f64
    };
    let recall = if recall_denominator == 0 {
        1.0
    } else {
        true_positives as f64 / recall_denominator as f64
    };

    let mut query_samples = Vec::with_capacity(QUERY_ITERATIONS);
    for _ in 0..QUERY_ITERATIONS {
        let started = Instant::now();
        for case in calls {
            black_box(query_case(&report.graph, case)?);
        }
        query_samples.push(started.elapsed());
    }
    query_samples.sort_unstable();
    let median = query_samples[QUERY_ITERATIONS / 2];
    let p95 = query_samples[QUERY_ITERATIONS * 95 / 100];
    let response_bytes = serde_json::to_vec(&case_results)?.len();

    let changed_path = repository
        .path()
        .join("app/Http/Controllers/ReportController.php");
    let current = fs::read_to_string(&changed_path)?;
    let edited = current.replacen(
        "$this->service->generate()",
        "$this->service->missingAfterEdit()",
        1,
    );
    if edited == current {
        return Err("incremental probe could not locate edit target".into());
    }
    fs::write(changed_path, edited)?;
    let incremental_started = Instant::now();
    let sources = scan_repository_sources(repository.path())?;
    let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
    let incremental_elapsed = incremental_started.elapsed();

    let output = json!({
        "schema_version": 1,
        "corpus": "fixtures/php/provider-evaluation",
        "profile": "release",
        "query_iterations": QUERY_ITERATIONS,
        "index": {
            "startup_microseconds": microseconds(startup_elapsed),
            "discovered_files": report.metrics.discovered_files,
            "syntax_error_files": report.metrics.syntax_error_files,
            "symbols": report.metrics.symbols,
            "edges": report.metrics.edges,
            "call_sites": report.metrics.call_sites,
            "ambiguous_call_sites": report.metrics.ambiguous_call_sites,
            "unresolved_call_sites": report.metrics.unresolved_call_sites,
            "laravel_detected": report.metrics.laravel_detected,
            "framework_edges": report.metrics.framework_edges
        },
        "syntax_accuracy": {
            "true_positives": true_positives,
            "false_positives": false_positives,
            "false_negatives": false_negatives,
            "true_negatives": true_negatives,
            "precision": precision,
            "recall": recall,
            "cases": case_results
        },
        "query": {
            "batch_case_count": calls.len(),
            "median_microseconds": microseconds(median),
            "p95_microseconds": microseconds(p95),
            "serialized_response_bytes": response_bytes
        },
        "incremental_update": {
            "elapsed_microseconds": microseconds(incremental_elapsed),
            "scanned_files": reconciled.metrics.scanned_files,
            "unchanged_files": reconciled.metrics.unchanged_files,
            "reparsed_files": reconciled.metrics.reparsed_files,
            "relationship_files_recomputed": reconciled.metrics.relationship_files_recomputed,
            "framework_files_reparsed": reconciled.metrics.framework_files_reparsed,
            "framework_relationship_files_recomputed": reconciled.metrics.framework_relationship_files_recomputed
        }
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
