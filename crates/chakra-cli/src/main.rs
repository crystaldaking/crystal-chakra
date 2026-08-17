//! `chakra` — user-facing entry point of the Chakra code intelligence service.
//!
//! `chakra serve` indexes the materialized Git worktree, starts the live
//! reconciliation owner, then runs MCP over stdio (ADR-0003).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use chakra_domain::indexing::{
    DEFAULT_MAX_INDEX_CALL_SITES, DEFAULT_MAX_INDEX_EDGES, DEFAULT_MAX_INDEX_FILES,
    DEFAULT_MAX_INDEX_SYMBOLS, DEFAULT_MAX_SOURCE_FILE_BYTES, DEFAULT_MAX_WORKSPACE_SOURCE_BYTES,
    DEFAULT_MEMORY_TARGET_BYTES, DEFAULT_STARTUP_TARGET_MILLIS, IndexBudgets, IndexCancellation,
};
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

    /// Run syntax-only and do not start the optional rust-analyzer provider.
    #[arg(long)]
    no_rust_analyzer: bool,

    /// rust-analyzer executable to use for optional precise enrichment.
    #[arg(long, value_name = "PATH", default_value = "rust-analyzer")]
    rust_analyzer_path: OsString,

    /// Maximum Git-discovered Rust/PHP files admitted to one revision.
    #[arg(long, default_value_t = DEFAULT_MAX_INDEX_FILES)]
    max_index_files: u64,

    /// Maximum bytes retained from one Rust/PHP source file.
    #[arg(long, default_value_t = DEFAULT_MAX_SOURCE_FILE_BYTES)]
    max_source_file_bytes: u64,

    /// Maximum total Rust/PHP source bytes retained by the syntax index.
    #[arg(long, default_value_t = DEFAULT_MAX_WORKSPACE_SOURCE_BYTES)]
    max_workspace_source_bytes: u64,

    /// Maximum declarations retained in the published graph.
    #[arg(long, default_value_t = DEFAULT_MAX_INDEX_SYMBOLS)]
    max_index_symbols: u64,

    /// Maximum relationships retained in the published graph.
    #[arg(long, default_value_t = DEFAULT_MAX_INDEX_EDGES)]
    max_index_edges: u64,

    /// Maximum compact syntax call sites retained in the published graph.
    #[arg(long, default_value_t = DEFAULT_MAX_INDEX_CALL_SITES)]
    max_index_call_sites: u64,

    /// Observable cold-start target in milliseconds; it never changes graph contents.
    #[arg(long, default_value_t = DEFAULT_STARTUP_TARGET_MILLIS)]
    startup_target_millis: u64,

    /// Observable current/phase-sampled resident-memory target in bytes.
    #[arg(long, default_value_t = DEFAULT_MEMORY_TARGET_BYTES)]
    memory_target_bytes: u64,
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

fn should_start_rust_analyzer(disabled: bool, has_rust_sources: bool) -> bool {
    !disabled && has_rust_sources
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
    let ServeArgs {
        repo,
        no_rust_analyzer,
        rust_analyzer_path,
        max_index_files,
        max_source_file_bytes,
        max_workspace_source_bytes,
        max_index_symbols,
        max_index_edges,
        max_index_call_sites,
        startup_target_millis,
        memory_target_bytes,
    } = args;
    let budgets = IndexBudgets {
        max_files: max_index_files,
        max_source_file_bytes,
        max_workspace_source_bytes,
        max_symbols: max_index_symbols,
        max_edges: max_index_edges,
        max_call_sites: max_index_call_sites,
        startup_target_millis,
        memory_target_bytes,
    };
    let options = match chakra_language::IndexOptions::new(budgets, IndexCancellation::default()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("chakra: invalid index budget: {error}");
            return ExitCode::FAILURE;
        }
    };
    let report = match tokio::task::spawn_blocking(move || {
        chakra_language::index_repository_with_options(&repo, options)
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
    let identity = match chakra_git::resolve_workspace_identity(&report.repository_root) {
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
    update.set_indexing(report.metrics.indexing.clone());
    update.set_status(WorkspaceStatus::Indexing);
    // The watcher is not active yet, so the initial scan cannot close the
    // startup race. The live owner reclaims freshness after it starts
    // watching and performs its mandatory reconciliation.
    update.set_freshness(Freshness::Stale);
    let publication_started = Instant::now();
    if let Err(error) = engine.publish(update) {
        eprintln!("chakra: failed to publish initial syntax index: {error}");
        return ExitCode::FAILURE;
    }
    tracing::info!(
        elapsed_micros = publication_started.elapsed().as_micros(),
        "initial syntax revision publication completed"
    );
    let initial_metrics = report.metrics;
    let has_rust_sources = initial_metrics.rust_files > 0;
    tracing::info!(
        files = initial_metrics.parsed_files,
        rust_files = initial_metrics.rust_files,
        php_files = initial_metrics.php_files,
        syntax_error_files = initial_metrics.syntax_error_files,
        truncated_call_sites = initial_metrics.truncated_call_sites,
        symbols = initial_metrics.symbols,
        edges = initial_metrics.edges,
        call_sites = initial_metrics.call_sites,
        indexing_degraded = initial_metrics.indexing.is_degraded(),
        source_bytes = initial_metrics.indexing.coverage.source_bytes,
        current_rss_bytes = ?initial_metrics.indexing.memory.current_rss_bytes,
        observed_phase_peak_rss_bytes = ?initial_metrics.indexing.memory.observed_phase_peak_rss_bytes,
        elapsed_micros = initial_metrics.elapsed.as_micros(),
        "initial Rust/PHP syntax revision published as stale pending live reconciliation"
    );
    let repository_root = report.repository_root;
    let syntax_index = report.syntax_index;
    let live_engine = engine.clone();
    let live = match tokio::task::spawn_blocking(move || {
        chakra_language::start_live_index(repository_root, syntax_index, live_engine)
    })
    .await
    {
        Ok(Ok(live)) => live,
        Ok(Err(error)) => {
            eprintln!("chakra: failed to start live syntax index: {error}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("chakra: live syntax index startup task failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let precise_provider = if !should_start_rust_analyzer(no_rust_analyzer, has_rust_sources) {
        if no_rust_analyzer {
            tracing::info!("rust-analyzer precise enrichment is disabled");
        } else {
            tracing::info!(
                "rust-analyzer was not started because the workspace has no Rust sources"
            );
        }
        None
    } else {
        let config = chakra_provider_rust_analyzer::RustAnalyzerConfig {
            executable: rust_analyzer_path,
            ..chakra_provider_rust_analyzer::RustAnalyzerConfig::default()
        };
        match chakra_provider_rust_analyzer::RustAnalyzerProvider::start(
            engine.provider_workspace(),
            config,
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
            eprintln!("chakra: failed to stop live syntax index: {error}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("chakra: live syntax index shutdown task failed: {error}");
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
                && !args.no_rust_analyzer
                && args.rust_analyzer_path == "rust-analyzer"
                && args.max_index_files == DEFAULT_MAX_INDEX_FILES
                && args.max_index_symbols == DEFAULT_MAX_INDEX_SYMBOLS
        ));
    }

    #[test]
    fn serve_accepts_precise_provider_controls() {
        let cli = Cli::try_parse_from([
            "chakra",
            "serve",
            "--no-rust-analyzer",
            "--rust-analyzer-path",
            "/opt/bin/rust-analyzer",
        ]);
        assert!(matches!(
            cli,
            Ok(Cli {
                command: Some(Commands::Serve(ref args)),
            }) if args.no_rust_analyzer
                && args.rust_analyzer_path == "/opt/bin/rust-analyzer"
        ));
    }

    #[test]
    fn rust_analyzer_start_policy_requires_rust_sources_and_permission() {
        assert!(should_start_rust_analyzer(false, true));
        assert!(!should_start_rust_analyzer(false, false));
        assert!(!should_start_rust_analyzer(true, true));
    }
}
