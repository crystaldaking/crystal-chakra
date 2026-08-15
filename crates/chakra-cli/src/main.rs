//! `chakra` — user-facing entry point of the Chakra code intelligence service.
//!
//! `chakra serve` indexes the materialized Git worktree, atomically publishes
//! the first fresh syntax revision, then runs MCP over stdio (ADR-0003).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_engine::WorkspaceEngine;
use clap::{Args, CommandFactory, Parser, Subcommand};

/// Local code intelligence layer for AI coding agents.
#[derive(Debug, Parser)]
#[command(name = "chakra", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Serve MCP over stdio; this is what agents connect to.
    Serve(ServeArgs),
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Repository root to serve (single repository, single worktree in v0.1).
    #[arg(long, value_name = "PATH", default_value = ".")]
    repo: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => {
            let mut command = Cli::command();
            match command.print_help() {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("chakra: failed to print help: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(Commands::Serve(args)) => serve(args).await,
    }
}

async fn serve(args: ServeArgs) -> ExitCode {
    // MCP owns stdout; logs go to stderr only.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    // Parsing is CPU-heavy and filesystem/Git discovery is blocking. Keep it
    // on Tokio's owned blocking pool instead of a runtime worker.
    let report = match tokio::task::spawn_blocking(move || {
        chakra_language_rust::index_repository(&args.repo)
    })
    .await
    {
        Ok(Ok(report)) => report,
        Ok(Err(error)) => {
            eprintln!("chakra: {error}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("chakra: syntax index task failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let identity = match WorkspaceIdentity::for_primary_worktree(&report.repository_root) {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("chakra: {error}");
            return ExitCode::FAILURE;
        }
    };
    let engine = WorkspaceEngine::new(identity);
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Ready);
    // Discovery and parsing used one complete scan of the actual worktree;
    // graph mutation revoked freshness, so the indexing owner reclaims it
    // only after the graph is complete and internally validated.
    update.set_freshness(Freshness::Fresh);
    if let Err(error) = engine.publish(update) {
        eprintln!("chakra: failed to publish initial syntax index: {error}");
        return ExitCode::FAILURE;
    }
    tracing::info!(
        files = report.metrics.parsed_files,
        syntax_error_files = report.metrics.syntax_error_files,
        truncated_call_sites = report.metrics.truncated_call_sites,
        symbols = report.metrics.symbols,
        edges = report.metrics.edges,
        elapsed_micros = report.metrics.elapsed.as_micros(),
        "initial Rust syntax revision published"
    );
    match chakra_mcp::serve_stdio(Arc::new(engine)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("chakra: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_invocation_carries_no_command() {
        let cli = Cli::try_parse_from(["chakra"]);
        assert!(matches!(cli, Ok(Cli { command: None })));
    }

    #[test]
    fn serve_parses_repo_path() {
        let cli = Cli::try_parse_from(["chakra", "serve", "--repo", "/tmp/example"]);
        assert!(matches!(
            cli,
            Ok(Cli {
                command: Some(Commands::Serve(ref args)),
            }) if args.repo == Path::new("/tmp/example")
        ));
    }
}
