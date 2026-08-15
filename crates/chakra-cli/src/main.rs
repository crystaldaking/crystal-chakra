//! `chakra` — user-facing entry point of the Chakra code intelligence service.
//!
//! `chakra serve` indexes the materialized Git worktree, starts the live
//! reconciliation owner, then runs MCP over stdio (ADR-0003).

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
    let engine = Arc::new(WorkspaceEngine::new(identity));
    let diff_adapter: Arc<dyn chakra_engine::WorkspaceDiffProvider> =
        Arc::new(chakra_git::GitWorkspaceDiff);
    if let Err(error) = engine.install_diff_provider(diff_adapter) {
        eprintln!("chakra: failed to install Git diff provider: {error}");
        return ExitCode::FAILURE;
    }
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_status(WorkspaceStatus::Indexing);
    // The watcher is not active yet, so the initial scan cannot close the
    // startup race. The live owner reclaims freshness after it starts
    // watching and performs its mandatory reconciliation.
    update.set_freshness(Freshness::Stale);
    if let Err(error) = engine.publish(update) {
        eprintln!("chakra: failed to publish initial syntax index: {error}");
        return ExitCode::FAILURE;
    }
    let initial_metrics = report.metrics;
    tracing::info!(
        files = initial_metrics.parsed_files,
        syntax_error_files = initial_metrics.syntax_error_files,
        truncated_call_sites = initial_metrics.truncated_call_sites,
        symbols = initial_metrics.symbols,
        edges = initial_metrics.edges,
        elapsed_micros = initial_metrics.elapsed.as_micros(),
        "initial Rust syntax revision published as stale pending live reconciliation"
    );
    let repository_root = report.repository_root;
    let syntax_index = report.syntax_index;
    let live_engine = engine.clone();
    let live = match tokio::task::spawn_blocking(move || {
        chakra_language_rust::start_live_rust_index(repository_root, syntax_index, live_engine)
    })
    .await
    {
        Ok(Ok(live)) => live,
        Ok(Err(error)) => {
            eprintln!("chakra: failed to start live Rust index: {error}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("chakra: live Rust index startup task failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let precise_provider = match chakra_provider_rust_analyzer::RustAnalyzerProvider::start(
        engine.provider_workspace(),
        chakra_provider_rust_analyzer::RustAnalyzerConfig::default(),
    ) {
        Ok(provider) => {
            let adapter: Arc<dyn chakra_engine::PreciseProvider> = provider.clone();
            if let Err(error) = engine.install_precise_provider(adapter) {
                tracing::warn!(%error, "precise provider was not installed");
                let _ = provider.shutdown();
                None
            } else {
                Some(provider)
            }
        }
        Err(error) => {
            tracing::warn!(%error, "precise provider could not start; syntax intelligence remains available");
            None
        }
    };
    let serve_result = chakra_mcp::serve_stdio(engine).await;
    if let Some(provider) = precise_provider {
        match tokio::task::spawn_blocking(move || provider.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "precise provider did not shut down cleanly");
            }
            Err(error) => {
                tracing::warn!(%error, "precise provider shutdown task failed");
            }
        }
    }
    match tokio::task::spawn_blocking(move || live.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("chakra: failed to stop live Rust index: {error}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("chakra: live Rust index shutdown task failed: {error}");
            return ExitCode::FAILURE;
        }
    }
    match serve_result {
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
