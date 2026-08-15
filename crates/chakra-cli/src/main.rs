//! `chakra` — user-facing entry point of the Chakra code intelligence service.
//!
//! `chakra serve` runs the MCP server over stdio (ADR-0003); indexing is
//! added in the next phase.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chakra_domain::identity::WorkspaceIdentity;
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

    let identity = match WorkspaceIdentity::for_primary_worktree(&args.repo) {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("chakra: {error}");
            return ExitCode::FAILURE;
        }
    };
    let engine = WorkspaceEngine::new(identity);
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
