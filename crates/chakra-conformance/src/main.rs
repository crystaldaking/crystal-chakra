//! `chakra-conformance` CLI: run the cross-language conformance catalog and
//! optionally emit machine-readable result files.

use std::path::PathBuf;
use std::process::{Command as ProcessCommand, ExitCode};

use chakra_conformance::corpus::{
    CorpusBudgets, CorpusManifest, CorpusRepoReport, RepoStatus, default_budgets_path,
    default_cache_root, default_manifest_path, default_results_dir, evaluate_language,
    evaluate_named_repository, load_results, machine_description, render_results_md, today_utc,
    verify_results,
};
use chakra_conformance::{Check, LanguageReport, failure, languages, run_language};
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
        /// Evaluate one `owner/repository` (requires exactly one language).
        #[arg(long)]
        repository: Option<String>,
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
            repository,
            emit,
            manifest,
            budgets,
            cache,
            results,
        } => corpus(
            language, repository, emit, manifest, budgets, cache, results, verify,
        ),
    }
}

/// Corpus subcommand: `--verify` validates committed artifacts; otherwise the
/// selected languages are evaluated and optionally emitted.
#[allow(clippy::too_many_arguments)]
fn corpus(
    language: Vec<String>,
    repository: Option<String>,
    emit: Option<PathBuf>,
    manifest: Option<PathBuf>,
    budgets: Option<PathBuf>,
    cache: Option<PathBuf>,
    results: Option<PathBuf>,
    verify: bool,
) -> Check<u64> {
    let manifest_path = manifest.unwrap_or_else(default_manifest_path);
    let manifest = CorpusManifest::load(&manifest_path)?;
    if verify {
        if repository.is_some() {
            return Err(failure("--repository cannot be combined with --verify").into());
        }
        let dir = results.unwrap_or_else(default_results_dir);
        let problems = verify_results(&dir, &manifest)?;
        for problem in &problems {
            eprintln!("corpus verify: {problem}");
        }
        println!("corpus verify: {} problem(s)", problems.len());
        return Ok(u64::try_from(problems.len()).unwrap_or(u64::MAX));
    }
    if cfg!(debug_assertions) {
        return Err(failure(
            "corpus evaluation requires an optimized binary; run `cargo run --release -p chakra-conformance -- corpus ...`",
        )
        .into());
    }
    let budgets_path = budgets.unwrap_or_else(default_budgets_path);
    let cache = cache.unwrap_or_else(default_cache_root);
    let languages = if language.is_empty() {
        manifest.language_names()
    } else {
        language
    };
    if repository.is_some() && languages.len() != 1 {
        return Err(failure("--repository requires exactly one --language").into());
    }
    let selected_repositories: Vec<(String, String)> = languages
        .iter()
        .flat_map(|language| {
            manifest
                .languages
                .get(language)
                .into_iter()
                .flat_map(|entry| {
                    entry
                        .repositories
                        .iter()
                        .map(|repo| (language.clone(), repo.name.clone()))
                })
        })
        .collect();
    if repository.is_none() && selected_repositories.len() > 1 {
        return run_corpus_repositories_isolated(
            &selected_repositories,
            emit.as_deref(),
            &manifest_path,
            &budgets_path,
            &cache,
        );
    }
    let budgets = CorpusBudgets::load(&budgets_path)?;
    let mut reports = Vec::new();
    let mut failures = 0_u64;
    for language in &languages {
        let evaluated = if let Some(repository) = repository.as_deref() {
            vec![evaluate_named_repository(
                language, repository, &manifest, &budgets, &cache,
            )?]
        } else {
            evaluate_language(language, &manifest, &budgets, &cache)?
        };
        for report in evaluated {
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

/// Runs each selected repository in a fresh process so allocator retention
/// and peak-RSS sampling cannot contaminate another repository's evidence or
/// budget verdict.
fn run_corpus_repositories_isolated(
    repositories: &[(String, String)],
    emit: Option<&std::path::Path>,
    manifest: &std::path::Path,
    budgets: &std::path::Path,
    cache: &std::path::Path,
) -> Check<u64> {
    let executable = std::env::current_exe()?;
    for (language, repository) in repositories {
        let mut child = ProcessCommand::new(&executable);
        child
            .arg("corpus")
            .arg("--language")
            .arg(language)
            .arg("--repository")
            .arg(repository)
            .arg("--manifest")
            .arg(manifest)
            .arg("--budgets")
            .arg(budgets)
            .arg("--cache")
            .arg(cache);
        if let Some(dir) = emit {
            child.arg("--emit").arg(dir);
        }
        let status = child.status()?;
        if !status.success() {
            return Err(failure(format!(
                "isolated corpus evaluation failed for {language} `{repository}` with status {status}"
            ))
            .into());
        }
    }
    if let Some(dir) = emit {
        let page = render_results_md(&load_results(dir)?, &machine_description(), &today_utc());
        let results_md = dir.parent().map_or_else(
            || dir.join("RESULTS.md"),
            |parent| parent.join("RESULTS.md"),
        );
        std::fs::write(&results_md, page)?;
        println!("wrote {}", results_md.display());
    }
    Ok(0)
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
