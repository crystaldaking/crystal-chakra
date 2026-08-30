//! Incremental Tree-sitter parsing evaluation tests (issue #45).
//!
//! The default-suite tests are the hermetic correctness gate: generated
//! per-language sources, scenario edits, and a seeded random edit fuzz must
//! produce incremental trees structurally identical to full reparses. The
//! `#[ignore]`d tests are the release-mode benchmark runs whose output feeds
//! `docs/evaluation/v0.2.0-incremental-tree-sitter.md`.

use chakra_conformance::corpus::machine_description;
use chakra_conformance::incremental::{
    BenchLanguage, CorpusSelection, EditPattern, ScenarioOutcome, ScenarioRow, TextEdit,
    bench_languages, cancellation_latency, cold_parse_timing, corpus_documents,
    fuzz_edit_equivalence, hermetic_source, memory_file_set, render_results_markdown,
    retained_tree_memory, run_edit_scenario, trees_are_structurally_equal,
};
use chakra_conformance::{Check, ensure};

const HERMETIC_FUNCTIONS: usize = 300;
const HERMETIC_FUZZ_STEPS: usize = 60;

#[test]
fn text_edits_reject_invalid_ranges_and_mismatched_results() -> Check<()> {
    let split_code_point = TextEdit {
        start_byte: 1,
        old_end_byte: 1,
        new_text: "x".to_owned(),
    };
    ensure(
        split_code_point.apply("λ").is_err(),
        "an edit must not split a UTF-8 code point",
    )?;

    let replacement = TextEdit {
        start_byte: 3,
        old_end_byte: 6,
        new_text: "beta".to_owned(),
    };
    ensure(
        replacement.input_edit("fn alpha", "fn gamma").is_err(),
        "InputEdit must describe the supplied post-edit source",
    )?;
    Ok(())
}

#[test]
fn structural_comparison_observes_tree_shape() -> Check<()> {
    let language = bench_languages()[0];
    let mut parser = language.parser()?;
    let function = parser
        .parse("fn value() {}", None)
        .ok_or_else(|| chakra_conformance::failure("function parse produced no tree"))?;
    let same_function = parser
        .parse("fn value() {}", None)
        .ok_or_else(|| chakra_conformance::failure("repeat parse produced no tree"))?;
    let structure = parser
        .parse("struct Value;", None)
        .ok_or_else(|| chakra_conformance::failure("struct parse produced no tree"))?;
    ensure(
        trees_are_structurally_equal(&function, &same_function),
        "identical full parses must compare equal",
    )?;
    ensure(
        !trees_are_structurally_equal(&function, &structure),
        "different syntax trees must compare unequal",
    )?;
    Ok(())
}

#[test]
fn hermetic_sources_parse_without_errors() -> Check<()> {
    for language in bench_languages() {
        let source = hermetic_source(&language, HERMETIC_FUNCTIONS);
        let mut parser = language.parser()?;
        let tree = parser
            .parse(&source, None)
            .ok_or_else(|| chakra_conformance::failure("hermetic source produced no tree"))?;
        ensure(
            !tree.root_node().has_error(),
            format!("{} hermetic source must parse cleanly", language.name),
        )?;
    }
    Ok(())
}

#[test]
fn fuzzed_incremental_trees_match_full_reparses() -> Check<()> {
    for (index, language) in bench_languages().iter().enumerate() {
        let source = hermetic_source(language, HERMETIC_FUNCTIONS);
        let report = fuzz_edit_equivalence(
            language,
            &source,
            HERMETIC_FUZZ_STEPS,
            0x5eed_0000 + index as u64,
        )?;
        ensure(
            report.mismatches == 0,
            format!(
                "{}: {} of {} fuzzed incremental trees diverged from a full reparse",
                language.name, report.mismatches, report.steps
            ),
        )?;
    }
    Ok(())
}

#[test]
fn scenario_edits_match_full_reparses() -> Check<()> {
    for language in bench_languages() {
        let base = hermetic_source(&language, HERMETIC_FUNCTIONS);
        let other = hermetic_source(&language, HERMETIC_FUNCTIONS / 2);
        let patterns = [
            EditPattern::SmallEdit,
            EditPattern::SyntaxError,
            EditPattern::AtomicReplace(other),
        ];
        for pattern in &patterns {
            let outcome = run_edit_scenario(&language, &base, pattern, 10)?;
            ensure(
                outcome.structural_mismatches == 0,
                format!(
                    "{}: {} scenario edits diverged from a full reparse",
                    language.name, outcome.structural_mismatches
                ),
            )?;
        }
    }
    Ok(())
}

/// Language filter shared by the benchmark tests so every measurement can run
/// in a fresh process (`CHAKRA_INCREMENTAL_BENCH_LANGUAGE=rust` etc.).
fn selected_bench_languages() -> Vec<BenchLanguage> {
    let filter = std::env::var("CHAKRA_INCREMENTAL_BENCH_LANGUAGE").ok();
    bench_languages()
        .into_iter()
        .filter(|language| filter.as_deref().is_none_or(|name| name == language.name))
        .collect()
}

fn cold_row(
    language: &BenchLanguage,
    label: &str,
    source: &str,
    iterations: usize,
) -> Check<ScenarioRow> {
    let full = cold_parse_timing(language, source, iterations)?;
    Ok(ScenarioRow {
        language: language.name.to_owned(),
        document: label.to_owned(),
        source_bytes: source.len(),
        scenario: "cold".to_owned(),
        outcome: ScenarioOutcome {
            full,
            incremental: full,
            structural_comparisons: 0,
            structural_mismatches: 0,
        },
    })
}

fn scenario_row(
    language: &BenchLanguage,
    label: &str,
    source: &str,
    scenario: &str,
    pattern: &EditPattern,
    iterations: usize,
) -> Check<ScenarioRow> {
    Ok(ScenarioRow {
        language: language.name.to_owned(),
        document: label.to_owned(),
        source_bytes: source.len(),
        scenario: scenario.to_owned(),
        outcome: run_edit_scenario(language, source, pattern, iterations)?,
    })
}

/// Runs the scenario catalog over one document selection and prints the
/// markdown table. Medium documents get cold/small-edit/syntax-error runs;
/// the largest document gets the large-file small-edit run; atomic replace
/// alternates the two largest documents.
fn bench_selection(
    language: &BenchLanguage,
    documents: &[(String, String)],
    rows: &mut Vec<ScenarioRow>,
) -> Check<()> {
    let mediums = &documents[2.min(documents.len())..];
    for (label, source) in mediums {
        rows.push(cold_row(language, label, source, 15)?);
        rows.push(scenario_row(
            language,
            label,
            source,
            "small-edit",
            &EditPattern::SmallEdit,
            25,
        )?);
    }
    if let Some((label, source)) = mediums.first() {
        rows.push(scenario_row(
            language,
            label,
            source,
            "syntax-error",
            &EditPattern::SyntaxError,
            12,
        )?);
    }
    for (label, source) in documents.iter().take(2) {
        rows.push(scenario_row(
            language,
            label,
            source,
            "large-file-edit",
            &EditPattern::SmallEdit,
            9,
        )?);
    }
    if let Some((label, source)) = documents.first()
        && let Some((_, other)) = documents.get(1)
    {
        rows.push(scenario_row(
            language,
            label,
            source,
            "atomic-replace",
            &EditPattern::AtomicReplace(other.clone()),
            9,
        )?);
    }
    Ok(())
}

#[test]
#[ignore = "benchmark: run in release with --ignored --nocapture"]
fn bench_hermetic_documents() -> Check<()> {
    let mut rows = Vec::new();
    for language in selected_bench_languages() {
        let documents = vec![
            (
                "hermetic-large".to_owned(),
                hermetic_source(&language, 6000),
            ),
            ("hermetic-alt".to_owned(), hermetic_source(&language, 4000)),
            (
                "hermetic-medium".to_owned(),
                hermetic_source(&language, 900),
            ),
        ];
        bench_selection(&language, &documents, &mut rows)?;
    }
    println!("machine: {}", machine_description());
    print!("{}", render_results_markdown(&rows));
    Ok(())
}

#[test]
#[ignore = "benchmark: run in release with --ignored --nocapture"]
fn bench_corpus_documents() -> Check<()> {
    let mut rows = Vec::new();
    for language in selected_bench_languages() {
        let selection: CorpusSelection = corpus_documents(&language, 2, 4)?;
        if selection.is_empty() {
            println!("{}: no cached corpus documents, skipping", language.name);
            continue;
        }
        let documents: Vec<(String, String)> = selection
            .largest
            .iter()
            .chain(&selection.mediums)
            .map(|document| (document.label.clone(), document.source.clone()))
            .collect();
        bench_selection(&language, &documents, &mut rows)?;
        if let Some(document) = selection.mediums.first() {
            let fuzz = fuzz_edit_equivalence(&language, &document.source, 300, 0x5eed_c04d)?;
            println!(
                "{} corpus fuzz {}: {}/{} steps, {} mismatches, {} error-tree steps",
                language.name,
                document.label,
                fuzz.steps,
                fuzz.steps,
                fuzz.mismatches,
                fuzz.error_tree_steps
            );
        }
    }
    println!("machine: {}", machine_description());
    print!("{}", render_results_markdown(&rows));
    Ok(())
}

#[test]
#[ignore = "benchmark: run in release with --ignored --nocapture, one language per process"]
fn bench_retained_memory() -> Check<()> {
    for language in selected_bench_languages() {
        let sources = memory_file_set(&language)?;
        if sources.is_empty() {
            println!("{}: no cached corpus file set, skipping", language.name);
            continue;
        }
        let report = retained_tree_memory(&language, &sources)?;
        println!(
            "{}: {} files, {} source bytes, RSS {} -> {} (delta {} bytes, {:.2} tree bytes per source byte)",
            language.name,
            report.files,
            report.source_bytes,
            report.rss_before_bytes,
            report.rss_with_trees_bytes,
            report.retained_delta_bytes(),
            report.tree_bytes_per_source_byte().unwrap_or(f64::NAN),
        );
    }
    Ok(())
}

#[test]
#[ignore = "benchmark: run in release with --ignored --nocapture"]
fn bench_cancellation_latency() -> Check<()> {
    for language in selected_bench_languages() {
        let selection = corpus_documents(&language, 1, 0)?;
        let Some(document) = selection.largest.first() else {
            println!("{}: no cached corpus document, skipping", language.name);
            continue;
        };
        let report = cancellation_latency(&language, &document.source, 15)?;
        println!(
            "{} {}: full median {:.3} ms (min {:.3}, {} completed), incremental median {:.3} ms (min {:.3}, {} completed)",
            language.name,
            document.label,
            report.full.median_wall.as_secs_f64() * 1e3,
            report.full.min_wall.as_secs_f64() * 1e3,
            report.full_completed_before_signal,
            report.incremental.median_wall.as_secs_f64() * 1e3,
            report.incremental.min_wall.as_secs_f64() * 1e3,
            report.incremental_completed_before_signal,
        );
    }
    Ok(())
}
