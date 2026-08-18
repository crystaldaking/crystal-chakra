//! `chakra-conformance` CLI: run the cross-language conformance catalog and
//! optionally emit machine-readable result files.

use std::path::PathBuf;
use std::process::ExitCode;

use chakra_conformance::corpus::{
    CorpusBudgets, CorpusManifest, CorpusRepoReport, RepoStatus, default_budgets_path,
    default_cache_root, default_manifest_path, default_results_dir, evaluate_language,
    load_results, machine_description, render_results_md, today_utc, verify_results,
};
use chakra_conformance::{Check, LanguageReport, languages, run_language};
use clap::{Parser, Subcommand};

/// Cross-language conformance harness (CONFORM-01).
#[derive(Parser)]
#[command(name = "chakra-conformance", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the scenario catalog and print a human-readable summary.
    Run {
        /// Languages to run (default: every fixture language).
        #[arg(long)]
        language: Vec<String>,
    },
    /// Run the catalog and write `<language>.json` result files.
    Emit {
        /// Directory receiving the result files.
        dir: PathBuf,
        /// Languages to emit (default: every fixture language).
        #[arg(long)]
        language: Vec<String>,
    },
    /// Evaluate the pinned public corpus (issue #25). Never fetches; missing
    /// or SHA-mismatched checkouts are recorded as skipped repositories.
    Corpus {
        /// Validate committed result artifacts against the manifest instead
        /// of running evaluations.
        #[arg(long)]
        verify: bool,
        /// Languages to evaluate (default: every manifest language;
        /// unsupported ones are recorded as skipped).
        #[arg(long)]
        language: Vec<String>,
        /// Write `<language>-<repo>.json` artifacts here and regenerate
        /// RESULTS.md next to the directory.
        #[arg(long)]
        emit: Option<PathBuf>,
        /// Corpus manifest path (default: docs/support/corpus/manifest.json).
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Budgets path (default: docs/support/corpus/budgets.json).
        #[arg(long)]
        budgets: Option<PathBuf>,
        /// Corpus cache root (default: target/corpus).
        #[arg(long)]
        cache: Option<PathBuf>,
        /// Results directory for --verify (default:
        /// docs/support/corpus/results).
        #[arg(long)]
        results: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match execute() {
        Ok(0) => ExitCode::SUCCESS,
        Ok(failures) => {
            eprintln!("conformance: {failures} scenario(s) failed");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("conformance: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the number of failed scenarios across all selected languages.
fn execute() -> Check<u64> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run { language } => {
            let mut failures = 0;
            for report in reports(language)? {
                print_summary(&report);
                failures += report.failed as u64;
            }
            Ok(failures)
        }
        Command::Emit { dir, language } => {
            std::fs::create_dir_all(&dir)?;
            let mut failures = 0;
            for report in reports(language)? {
                let path = dir.join(format!("{}.json", report.language));
                std::fs::write(&path, report.render()?)?;
                println!("wrote {}", path.display());
                print_summary(&report);
                failures += report.failed as u64;
            }
            Ok(failures)
        }
        Command::Corpus {
            verify,
            language,
            emit,
            manifest,
            budgets,
            cache,
            results,
        } => corpus(language, emit, manifest, budgets, cache, results, verify),
    }
}

/// Corpus subcommand: `--verify` validates committed artifacts; otherwise the
/// selected languages are evaluated and optionally emitted.
#[allow(clippy::too_many_arguments)]
fn corpus(
    language: Vec<String>,
    emit: Option<PathBuf>,
    manifest: Option<PathBuf>,
    budgets: Option<PathBuf>,
    cache: Option<PathBuf>,
    results: Option<PathBuf>,
    verify: bool,
) -> Check<u64> {
    let manifest = CorpusManifest::load(&manifest.unwrap_or_else(default_manifest_path))?;
    if verify {
        let dir = results.unwrap_or_else(default_results_dir);
        let problems = verify_results(&dir, &manifest)?;
        for problem in &problems {
            eprintln!("corpus verify: {problem}");
        }
        println!("corpus verify: {} problem(s)", problems.len());
        return Ok(u64::try_from(problems.len()).unwrap_or(u64::MAX));
    }
    let budgets = CorpusBudgets::load(&budgets.unwrap_or_else(default_budgets_path))?;
    let cache = cache.unwrap_or_else(default_cache_root);
    let languages = if language.is_empty() {
        manifest.language_names()
    } else {
        language
    };
    let mut reports = Vec::new();
    let mut failures = 0_u64;
    for language in &languages {
        for report in evaluate_language(language, &manifest, &budgets, &cache)? {
            print_corpus_summary(&report);
            failures += report.failed as u64;
            reports.push(report);
        }
    }
    if let Some(dir) = emit {
        std::fs::create_dir_all(&dir)?;
        for report in &reports {
            let path = dir.join(report.file_name());
            std::fs::write(&path, report.render()?)?;
            println!("wrote {}", path.display());
        }
        let page = render_results_md(&load_results(&dir)?, &machine_description(), &today_utc());
        let results_md = dir.parent().map_or_else(
            || dir.join("RESULTS.md"),
            |parent| parent.join("RESULTS.md"),
        );
        std::fs::write(&results_md, page)?;
        println!("wrote {}", results_md.display());
    }
    Ok(failures)
}

fn print_corpus_summary(report: &CorpusRepoReport) {
    if report.status == RepoStatus::Skipped {
        println!(
            "{} {}: skipped ({})",
            report.language, report.repository, report.skip_reason
        );
        return;
    }
    println!(
        "{} {}: {}/{} scenarios passed",
        report.language, report.repository, report.passed, report.scenario_count
    );
    for scenario in &report.scenarios {
        if scenario.status == chakra_conformance::corpus::CorpusScenarioStatus::Fail {
            println!("  FAIL {}: {}", scenario.id, scenario.details);
        }
    }
}

fn reports(selected: Vec<String>) -> Check<Vec<LanguageReport>> {
    let languages = if selected.is_empty() {
        languages()?
    } else {
        selected
    };
    languages
        .iter()
        .map(|language| run_language(language))
        .collect()
}

fn print_summary(report: &LanguageReport) {
    println!(
        "{}: {}/{} scenarios passed",
        report.language, report.passed, report.scenario_count
    );
    for scenario in &report.scenarios {
        if scenario.status == chakra_conformance::ScenarioStatus::Fail {
            println!("  FAIL {}: {}", scenario.id, scenario.details);
        }
    }
}
