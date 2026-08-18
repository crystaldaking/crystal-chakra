//! `chakra-conformance` CLI: run the cross-language conformance catalog and
//! optionally emit machine-readable result files.

use std::path::PathBuf;
use std::process::ExitCode;

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
