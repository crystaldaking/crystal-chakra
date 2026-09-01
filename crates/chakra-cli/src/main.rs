//! `chakra` — user-facing entry point of the Chakra code intelligence service.
//!
//! `chakra serve` indexes the materialized Git worktree, starts the live
//! reconciliation owner, then runs MCP over stdio (ADR-0003).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chakra_domain::indexing::{
    DEFAULT_MAX_INDEX_CALL_SITES, DEFAULT_MAX_INDEX_EDGES, DEFAULT_MAX_INDEX_FILES,
    DEFAULT_MAX_INDEX_SYMBOLS, DEFAULT_MAX_INDEX_WORKERS, DEFAULT_MAX_SOURCE_FILE_BYTES,
    DEFAULT_MAX_WORKSPACE_SOURCE_BYTES, DEFAULT_MEMORY_TARGET_BYTES, DEFAULT_STARTUP_TARGET_MILLIS,
    IndexBudgets, IndexCancellation,
};
use chakra_domain::symbol::Language;
use chakra_engine::PreciseProvider;
use chakra_provider_pool::{
    ProviderPool, ProviderPoolConfig, ProviderRegistration, ProviderStartError,
};
use chakra_workspace::{WorkspaceRegistry, WorkspaceRegistryConfig, WorkspaceStartOptions};
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
    /// Materialized Git worktree to serve.
    #[arg(long, value_name = "PATH", default_value = ".")]
    repo: PathBuf,

    /// Maximum watcher construction and initial registration time, in milliseconds.
    #[arg(long, default_value_t = 30_000)]
    live_index_startup_timeout_millis: u64,

    /// Run syntax-only and do not start the optional rust-analyzer provider.
    #[arg(long)]
    no_rust_analyzer: bool,

    /// rust-analyzer executable to use for optional precise enrichment.
    #[arg(long, value_name = "PATH", default_value = "rust-analyzer")]
    rust_analyzer_path: OsString,

    /// Run without the optional vtsls TypeScript/JavaScript provider.
    #[arg(long)]
    no_vtsls: bool,

    /// Explicit vtsls executable; omit to use bounded PATH/npm discovery.
    #[arg(long, value_name = "PATH")]
    vtsls_path: Option<OsString>,

    /// Run without the optional pyright Python provider.
    #[arg(long)]
    no_pyright: bool,

    /// Explicit pyright-langserver executable; omit for bounded discovery.
    #[arg(long, value_name = "PATH")]
    pyright_path: Option<OsString>,

    /// Run without the optional jdtls Java provider.
    #[arg(long)]
    no_jdtls: bool,

    /// Explicit jdtls executable; omit to use bounded PATH discovery.
    #[arg(long, value_name = "PATH")]
    jdtls_path: Option<OsString>,

    /// Maximum jdtls project-import readiness wait, in milliseconds.
    #[arg(long, default_value_t = 3 * 60 * 1_000_u64)]
    jdtls_readiness_timeout_millis: u64,

    /// Run without the optional csharp-ls C# provider.
    #[arg(long)]
    no_csharp_ls: bool,

    /// Explicit csharp-ls executable; omit for side-effect-free PATH discovery.
    #[arg(long, value_name = "PATH")]
    csharp_ls_path: Option<OsString>,

    /// Run without the optional bash-language-server Shell provider.
    #[arg(long)]
    no_bash_language_server: bool,

    /// Explicit bash-language-server executable; omit for PATH discovery.
    #[arg(long, value_name = "PATH")]
    bash_language_server_path: Option<OsString>,

    /// Run without the optional clangd C/C++ provider.
    #[arg(long)]
    no_clangd: bool,

    /// Explicit clangd executable; omit for side-effect-free PATH discovery.
    #[arg(long, value_name = "PATH")]
    clangd_path: Option<OsString>,

    /// Run without the optional terraform-ls HCL provider.
    #[arg(long)]
    no_terraform_ls: bool,

    /// Explicit terraform-ls executable; omit for side-effect-free PATH discovery.
    #[arg(long, value_name = "PATH")]
    terraform_ls_path: Option<OsString>,

    /// Run without the optional gopls Go provider.
    #[arg(long)]
    no_gopls: bool,

    /// Explicit gopls executable; omit for side-effect-free PATH discovery.
    #[arg(long, value_name = "PATH")]
    gopls_path: Option<OsString>,

    /// Maximum simultaneously active precise providers.
    #[arg(long, default_value_t = 3)]
    max_active_providers: usize,

    /// Maximum deterministic memory reservations for active providers.
    #[arg(long, default_value_t = 2 * 1024 * 1024 * 1024_u64)]
    max_provider_reserved_memory_bytes: u64,

    /// Maximum precise-provider queries admitted concurrently.
    #[arg(long, default_value_t = 4)]
    max_concurrent_provider_queries: usize,

    /// Maximum precise-provider queries waiting for admission.
    #[arg(long, default_value_t = 16)]
    max_queued_provider_queries: usize,

    /// Maximum queue wait before syntax fallback, in milliseconds.
    #[arg(long, default_value_t = 1_000)]
    provider_queue_timeout_millis: u64,

    /// Idle time before an inactive provider is stopped, in milliseconds.
    #[arg(long, default_value_t = 5 * 60 * 1_000_u64)]
    provider_idle_timeout_millis: u64,
    /// Maximum Git-discovered supported source files admitted to one revision.
    #[arg(long, default_value_t = DEFAULT_MAX_INDEX_FILES)]
    max_index_files: u64,

    /// Maximum bytes retained from one supported source file.
    #[arg(long, default_value_t = DEFAULT_MAX_SOURCE_FILE_BYTES)]
    max_source_file_bytes: u64,

    /// Maximum total supported source bytes retained by the syntax index.
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

    /// Maximum syntax parser workers; effective use is CPU/memory/phase bounded.
    #[arg(long, default_value_t = DEFAULT_MAX_INDEX_WORKERS)]
    max_index_workers: u64,
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

fn should_register_provider(disabled: bool) -> bool {
    !disabled
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
        live_index_startup_timeout_millis,
        no_rust_analyzer,
        rust_analyzer_path,
        no_vtsls,
        vtsls_path,
        no_pyright,
        pyright_path,
        no_jdtls,
        jdtls_path,
        jdtls_readiness_timeout_millis,
        no_csharp_ls,
        csharp_ls_path,
        no_bash_language_server,
        bash_language_server_path,
        no_clangd,
        clangd_path,
        no_terraform_ls,
        terraform_ls_path,
        no_gopls,
        gopls_path,
        max_active_providers,
        max_provider_reserved_memory_bytes,
        max_concurrent_provider_queries,
        max_queued_provider_queries,
        provider_queue_timeout_millis,
        provider_idle_timeout_millis,
        max_index_files,
        max_source_file_bytes,
        max_workspace_source_bytes,
        max_index_symbols,
        max_index_edges,
        max_index_call_sites,
        startup_target_millis,
        memory_target_bytes,
        max_index_workers,
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
        max_workers: max_index_workers,
    };
    let options = match chakra_language::IndexOptions::new(budgets, IndexCancellation::default()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("chakra: invalid index budget: {error}");
            return ExitCode::FAILURE;
        }
    };
    let registry = match WorkspaceRegistry::new(WorkspaceRegistryConfig { max_workspaces: 1 }) {
        Ok(registry) => Arc::new(registry),
        Err(error) => {
            eprintln!("chakra: invalid workspace registry configuration: {error}");
            return ExitCode::FAILURE;
        }
    };
    let start_registry = registry.clone();
    let registered = match tokio::task::spawn_blocking(move || {
        start_registry.register(
            &repo,
            WorkspaceStartOptions {
                index: options,
                live: chakra_language::LiveIndexOptions {
                    startup_timeout: Duration::from_millis(live_index_startup_timeout_millis),
                    ..chakra_language::LiveIndexOptions::default()
                },
            },
        )
    })
    .await
    {
        Ok(Ok(registered)) => registered,
        Ok(Err(error)) => {
            eprintln!("chakra: {error}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("chakra: workspace startup task failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let engine = registered.engine();
    let initial_metrics = registered.initial_metrics();
    tracing::info!(
        files = initial_metrics.parsed_files,
        rust_files = initial_metrics.rust_files,
        php_files = initial_metrics.php_files,
        cpp_files = initial_metrics.cpp_files,
        laravel_detected = initial_metrics.laravel_detected,
        framework_symbols = initial_metrics.framework_symbols,
        framework_edges = initial_metrics.framework_edges,
        framework_truncated_files = initial_metrics.framework_truncated_files,
        syntax_error_files = initial_metrics.syntax_error_files,
        truncated_call_sites = initial_metrics.truncated_call_sites,
        symbols = initial_metrics.symbols,
        edges = initial_metrics.edges,
        call_sites = initial_metrics.call_sites,
        indexing_degraded = initial_metrics.indexing.is_degraded(),
        source_bytes = initial_metrics.indexing.coverage.source_bytes,
        configured_index_workers = initial_metrics.indexing.scheduling.configured_max_workers,
        effective_index_workers = initial_metrics.indexing.scheduling.effective_worker_limit,
        peak_active_index_workers = initial_metrics.indexing.scheduling.peak_active_workers,
        current_rss_bytes = ?initial_metrics.indexing.memory.current_rss_bytes,
        observed_phase_peak_rss_bytes = ?initial_metrics.indexing.memory.observed_phase_peak_rss_bytes,
        elapsed_micros = initial_metrics.elapsed.as_micros(),
        "initial syntax revision published as stale pending live reconciliation"
    );
    let mut registrations = Vec::new();
    if should_register_provider(no_rust_analyzer) {
        let query_wait_budget = chakra_provider_rust_analyzer::DEFAULT_QUERY_WAIT_TIMEOUT;
        registrations.push(
            ProviderRegistration::new(
                "rust-analyzer",
                vec![Language::Rust],
                768 * 1024 * 1024,
                move |workspace,
                      _operation|
                      -> Result<Arc<dyn PreciseProvider>, ProviderStartError> {
                    let config = chakra_provider_rust_analyzer::RustAnalyzerConfig {
                        executable: rust_analyzer_path.clone(),
                        ..chakra_provider_rust_analyzer::RustAnalyzerConfig::default()
                    };
                    chakra_provider_rust_analyzer::RustAnalyzerProvider::start(workspace, config)
                        .map(|provider| provider as Arc<dyn PreciseProvider>)
                        .map_err(|error| ProviderStartError::new(error.to_string()))
                },
            )
            .with_additional_wait_budget(query_wait_budget),
        );
    } else {
        tracing::info!("rust-analyzer precise enrichment is disabled");
    }
    if should_register_provider(no_vtsls) {
        let command: OnceLock<chakra_provider_vtsls::VtslsCommand> = OnceLock::new();
        let discovery_budget = if vtsls_path.is_some() {
            Duration::ZERO
        } else {
            chakra_provider_vtsls::COMMAND_DISCOVERY_TIMEOUT
        };
        let query_wait_budget = chakra_provider_vtsls::DEFAULT_QUERY_WAIT_TIMEOUT;
        registrations.push(
            ProviderRegistration::new(
                "vtsls",
                vec![Language::TypeScript, Language::JavaScript],
                512 * 1024 * 1024,
                move |workspace,
                      operation|
                      -> Result<Arc<dyn PreciseProvider>, ProviderStartError> {
                    let resolved_command = if let Some(command) = command.get() {
                        command.clone()
                    } else {
                        let resolved = chakra_provider_vtsls::resolve_command_with_context(
                            vtsls_path.as_deref(),
                            operation,
                        )
                        .map_err(ProviderStartError::from)?;
                        let _ = command.set(resolved.clone());
                        resolved
                    };
                    let config = chakra_provider_vtsls::VtslsConfig {
                        command: resolved_command,
                        ..chakra_provider_vtsls::VtslsConfig::default()
                    };
                    chakra_provider_vtsls::VtslsProvider::start(workspace, config)
                        .map(|provider| provider as Arc<dyn PreciseProvider>)
                        .map_err(|error| ProviderStartError::new(error.to_string()))
                },
            )
            .with_additional_wait_budget(discovery_budget.saturating_add(query_wait_budget)),
        );
    } else {
        tracing::info!("vtsls precise enrichment is disabled");
    }
    if should_register_provider(no_pyright) {
        let command: OnceLock<chakra_provider_pyright::PyrightCommand> = OnceLock::new();
        let discovery_budget = if pyright_path.is_some() {
            Duration::ZERO
        } else {
            chakra_provider_pyright::COMMAND_DISCOVERY_TIMEOUT
        };
        let query_wait_budget = chakra_provider_pyright::DEFAULT_QUERY_WAIT_TIMEOUT;
        registrations.push(
            ProviderRegistration::new(
                "pyright",
                vec![Language::Python],
                512 * 1024 * 1024,
                move |workspace,
                      operation|
                      -> Result<Arc<dyn PreciseProvider>, ProviderStartError> {
                    let resolved_command = if let Some(command) = command.get() {
                        command.clone()
                    } else {
                        let resolved = chakra_provider_pyright::resolve_command_with_context(
                            pyright_path.as_deref(),
                            operation,
                        )
                        .map_err(ProviderStartError::from)?;
                        let _ = command.set(resolved.clone());
                        resolved
                    };
                    let config = chakra_provider_pyright::PyrightConfig {
                        command: resolved_command,
                        ..chakra_provider_pyright::PyrightConfig::default()
                    };
                    chakra_provider_pyright::PyrightProvider::start(workspace, config)
                        .map(|provider| provider as Arc<dyn PreciseProvider>)
                        .map_err(|error| ProviderStartError::new(error.to_string()))
                },
            )
            .with_additional_wait_budget(discovery_budget.saturating_add(query_wait_budget)),
        );
    } else {
        tracing::info!("pyright precise enrichment is disabled");
    }
    if should_register_provider(no_jdtls) {
        let command: OnceLock<chakra_provider_jdtls::JdtlsCommand> = OnceLock::new();
        let query_wait_budget = chakra_provider_jdtls::DEFAULT_QUERY_WAIT_TIMEOUT;
        registrations.push(
            ProviderRegistration::new(
                "jdtls",
                vec![Language::Java],
                1024 * 1024 * 1024,
                move |workspace,
                      operation|
                      -> Result<Arc<dyn PreciseProvider>, ProviderStartError> {
                    let resolved_command = if let Some(command) = command.get() {
                        command.clone()
                    } else {
                        let resolved = chakra_provider_jdtls::resolve_command_with_context(
                            jdtls_path.as_deref(),
                            &workspace.repository_root,
                            operation,
                        )
                        .map_err(ProviderStartError::from)?;
                        let _ = command.set(resolved.clone());
                        resolved
                    };
                    let config = chakra_provider_jdtls::JdtlsConfig {
                        command: resolved_command,
                        readiness_timeout: Duration::from_millis(jdtls_readiness_timeout_millis),
                        ..chakra_provider_jdtls::JdtlsConfig::default()
                    };
                    chakra_provider_jdtls::JdtlsProvider::start(workspace, config)
                        .map(|provider| provider as Arc<dyn PreciseProvider>)
                        .map_err(|error| ProviderStartError::new(error.to_string()))
                },
            )
            .with_additional_wait_budget(query_wait_budget),
        );
    } else {
        tracing::info!("jdtls precise enrichment is disabled");
    }
    if should_register_provider(no_csharp_ls) {
        let command: OnceLock<chakra_provider_csharp_ls::CsharpLsCommand> = OnceLock::new();
        let query_wait_budget = chakra_provider_csharp_ls::DEFAULT_QUERY_WAIT_TIMEOUT;
        registrations.push(
            ProviderRegistration::new(
                "csharp-ls",
                vec![Language::CSharp],
                1024 * 1024 * 1024,
                move |workspace,
                      operation|
                      -> Result<Arc<dyn PreciseProvider>, ProviderStartError> {
                    let resolved_command = if let Some(command) = command.get() {
                        command.clone()
                    } else {
                        let resolved = chakra_provider_csharp_ls::resolve_command_with_context(
                            csharp_ls_path.as_deref(),
                            operation,
                        )
                        .map_err(ProviderStartError::from)?;
                        let _ = command.set(resolved.clone());
                        resolved
                    };
                    let config = chakra_provider_csharp_ls::CsharpLsConfig {
                        command: resolved_command,
                        ..chakra_provider_csharp_ls::CsharpLsConfig::default()
                    };
                    chakra_provider_csharp_ls::CsharpLsProvider::start(workspace, config)
                        .map(|provider| provider as Arc<dyn PreciseProvider>)
                        .map_err(|error| ProviderStartError::new(error.to_string()))
                },
            )
            .with_additional_wait_budget(query_wait_budget),
        );
    } else {
        tracing::info!("csharp-ls precise enrichment is disabled");
    }
    if should_register_provider(no_bash_language_server) {
        let command: OnceLock<chakra_provider_bash_language_server::BashLanguageServerCommand> =
            OnceLock::new();
        let query_wait_budget = chakra_provider_bash_language_server::DEFAULT_QUERY_WAIT_TIMEOUT;
        registrations.push(
            ProviderRegistration::new(
                "bash-language-server",
                vec![Language::Shell],
                512 * 1024 * 1024,
                move |workspace,
                      operation|
                      -> Result<Arc<dyn PreciseProvider>, ProviderStartError> {
                    let resolved_command = if let Some(command) = command.get() {
                        command.clone()
                    } else {
                        let resolved =
                            chakra_provider_bash_language_server::resolve_command_with_context(
                                bash_language_server_path.as_deref(),
                                operation,
                            )
                            .map_err(ProviderStartError::from)?;
                        let _ = command.set(resolved.clone());
                        resolved
                    };
                    let config = chakra_provider_bash_language_server::BashLanguageServerConfig {
                        command: resolved_command,
                        ..chakra_provider_bash_language_server::BashLanguageServerConfig::default()
                    };
                    chakra_provider_bash_language_server::BashLanguageServerProvider::start(
                        workspace, config,
                    )
                    .map(|provider| provider as Arc<dyn PreciseProvider>)
                    .map_err(|error| ProviderStartError::new(error.to_string()))
                },
            )
            .with_additional_wait_budget(query_wait_budget),
        );
    } else {
        tracing::info!("bash-language-server precise enrichment is disabled");
    }
    if should_register_provider(no_clangd) {
        let command: OnceLock<chakra_provider_clangd::ClangdCommand> = OnceLock::new();
        let query_wait_budget = chakra_provider_clangd::DEFAULT_QUERY_WAIT_TIMEOUT;
        registrations.push(
            ProviderRegistration::new(
                "clangd",
                vec![Language::Cpp],
                2 * 1024 * 1024 * 1024,
                move |workspace,
                      operation|
                      -> Result<Arc<dyn PreciseProvider>, ProviderStartError> {
                    let resolved_command = if let Some(command) = command.get() {
                        command.clone()
                    } else {
                        let resolved = chakra_provider_clangd::resolve_command_with_context(
                            clangd_path.as_deref(),
                            operation,
                        )
                        .map_err(ProviderStartError::from)?;
                        let _ = command.set(resolved.clone());
                        resolved
                    };
                    let config = chakra_provider_clangd::ClangdConfig {
                        command: resolved_command,
                        ..chakra_provider_clangd::ClangdConfig::default()
                    };
                    chakra_provider_clangd::ClangdProvider::start(workspace, config)
                        .map(|provider| provider as Arc<dyn PreciseProvider>)
                        .map_err(|error| ProviderStartError::new(error.to_string()))
                },
            )
            .with_additional_wait_budget(query_wait_budget),
        );
    } else {
        tracing::info!("clangd precise enrichment is disabled");
    }
    if should_register_provider(no_terraform_ls) {
        let command: OnceLock<chakra_provider_terraform_ls::TerraformLsCommand> = OnceLock::new();
        let query_wait_budget = chakra_provider_terraform_ls::DEFAULT_QUERY_WAIT_TIMEOUT;
        registrations.push(
            ProviderRegistration::new(
                "terraform-ls",
                vec![Language::Hcl],
                512 * 1024 * 1024,
                move |workspace,
                      operation|
                      -> Result<Arc<dyn PreciseProvider>, ProviderStartError> {
                    let resolved_command = if let Some(command) = command.get() {
                        command.clone()
                    } else {
                        let resolved = chakra_provider_terraform_ls::resolve_command_with_context(
                            terraform_ls_path.as_deref(),
                            operation,
                        )
                        .map_err(ProviderStartError::from)?;
                        let _ = command.set(resolved.clone());
                        resolved
                    };
                    let config = chakra_provider_terraform_ls::TerraformLsConfig {
                        command: resolved_command,
                        ..chakra_provider_terraform_ls::TerraformLsConfig::default()
                    };
                    chakra_provider_terraform_ls::TerraformLsProvider::start(workspace, config)
                        .map(|provider| provider as Arc<dyn PreciseProvider>)
                        .map_err(|error| ProviderStartError::new(error.to_string()))
                },
            )
            .with_additional_wait_budget(query_wait_budget)
            .with_path_filter(chakra_provider_terraform_ls::supports_path),
        );
    } else {
        tracing::info!("terraform-ls precise enrichment is disabled");
    }
    if should_register_provider(no_gopls) {
        let command: OnceLock<chakra_provider_gopls::GoplsCommand> = OnceLock::new();
        let query_wait_budget = chakra_provider_gopls::DEFAULT_QUERY_WAIT_TIMEOUT;
        registrations.push(
            ProviderRegistration::new(
                "gopls",
                vec![Language::Go],
                768 * 1024 * 1024,
                move |workspace,
                      operation|
                      -> Result<Arc<dyn PreciseProvider>, ProviderStartError> {
                    let resolved_command = if let Some(command) = command.get() {
                        command.clone()
                    } else {
                        let resolved = chakra_provider_gopls::resolve_command_with_context(
                            gopls_path.as_deref(),
                            operation,
                        )
                        .map_err(ProviderStartError::from)?;
                        let _ = command.set(resolved.clone());
                        resolved
                    };
                    let config = chakra_provider_gopls::GoplsConfig {
                        command: resolved_command,
                        ..chakra_provider_gopls::GoplsConfig::default()
                    };
                    chakra_provider_gopls::GoplsProvider::start(workspace, config)
                        .map(|provider| provider as Arc<dyn PreciseProvider>)
                        .map_err(|error| ProviderStartError::new(error.to_string()))
                },
            )
            .with_additional_wait_budget(query_wait_budget),
        );
    } else {
        tracing::info!("gopls precise enrichment is disabled");
    }
    let provider_pool = match ProviderPool::start(
        ProviderPoolConfig {
            max_active_providers,
            max_reserved_memory_bytes: max_provider_reserved_memory_bytes,
            max_concurrent_queries: max_concurrent_provider_queries,
            max_queued_queries: max_queued_provider_queries,
            query_queue_timeout: Duration::from_millis(provider_queue_timeout_millis),
            idle_timeout: Duration::from_millis(provider_idle_timeout_millis),
            ..ProviderPoolConfig::default()
        },
        registrations,
    ) {
        Ok(pool) => pool,
        Err(error) => {
            eprintln!("chakra: invalid precise-provider pool: {error}");
            let _ = tokio::task::spawn_blocking(move || registry.shutdown()).await;
            return ExitCode::FAILURE;
        }
    };
    for provider in provider_pool.providers() {
        if let Err(error) = engine.install_precise_provider(provider) {
            eprintln!("chakra: failed to install precise provider: {error}");
            let _ = tokio::task::spawn_blocking(move || provider_pool.shutdown()).await;
            let _ = tokio::task::spawn_blocking(move || registry.shutdown()).await;
            return ExitCode::FAILURE;
        }
    }
    let serve_result = chakra_mcp::serve_stdio_router(registry.clone()).await;
    match tokio::task::spawn_blocking(move || provider_pool.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "precise-provider pool did not shut down cleanly");
        }
        Err(error) => {
            tracing::warn!(%error, "precise-provider pool shutdown task failed");
        }
    }
    match tokio::task::spawn_blocking(move || registry.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("chakra: failed to stop workspace registry: {error}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("chakra: workspace registry shutdown task failed: {error}");
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
    fn serve_budget_help_is_language_neutral() -> Result<(), Box<dyn std::error::Error>> {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("serve")
            .ok_or("serve subcommand must exist")?
            .render_long_help()
            .to_string();
        assert!(help.contains("supported source files"));
        assert!(help.contains("supported source bytes"));
        assert!(!help.contains("Rust/PHP"));
        Ok(())
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
                && args.live_index_startup_timeout_millis == 30_000
                && !args.no_rust_analyzer
                && args.rust_analyzer_path == "rust-analyzer"
                && !args.no_vtsls
                && args.vtsls_path.is_none()
                && !args.no_pyright
                && args.pyright_path.is_none()
                && !args.no_jdtls
                && args.jdtls_path.is_none()
                && args.jdtls_readiness_timeout_millis == 180_000
                && !args.no_csharp_ls
                && args.csharp_ls_path.is_none()
                && !args.no_bash_language_server
                && args.bash_language_server_path.is_none()
                && !args.no_clangd
                && args.clangd_path.is_none()
                && !args.no_terraform_ls
                && args.terraform_ls_path.is_none()
                && !args.no_gopls
                && args.gopls_path.is_none()
                && args.max_active_providers == 3
                && args.max_index_files == DEFAULT_MAX_INDEX_FILES
                && args.max_index_symbols == DEFAULT_MAX_INDEX_SYMBOLS
                && args.max_index_workers == DEFAULT_MAX_INDEX_WORKERS
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
            "--no-vtsls",
            "--vtsls-path",
            "/opt/bin/vtsls",
            "--no-pyright",
            "--pyright-path",
            "/opt/bin/pyright-langserver",
            "--no-jdtls",
            "--jdtls-path",
            "/opt/bin/jdtls",
            "--jdtls-readiness-timeout-millis",
            "240000",
            "--no-csharp-ls",
            "--csharp-ls-path",
            "/opt/bin/csharp-ls",
            "--no-bash-language-server",
            "--bash-language-server-path",
            "/opt/bin/bash-language-server",
            "--no-clangd",
            "--clangd-path",
            "/opt/bin/clangd",
            "--no-terraform-ls",
            "--terraform-ls-path",
            "/opt/bin/terraform-ls",
            "--no-gopls",
            "--gopls-path",
            "/opt/bin/gopls",
        ]);
        assert!(matches!(
            cli,
            Ok(Cli {
                command: Some(Commands::Serve(ref args)),
            }) if args.no_rust_analyzer
                && args.rust_analyzer_path == "/opt/bin/rust-analyzer"
                && args.no_vtsls
                && args.vtsls_path.as_deref() == Some(std::ffi::OsStr::new("/opt/bin/vtsls"))
                && args.no_pyright
                && args.pyright_path.as_deref()
                    == Some(std::ffi::OsStr::new("/opt/bin/pyright-langserver"))
                && args.no_jdtls
                && args.jdtls_path.as_deref()
                    == Some(std::ffi::OsStr::new("/opt/bin/jdtls"))
                && args.jdtls_readiness_timeout_millis == 240_000
                && args.no_csharp_ls
                && args.csharp_ls_path.as_deref()
                    == Some(std::ffi::OsStr::new("/opt/bin/csharp-ls"))
                && args.no_bash_language_server
                && args.bash_language_server_path.as_deref()
                    == Some(std::ffi::OsStr::new("/opt/bin/bash-language-server"))
                && args.no_clangd
                && args.clangd_path.as_deref()
                    == Some(std::ffi::OsStr::new("/opt/bin/clangd"))
                && args.no_terraform_ls
                && args.terraform_ls_path.as_deref()
                    == Some(std::ffi::OsStr::new("/opt/bin/terraform-ls"))
                && args.no_gopls
                && args.gopls_path.as_deref()
                    == Some(std::ffi::OsStr::new("/opt/bin/gopls"))
        ));
    }

    #[test]
    fn serve_accepts_an_explicit_index_worker_limit() {
        let cli = Cli::try_parse_from(["chakra", "serve", "--max-index-workers", "2"]);
        assert!(matches!(
            cli,
            Ok(Cli {
                command: Some(Commands::Serve(ref args)),
            }) if args.max_index_workers == 2
        ));
    }

    #[test]
    fn provider_registration_policy_is_independent_of_startup_inventory() {
        assert!(should_register_provider(false));
        assert!(!should_register_provider(true));
    }
}
