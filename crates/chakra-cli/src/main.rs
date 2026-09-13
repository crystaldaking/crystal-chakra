//! `chakra` — user-facing entry point of the Chakra code intelligence service.
//!
//! `chakra serve` indexes the materialized Git worktree, starts the live
//! reconciliation owner, then runs MCP over stdio (ADR-0003).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chakra_domain::indexing::IndexCancellation;
use chakra_domain::symbol::Language;
use chakra_engine::PreciseProvider;
use chakra_provider_pool::{
    ProviderPool, ProviderPoolConfig, ProviderRegistration, ProviderStartError,
};
use chakra_workspace::{WorkspaceRegistry, WorkspaceRegistryConfig, WorkspaceStartOptions};
use clap::{Args, CommandFactory, Parser, Subcommand};

mod config;
mod update;

use config::{ConfigLayers, ConfigSource, EffectiveConfig, ProviderKey};

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
    Serve(Box<ServeArgs>),
    /// Inspect the effective configuration (ADR-0053).
    Config(ConfigArgs),
    /// Check GitHub for a newer stable Chakra release (ADR-0054).
    Update(UpdateArgs),
}

#[derive(Debug, Args)]
struct UpdateArgs {
    /// Query the latest stable release and report the result. Exit status:
    /// 0 up to date, 1 check unavailable, 2 update available (ADR-0054).
    #[arg(long, required = true)]
    check: bool,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommands,
}

#[derive(Debug, Subcommand)]
enum ConfigCommands {
    /// Print the effective configuration and the source layer of each key.
    Show(ConfigShowArgs),
}

#[derive(Debug, Args)]
struct ConfigShowArgs {
    /// Worktree whose configuration to inspect.
    #[arg(long, value_name = "PATH", default_value = ".")]
    repo: PathBuf,

    /// Explicit shared configuration file; disables chakra.toml discovery.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Materialized Git worktree to serve; repeat for linked worktrees.
    #[arg(long, value_name = "PATH", default_value = ".")]
    repo: Vec<PathBuf>,

    /// Explicit shared configuration file; disables chakra.toml discovery.
    /// The private override is its chakra.local.toml sibling (ADR-0053).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Maximum linked worktrees admitted to this process.
    #[arg(long)]
    max_workspaces: Option<usize>,

    /// Maximum watcher construction and initial registration time, in milliseconds.
    #[arg(long)]
    live_index_startup_timeout_millis: Option<u64>,

    /// Run syntax-only and do not start the optional rust-analyzer provider.
    #[arg(long)]
    no_rust_analyzer: bool,

    /// rust-analyzer executable to use for optional precise enrichment.
    #[arg(long, value_name = "PATH")]
    rust_analyzer_path: Option<OsString>,

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
    #[arg(long)]
    jdtls_readiness_timeout_millis: Option<u64>,

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
    #[arg(long)]
    max_active_providers: Option<usize>,

    /// Maximum simultaneously active precise providers in one worktree.
    #[arg(long)]
    max_active_providers_per_workspace: Option<usize>,

    /// Maximum deterministic memory reservations for active providers.
    #[arg(long)]
    max_provider_reserved_memory_bytes: Option<u64>,

    /// Maximum deterministic provider-memory reservations in one worktree.
    #[arg(long)]
    max_provider_reserved_memory_bytes_per_workspace: Option<u64>,

    /// Maximum precise-provider queries admitted concurrently.
    #[arg(long)]
    max_concurrent_provider_queries: Option<usize>,

    /// Maximum precise-provider queries waiting for admission.
    #[arg(long)]
    max_queued_provider_queries: Option<usize>,

    /// Maximum queue wait before syntax fallback, in milliseconds.
    #[arg(long)]
    provider_queue_timeout_millis: Option<u64>,

    /// Idle time before an inactive provider is stopped, in milliseconds.
    #[arg(long)]
    provider_idle_timeout_millis: Option<u64>,

    /// Maximum Git-discovered supported source files admitted to one revision.
    #[arg(long)]
    max_index_files: Option<u64>,

    /// Maximum bytes retained from one supported source file.
    #[arg(long)]
    max_source_file_bytes: Option<u64>,

    /// Maximum total supported source bytes retained by the syntax index.
    #[arg(long)]
    max_workspace_source_bytes: Option<u64>,

    /// Maximum declarations retained in the published graph.
    #[arg(long)]
    max_index_symbols: Option<u64>,

    /// Maximum relationships retained in the published graph.
    #[arg(long)]
    max_index_edges: Option<u64>,

    /// Maximum compact syntax call sites retained in the published graph.
    #[arg(long)]
    max_index_call_sites: Option<u64>,

    /// Observable cold-start target in milliseconds; it never changes graph contents.
    #[arg(long)]
    startup_target_millis: Option<u64>,

    /// Observable current/phase-sampled resident-memory target in bytes.
    #[arg(long)]
    memory_target_bytes: Option<u64>,

    /// Maximum syntax parser workers; effective use is CPU/memory/phase bounded.
    #[arg(long)]
    max_index_workers: Option<u64>,
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
        Some(Commands::Serve(args)) => serve(*args).await,
        Some(Commands::Config(args)) => config_command(args),
        Some(Commands::Update(args)) => {
            let _ = args;
            ExitCode::from(update::run_manual_check(update::GITHUB_API_BASE))
        }
    }
}

/// Resolve the configuration layers for the primary worktree (ADR-0053).
///
/// Discovery anchors at the Git worktree root of `repo`; when the path is not
/// a Git worktree the given directory itself is the configuration base and
/// workspace registration reports the repository error later. A missing
/// `chakra.toml` contributes no layer and keeps built-in defaults.
fn resolve_config(
    repo: &std::path::Path,
    explicit: Option<&std::path::Path>,
) -> Result<ConfigLayers, config::ConfigError> {
    let root = match explicit {
        Some(_) => repo.to_owned(),
        None => chakra_git::resolve_repository_root(repo).unwrap_or_else(|_| repo.to_owned()),
    };
    ConfigLayers::load(&root, explicit)
}

/// Apply explicit CLI options as the final precedence layer (ADR-0053).
/// Absent flags contribute nothing, so parser defaults can never override
/// configured values.
fn apply_cli_overrides(effective: &mut EffectiveConfig, args: &ServeArgs) {
    if let Some(value) = args.max_workspaces {
        effective.max_workspaces = value;
        effective.record_cli_override("startup.max_workspaces");
    }
    if let Some(value) = args.live_index_startup_timeout_millis {
        effective.live_index_startup_timeout_millis = value;
        effective.record_cli_override("startup.live_index_startup_timeout_millis");
    }
    if let Some(value) = args.max_active_providers {
        effective.max_active_providers = value;
        effective.record_cli_override("providers.max_active");
    }
    if let Some(value) = args.max_active_providers_per_workspace {
        effective.max_active_providers_per_workspace = value;
        effective.record_cli_override("providers.max_active_per_workspace");
    }
    if let Some(value) = args.max_provider_reserved_memory_bytes {
        effective.max_provider_reserved_memory_bytes = value;
        effective.record_cli_override("providers.max_reserved_memory_bytes");
    }
    if let Some(value) = args.max_provider_reserved_memory_bytes_per_workspace {
        effective.max_provider_reserved_memory_bytes_per_workspace = value;
        effective.record_cli_override("providers.max_reserved_memory_bytes_per_workspace");
    }
    if let Some(value) = args.max_concurrent_provider_queries {
        effective.max_concurrent_provider_queries = value;
        effective.record_cli_override("providers.max_concurrent_queries");
    }
    if let Some(value) = args.max_queued_provider_queries {
        effective.max_queued_provider_queries = value;
        effective.record_cli_override("providers.max_queued_queries");
    }
    if let Some(value) = args.provider_queue_timeout_millis {
        effective.provider_queue_timeout_millis = value;
        effective.record_cli_override("providers.queue_timeout_millis");
    }
    if let Some(value) = args.provider_idle_timeout_millis {
        effective.provider_idle_timeout_millis = value;
        effective.record_cli_override("providers.idle_timeout_millis");
    }
    if let Some(value) = args.jdtls_readiness_timeout_millis {
        effective.jdtls_readiness_timeout_millis = value;
        effective.record_cli_override("providers.jdtls_readiness_timeout_millis");
    }
    if let Some(value) = args.max_index_files {
        effective.budgets.max_files = value;
        effective.record_cli_override("index.max_files");
    }
    if let Some(value) = args.max_source_file_bytes {
        effective.budgets.max_source_file_bytes = value;
        effective.record_cli_override("index.max_source_file_bytes");
    }
    if let Some(value) = args.max_workspace_source_bytes {
        effective.budgets.max_workspace_source_bytes = value;
        effective.record_cli_override("index.max_workspace_source_bytes");
    }
    if let Some(value) = args.max_index_symbols {
        effective.budgets.max_symbols = value;
        effective.record_cli_override("index.max_symbols");
    }
    if let Some(value) = args.max_index_edges {
        effective.budgets.max_edges = value;
        effective.record_cli_override("index.max_edges");
    }
    if let Some(value) = args.max_index_call_sites {
        effective.budgets.max_call_sites = value;
        effective.record_cli_override("index.max_call_sites");
    }
    if let Some(value) = args.startup_target_millis {
        effective.budgets.startup_target_millis = value;
        effective.record_cli_override("index.startup_target_millis");
    }
    if let Some(value) = args.memory_target_bytes {
        effective.budgets.memory_target_bytes = value;
        effective.record_cli_override("index.memory_target_bytes");
    }
    if let Some(value) = args.max_index_workers {
        effective.budgets.max_workers = value;
        effective.record_cli_override("index.max_workers");
    }
    let disables = [
        (ProviderKey::RustAnalyzer, args.no_rust_analyzer),
        (ProviderKey::Vtsls, args.no_vtsls),
        (ProviderKey::Pyright, args.no_pyright),
        (ProviderKey::Jdtls, args.no_jdtls),
        (ProviderKey::CsharpLs, args.no_csharp_ls),
        (
            ProviderKey::BashLanguageServer,
            args.no_bash_language_server,
        ),
        (ProviderKey::Clangd, args.no_clangd),
        (ProviderKey::TerraformLs, args.no_terraform_ls),
        (ProviderKey::Gopls, args.no_gopls),
    ];
    for (key, disabled) in disables {
        if disabled {
            effective.provider_mut(key).enabled = false;
            effective.record_cli_override(&format!("providers.{}.enabled", key.as_str()));
        }
    }
    let paths = [
        (ProviderKey::RustAnalyzer, &args.rust_analyzer_path),
        (ProviderKey::Vtsls, &args.vtsls_path),
        (ProviderKey::Pyright, &args.pyright_path),
        (ProviderKey::Jdtls, &args.jdtls_path),
        (ProviderKey::CsharpLs, &args.csharp_ls_path),
        (
            ProviderKey::BashLanguageServer,
            &args.bash_language_server_path,
        ),
        (ProviderKey::Clangd, &args.clangd_path),
        (ProviderKey::TerraformLs, &args.terraform_ls_path),
        (ProviderKey::Gopls, &args.gopls_path),
    ];
    for (key, value) in paths {
        if let Some(path) = value {
            effective.provider_mut(key).path = Some(PathBuf::from(path));
            effective.record_cli_override(&format!("providers.{}.path", key.as_str()));
        }
    }
}

/// Resolve the workspace-scoped settings (index budgets, live startup
/// timeout) for one registered worktree.
///
/// Every worktree reads its own checked-out `chakra.toml` (ADR-0053). An
/// explicit `--config` disables discovery and applies to every registered
/// worktree; explicit CLI options always win. Process-global settings
/// (provider pool, provider enablement, registry limits) are not resolved
/// here — they come from the primary worktree's configuration.
fn workspace_scoped_settings(
    effective: &EffectiveConfig,
    args: &ServeArgs,
    root: &std::path::Path,
    primary_repo: &std::path::Path,
) -> Result<(chakra_domain::indexing::IndexBudgets, u64), config::ConfigError> {
    if args.config.is_some() || root == primary_repo {
        return Ok((
            effective.budgets,
            effective.live_index_startup_timeout_millis,
        ));
    }
    let workspace_layers = resolve_config(root, None)?;
    let mut workspace_effective = workspace_layers.merge();
    apply_cli_overrides(&mut workspace_effective, args);
    Ok((
        workspace_effective.budgets,
        workspace_effective.live_index_startup_timeout_millis,
    ))
}

fn push_rendered(
    out: &mut String,
    effective: &EffectiveConfig,
    key: &str,
    value: impl std::fmt::Display,
) {
    use std::fmt::Write as _;
    let source = effective
        .sources()
        .get(key)
        .cloned()
        .unwrap_or(ConfigSource::Default);
    let _ = writeln!(out, "{key} = {value}    # {source}");
}

/// Render the effective configuration with per-key source layers.
fn render_effective(effective: &EffectiveConfig) -> String {
    let mut out = String::new();
    push_rendered(
        &mut out,
        effective,
        "index.max_files",
        effective.budgets.max_files,
    );
    push_rendered(
        &mut out,
        effective,
        "index.max_source_file_bytes",
        effective.budgets.max_source_file_bytes,
    );
    push_rendered(
        &mut out,
        effective,
        "index.max_workspace_source_bytes",
        effective.budgets.max_workspace_source_bytes,
    );
    push_rendered(
        &mut out,
        effective,
        "index.max_symbols",
        effective.budgets.max_symbols,
    );
    push_rendered(
        &mut out,
        effective,
        "index.max_edges",
        effective.budgets.max_edges,
    );
    push_rendered(
        &mut out,
        effective,
        "index.max_call_sites",
        effective.budgets.max_call_sites,
    );
    push_rendered(
        &mut out,
        effective,
        "index.max_workers",
        effective.budgets.max_workers,
    );
    push_rendered(
        &mut out,
        effective,
        "index.startup_target_millis",
        effective.budgets.startup_target_millis,
    );
    push_rendered(
        &mut out,
        effective,
        "index.memory_target_bytes",
        effective.budgets.memory_target_bytes,
    );
    push_rendered(
        &mut out,
        effective,
        "startup.max_workspaces",
        effective.max_workspaces,
    );
    push_rendered(
        &mut out,
        effective,
        "startup.live_index_startup_timeout_millis",
        effective.live_index_startup_timeout_millis,
    );
    push_rendered(
        &mut out,
        effective,
        "providers.max_active",
        effective.max_active_providers,
    );
    push_rendered(
        &mut out,
        effective,
        "providers.max_active_per_workspace",
        effective.max_active_providers_per_workspace,
    );
    push_rendered(
        &mut out,
        effective,
        "providers.max_reserved_memory_bytes",
        effective.max_provider_reserved_memory_bytes,
    );
    push_rendered(
        &mut out,
        effective,
        "providers.max_reserved_memory_bytes_per_workspace",
        effective.max_provider_reserved_memory_bytes_per_workspace,
    );
    push_rendered(
        &mut out,
        effective,
        "providers.max_concurrent_queries",
        effective.max_concurrent_provider_queries,
    );
    push_rendered(
        &mut out,
        effective,
        "providers.max_queued_queries",
        effective.max_queued_provider_queries,
    );
    push_rendered(
        &mut out,
        effective,
        "providers.queue_timeout_millis",
        effective.provider_queue_timeout_millis,
    );
    push_rendered(
        &mut out,
        effective,
        "providers.idle_timeout_millis",
        effective.provider_idle_timeout_millis,
    );
    push_rendered(
        &mut out,
        effective,
        "providers.jdtls_readiness_timeout_millis",
        effective.jdtls_readiness_timeout_millis,
    );
    push_rendered(
        &mut out,
        effective,
        "update.automatic",
        effective.update_automatic,
    );
    for key in ProviderKey::ALL {
        let settings = effective.provider(*key);
        push_rendered(
            &mut out,
            effective,
            &format!("providers.{}.enabled", key.as_str()),
            settings.enabled,
        );
        if let Some(path) = &settings.path {
            push_rendered(
                &mut out,
                effective,
                &format!("providers.{}.path", key.as_str()),
                path.display(),
            );
        }
    }
    out
}

fn config_command(args: ConfigArgs) -> ExitCode {
    match args.command {
        ConfigCommands::Show(show) => {
            let layers = match resolve_config(&show.repo, show.config.as_deref()) {
                Ok(layers) => layers,
                Err(error) => {
                    eprintln!("chakra: {error}");
                    return ExitCode::FAILURE;
                }
            };
            let effective = layers.merge();
            // Configuration diagnostics go to stdout; no MCP server runs here.
            print!("{}", render_effective(&effective));
            ExitCode::SUCCESS
        }
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
    let Some(primary_repo) = args.repo.first().cloned() else {
        eprintln!("chakra: at least one --repo worktree is required");
        return ExitCode::FAILURE;
    };
    // Configuration is loaded and merged once at startup; changing it
    // requires a restart (ADR-0053).
    let layers = match resolve_config(&primary_repo, args.config.as_deref()) {
        Ok(layers) => layers,
        Err(error) => {
            eprintln!("chakra: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut effective = layers.merge();
    apply_cli_overrides(&mut effective, &args);
    let repo = args.repo.clone();
    let registry = match WorkspaceRegistry::new(WorkspaceRegistryConfig {
        max_workspaces: effective.max_workspaces,
    }) {
        Ok(registry) => Arc::new(registry),
        Err(error) => {
            eprintln!("chakra: invalid workspace registry configuration: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut registered_workspaces = Vec::with_capacity(repo.len());
    for root in repo {
        let (workspace_budgets, workspace_live_timeout) =
            match workspace_scoped_settings(&effective, &args, &root, &primary_repo) {
                Ok(settings) => settings,
                Err(error) => {
                    eprintln!("chakra: {error}");
                    let registry = registry.clone();
                    let _ = tokio::task::spawn_blocking(move || registry.shutdown()).await;
                    return ExitCode::FAILURE;
                }
            };
        let options = match chakra_language::IndexOptions::new(
            workspace_budgets,
            IndexCancellation::default(),
        ) {
            Ok(options) => options,
            Err(error) => {
                eprintln!("chakra: invalid index budget: {error}");
                let registry = registry.clone();
                let _ = tokio::task::spawn_blocking(move || registry.shutdown()).await;
                return ExitCode::FAILURE;
            }
        };
        let start_registry = registry.clone();
        let workspace_options = WorkspaceStartOptions {
            index: options,
            live: chakra_language::LiveIndexOptions {
                startup_timeout: Duration::from_millis(workspace_live_timeout),
                ..chakra_language::LiveIndexOptions::default()
            },
        };
        let registered = match tokio::task::spawn_blocking(move || {
            start_registry.register(&root, workspace_options)
        })
        .await
        {
            Ok(Ok(registered)) => registered,
            Ok(Err(error)) => {
                eprintln!("chakra: {error}");
                let _ = tokio::task::spawn_blocking(move || registry.shutdown()).await;
                return ExitCode::FAILURE;
            }
            Err(error) => {
                eprintln!("chakra: workspace startup task failed: {error}");
                let _ = tokio::task::spawn_blocking(move || registry.shutdown()).await;
                return ExitCode::FAILURE;
            }
        };
        let initial_metrics = registered.initial_metrics();
        tracing::info!(
            workspace = %registered.identity().workspace,
            root = %registered.identity().root.display(),
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
        registered_workspaces.push(registered);
    }
    let mut registrations = Vec::new();
    // Provider executable overrides resolved from configuration and CLI; the
    // closures below capture these locals.
    let rust_analyzer_executable: OsString = effective
        .provider(ProviderKey::RustAnalyzer)
        .path
        .clone()
        .map(PathBuf::into_os_string)
        .unwrap_or_else(|| OsString::from("rust-analyzer"));
    let vtsls_path = effective
        .provider(ProviderKey::Vtsls)
        .path
        .clone()
        .map(PathBuf::into_os_string);
    let pyright_path = effective
        .provider(ProviderKey::Pyright)
        .path
        .clone()
        .map(PathBuf::into_os_string);
    let jdtls_path = effective
        .provider(ProviderKey::Jdtls)
        .path
        .clone()
        .map(PathBuf::into_os_string);
    let csharp_ls_path = effective
        .provider(ProviderKey::CsharpLs)
        .path
        .clone()
        .map(PathBuf::into_os_string);
    let bash_language_server_path = effective
        .provider(ProviderKey::BashLanguageServer)
        .path
        .clone()
        .map(PathBuf::into_os_string);
    let clangd_path = effective
        .provider(ProviderKey::Clangd)
        .path
        .clone()
        .map(PathBuf::into_os_string);
    let terraform_ls_path = effective
        .provider(ProviderKey::TerraformLs)
        .path
        .clone()
        .map(PathBuf::into_os_string);
    let gopls_path = effective
        .provider(ProviderKey::Gopls)
        .path
        .clone()
        .map(PathBuf::into_os_string);
    let jdtls_readiness_timeout_millis = effective.jdtls_readiness_timeout_millis;
    if effective.provider(ProviderKey::RustAnalyzer).enabled {
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
                        executable: rust_analyzer_executable.clone(),
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
    if effective.provider(ProviderKey::Vtsls).enabled {
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
    if effective.provider(ProviderKey::Pyright).enabled {
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
    if effective.provider(ProviderKey::Jdtls).enabled {
        let query_wait_budget = chakra_provider_jdtls::DEFAULT_QUERY_WAIT_TIMEOUT;
        registrations.push(
            ProviderRegistration::new(
                "jdtls",
                vec![Language::Java],
                1024 * 1024 * 1024,
                move |workspace,
                      operation|
                      -> Result<Arc<dyn PreciseProvider>, ProviderStartError> {
                    // The mandatory jdtls `-data` directory is derived from
                    // this worktree and must never be cached across roots.
                    let resolved_command = chakra_provider_jdtls::resolve_command_with_context(
                        jdtls_path.as_deref(),
                        &workspace.repository_root,
                        operation,
                    )
                    .map_err(ProviderStartError::from)?;
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
    if effective.provider(ProviderKey::CsharpLs).enabled {
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
    if effective.provider(ProviderKey::BashLanguageServer).enabled {
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
    if effective.provider(ProviderKey::Clangd).enabled {
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
    if effective.provider(ProviderKey::TerraformLs).enabled {
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
    if effective.provider(ProviderKey::Gopls).enabled {
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
            max_active_providers: effective.max_active_providers,
            max_active_providers_per_workspace: effective.max_active_providers_per_workspace,
            max_reserved_memory_bytes: effective.max_provider_reserved_memory_bytes,
            max_reserved_memory_bytes_per_workspace: effective
                .max_provider_reserved_memory_bytes_per_workspace,
            max_concurrent_queries: effective.max_concurrent_provider_queries,
            max_queued_queries: effective.max_queued_provider_queries,
            query_queue_timeout: Duration::from_millis(effective.provider_queue_timeout_millis),
            idle_timeout: Duration::from_millis(effective.provider_idle_timeout_millis),
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
    for registered in &registered_workspaces {
        let providers = match provider_pool.providers_for(registered.identity()) {
            Ok(providers) => providers,
            Err(error) => {
                eprintln!("chakra: failed to bind precise providers: {error}");
                let _ = tokio::task::spawn_blocking(move || provider_pool.shutdown()).await;
                let _ = tokio::task::spawn_blocking(move || registry.shutdown()).await;
                return ExitCode::FAILURE;
            }
        };
        let engine = registered.engine();
        for provider in providers {
            if let Err(error) = engine.install_precise_provider(provider) {
                eprintln!("chakra: failed to install precise provider: {error}");
                let _ = tokio::task::spawn_blocking(move || provider_pool.shutdown()).await;
                let _ = tokio::task::spawn_blocking(move || registry.shutdown()).await;
                return ExitCode::FAILURE;
            }
        }
    }
    let update_task = crate::update::spawn_automatic_check(effective.update_automatic);
    let serve_result = chakra_mcp::serve_stdio_router(registry.clone()).await;
    // The update task is bounded by its own deadline; aborting guarantees no
    // orphaned work outlives the server (ADR-0054).
    if let Some(handle) = update_task {
        handle.abort();
    }
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
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_budget_help_is_language_neutral() -> TestResult {
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
        // The parser holds no defaults: an absent flag contributes nothing, so
        // configured values win over clap and explicit CLI wins over files
        // (ADR-0053).
        let cli = Cli::try_parse_from(["chakra", "serve", "--repo", "/tmp/example"]);
        assert!(matches!(
            cli,
            Ok(Cli {
                command: Some(Commands::Serve(ref args)),
            }) if args.repo == [PathBuf::from("/tmp/example")]
                && args.config.is_none()
                && args.max_workspaces.is_none()
                && args.live_index_startup_timeout_millis.is_none()
                && !args.no_rust_analyzer
                && args.rust_analyzer_path.is_none()
                && !args.no_vtsls
                && args.vtsls_path.is_none()
                && !args.no_pyright
                && args.pyright_path.is_none()
                && !args.no_jdtls
                && args.jdtls_path.is_none()
                && args.jdtls_readiness_timeout_millis.is_none()
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
                && args.max_active_providers.is_none()
                && args.max_active_providers_per_workspace.is_none()
                && args.max_index_files.is_none()
                && args.max_index_symbols.is_none()
                && args.max_index_workers.is_none()
        ));
    }

    #[test]
    fn serve_without_repo_keeps_the_current_directory_default() {
        let cli = Cli::try_parse_from(["chakra", "serve"]);
        assert!(matches!(
            cli,
            Ok(Cli {
                command: Some(Commands::Serve(ref args)),
            }) if args.repo == [PathBuf::from(".")]
        ));
    }

    #[test]
    fn serve_accepts_repeated_worktrees_and_distinct_provider_limits() {
        let cli = Cli::try_parse_from([
            "chakra",
            "serve",
            "--repo",
            "/tmp/first",
            "--repo",
            "/tmp/second",
            "--max-workspaces",
            "2",
            "--max-active-providers",
            "4",
            "--max-active-providers-per-workspace",
            "2",
            "--max-provider-reserved-memory-bytes-per-workspace",
            "1048576",
        ]);
        assert!(matches!(
            cli,
            Ok(Cli {
                command: Some(Commands::Serve(ref args)),
            }) if args.repo == [PathBuf::from("/tmp/first"), PathBuf::from("/tmp/second")]
                && args.max_workspaces == Some(2)
                && args.max_active_providers == Some(4)
                && args.max_active_providers_per_workspace == Some(2)
                && args.max_provider_reserved_memory_bytes_per_workspace == Some(1_048_576)
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
                && args.rust_analyzer_path.as_deref()
                    == Some(std::ffi::OsStr::new("/opt/bin/rust-analyzer"))
                && args.no_vtsls
                && args.vtsls_path.as_deref() == Some(std::ffi::OsStr::new("/opt/bin/vtsls"))
                && args.no_pyright
                && args.pyright_path.as_deref()
                    == Some(std::ffi::OsStr::new("/opt/bin/pyright-langserver"))
                && args.no_jdtls
                && args.jdtls_path.as_deref()
                    == Some(std::ffi::OsStr::new("/opt/bin/jdtls"))
                && args.jdtls_readiness_timeout_millis == Some(240_000)
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
            }) if args.max_index_workers == Some(2)
        ));
    }

    #[test]
    fn file_configuration_applies_when_cli_flags_are_absent() -> TestResult {
        // A non-Git tempdir is its own configuration base; workspace
        // registration would report the missing repository later.
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join(config::SHARED_CONFIG_FILENAME),
            "schema_version = 1\n\n[providers.clangd]\nenabled = false\n\n[providers]\nmax_active = 2\n",
        )?;
        let root = directory.path().to_string_lossy().into_owned();
        let cli = Cli::try_parse_from(["chakra", "serve", "--repo", &root]);
        let args = match cli {
            Ok(Cli {
                command: Some(Commands::Serve(args)),
            }) => args,
            other => return Err(format!("unexpected parse result: {other:?}").into()),
        };
        let layers = resolve_config(std::path::Path::new(&root), None)?;
        let mut effective = layers.merge();
        apply_cli_overrides(&mut effective, &args);
        assert!(!effective.provider(ProviderKey::Clangd).enabled);
        assert!(effective.provider(ProviderKey::Gopls).enabled);
        assert_eq!(effective.max_active_providers, 2);
        assert!(matches!(
            effective.sources()["providers.clangd.enabled"],
            ConfigSource::Shared(_)
        ));
        Ok(())
    }

    #[test]
    fn secondary_worktree_reads_its_own_configuration() -> TestResult {
        // Each registered worktree reads its own checked-out chakra.toml for
        // workspace-scoped settings instead of inheriting the primary's
        // (ADR-0053, issue #213).
        let primary = tempfile::tempdir()?;
        let secondary = tempfile::tempdir()?;
        std::fs::write(
            primary.path().join(config::SHARED_CONFIG_FILENAME),
            "schema_version = 1\n\n[index]\nmax_files = 1\n",
        )?;
        std::fs::write(
            secondary.path().join(config::SHARED_CONFIG_FILENAME),
            "schema_version = 1\n\n[index]\nmax_files = 25\n",
        )?;
        let primary_root = primary.path().to_string_lossy().into_owned();
        let secondary_root = secondary.path().to_string_lossy().into_owned();
        let cli = Cli::try_parse_from(["chakra", "serve", "--repo", &primary_root]);
        let args = match cli {
            Ok(Cli {
                command: Some(Commands::Serve(args)),
            }) => args,
            other => return Err(format!("unexpected parse result: {other:?}").into()),
        };
        let primary_path = std::path::Path::new(&primary_root);
        let secondary_path = std::path::Path::new(&secondary_root);
        let mut effective = resolve_config(primary_path, None)?.merge();
        apply_cli_overrides(&mut effective, &args);
        let (primary_budgets, _) =
            workspace_scoped_settings(&effective, &args, primary_path, primary_path)?;
        let (secondary_budgets, _) =
            workspace_scoped_settings(&effective, &args, secondary_path, primary_path)?;
        assert_eq!(primary_budgets.max_files, 1);
        assert_eq!(secondary_budgets.max_files, 25);
        Ok(())
    }

    #[test]
    fn cli_options_override_file_configuration() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join(config::SHARED_CONFIG_FILENAME),
            "schema_version = 1\n\n[index]\nmax_files = 100\n\n[providers]\nmax_active = 2\n",
        )?;
        let root = directory.path().to_string_lossy().into_owned();
        let cli =
            Cli::try_parse_from(["chakra", "serve", "--repo", &root, "--max-index-files", "7"]);
        let args = match cli {
            Ok(Cli {
                command: Some(Commands::Serve(args)),
            }) => args,
            other => return Err(format!("unexpected parse result: {other:?}").into()),
        };
        let layers = resolve_config(std::path::Path::new(&root), None)?;
        let mut effective = layers.merge();
        apply_cli_overrides(&mut effective, &args);
        assert_eq!(effective.budgets.max_files, 7);
        assert_eq!(effective.max_active_providers, 2);
        assert_eq!(effective.sources()["index.max_files"], ConfigSource::Cli);
        assert!(matches!(
            effective.sources()["providers.max_active"],
            ConfigSource::Shared(_)
        ));
        Ok(())
    }

    #[test]
    fn config_show_renders_effective_values_with_sources() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join(config::SHARED_CONFIG_FILENAME),
            "schema_version = 1\n\n[index]\nmax_files = 55\n",
        )?;
        let root = directory.path().to_string_lossy().into_owned();
        let effective = resolve_config(std::path::Path::new(&root), None)?.merge();
        let rendered = render_effective(&effective);
        assert!(rendered.contains("index.max_files = 55"), "{rendered}");
        assert!(rendered.contains("# shared:"), "{rendered}");
        assert!(rendered.contains("index.max_workers"), "{rendered}");
        assert!(
            rendered.contains("providers.gopls.enabled = true"),
            "{rendered}"
        );
        assert!(
            rendered.contains("providers.rust-analyzer.path = rust-analyzer"),
            "{rendered}"
        );
        Ok(())
    }
}
