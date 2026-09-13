//! `chakra.toml` project configuration: parsing, validation, precedence
//! merging, and per-key source tracking (ADR-0053).
//!
//! The module is the CLI-side configuration boundary. It produces plain typed
//! values; no domain, engine, or provider crate depends on TOML or on these
//! types beyond receiving the merged output.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use chakra_domain::indexing::{
    DEFAULT_MAX_INDEX_CALL_SITES, DEFAULT_MAX_INDEX_EDGES, DEFAULT_MAX_INDEX_FILES,
    DEFAULT_MAX_INDEX_SYMBOLS, DEFAULT_MAX_INDEX_WORKERS, DEFAULT_MAX_SOURCE_FILE_BYTES,
    DEFAULT_MAX_WORKSPACE_SOURCE_BYTES, DEFAULT_MEMORY_TARGET_BYTES, DEFAULT_STARTUP_TARGET_MILLIS,
    IndexBudgets,
};

/// Shared, committed project configuration file name (ADR-0053).
pub const SHARED_CONFIG_FILENAME: &str = "chakra.toml";
/// Private, non-committed project override file name (ADR-0053).
pub const PRIVATE_CONFIG_FILENAME: &str = "chakra.local.toml";
/// The only configuration schema this binary accepts.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;
/// Maximum accepted size of one configuration file. Configuration is
/// hand-written and orders of magnitude smaller; the cap only stops
/// accidental or hostile floods, and special files (FIFOs, devices) are
/// rejected before reading so they cannot block startup.
pub const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;

/// Process startup defaults previously hardcoded as clap default values.
/// They live here so built-in defaults, files, and CLI overrides share one
/// source of truth.
pub const DEFAULT_MAX_WORKSPACES: usize = 4;
pub const DEFAULT_LIVE_INDEX_STARTUP_TIMEOUT_MILLIS: u64 = 30_000;
pub const DEFAULT_MAX_ACTIVE_PROVIDERS: usize = 3;
pub const DEFAULT_MAX_ACTIVE_PROVIDERS_PER_WORKSPACE: usize = 3;
pub const DEFAULT_MAX_PROVIDER_RESERVED_MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_PROVIDER_RESERVED_MEMORY_BYTES_PER_WORKSPACE: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_CONCURRENT_PROVIDER_QUERIES: usize = 4;
pub const DEFAULT_MAX_QUEUED_PROVIDER_QUERIES: usize = 16;
pub const DEFAULT_PROVIDER_QUEUE_TIMEOUT_MILLIS: u64 = 1_000;
pub const DEFAULT_PROVIDER_IDLE_TIMEOUT_MILLIS: u64 = 5 * 60 * 1_000;
pub const DEFAULT_JDTLS_READINESS_TIMEOUT_MILLIS: u64 = 3 * 60 * 1_000;

/// One configuration layer in precedence order (lowest first).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigSource {
    /// Built-in default; no file or flag contributed the value.
    Default,
    /// The shared, committed `chakra.toml`.
    Shared(PathBuf),
    /// The private, non-committed `chakra.local.toml`.
    Private(PathBuf),
    /// An explicit CLI option.
    Cli,
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigSource::Default => write!(f, "default"),
            ConfigSource::Shared(path) => write!(f, "shared:{}", path.display()),
            ConfigSource::Private(path) => write!(f, "private:{}", path.display()),
            ConfigSource::Cli => write!(f, "cli"),
        }
    }
}

/// Configuration load or validation failure with file and key context.
/// A configuration that fails to parse or validate is never applied
/// partially (ADR-0053).
#[derive(Debug)]
pub enum ConfigError {
    /// A file that was selected or discovered could not be read.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The TOML document is malformed or fails schema deserialization.
    Parse { path: PathBuf, message: String },
    /// A parsed document violates the schema contract.
    Validation { path: PathBuf, message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, source } => {
                write!(f, "cannot read configuration {}: {source}", path.display())
            }
            ConfigError::Parse { path, message } => {
                write!(f, "invalid configuration {}: {message}", path.display())
            }
            ConfigError::Validation { path, message } => {
                write!(f, "invalid configuration {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Per-provider settings that both layers may carry.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    enabled: Option<bool>,
    /// Provider executable path. Portable shared configuration rejects this
    /// key; only the private layer may carry it (ADR-0053 trust boundary).
    path: Option<PathBuf>,
}

macro_rules! provider_table {
    ($($field:ident: $variant:ident => $name:literal),+ $(,)?) => {
        #[derive(Debug, Default, Clone, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawProviderTables {
            $(#[serde(rename = $name)] $field: Option<RawProvider>,)+
        }

        /// Stable set of precise-provider keys understood by configuration.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        pub enum ProviderKey {
            $($variant,)+
        }

        impl ProviderKey {
            /// Every provider key in registry order.
            pub const ALL: &'static [ProviderKey] = &[$(ProviderKey::$variant,)+];

            /// The provider's configuration table and CLI registration name.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(ProviderKey::$variant => $name,)+
                }
            }
        }

        impl RawProviderTables {
            fn get(&self, key: ProviderKey) -> Option<&RawProvider> {
                match key {
                    $(ProviderKey::$variant => self.$field.as_ref(),)+
                }
            }
        }
    };
}

provider_table! {
    rust_analyzer: RustAnalyzer => "rust-analyzer",
    vtsls: Vtsls => "vtsls",
    pyright: Pyright => "pyright",
    jdtls: Jdtls => "jdtls",
    csharp_ls: CsharpLs => "csharp-ls",
    bash_language_server: BashLanguageServer => "bash-language-server",
    clangd: Clangd => "clangd",
    terraform_ls: TerraformLs => "terraform-ls",
    gopls: Gopls => "gopls",
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIndex {
    max_files: Option<u64>,
    max_source_file_bytes: Option<u64>,
    max_workspace_source_bytes: Option<u64>,
    max_symbols: Option<u64>,
    max_edges: Option<u64>,
    max_call_sites: Option<u64>,
    max_workers: Option<u64>,
    startup_target_millis: Option<u64>,
    memory_target_bytes: Option<u64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStartup {
    max_workspaces: Option<u64>,
    live_index_startup_timeout_millis: Option<u64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProviders {
    max_active: Option<u64>,
    max_active_per_workspace: Option<u64>,
    max_reserved_memory_bytes: Option<u64>,
    max_reserved_memory_bytes_per_workspace: Option<u64>,
    max_concurrent_queries: Option<u64>,
    max_queued_queries: Option<u64>,
    queue_timeout_millis: Option<u64>,
    idle_timeout_millis: Option<u64>,
    jdtls_readiness_timeout_millis: Option<u64>,
    #[serde(flatten)]
    tables: RawProviderTables,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema_version: u32,
    index: Option<RawIndex>,
    providers: Option<RawProviders>,
    startup: Option<RawStartup>,
}

/// One parsed configuration file with its layer and origin directory.
#[derive(Debug)]
struct Layer {
    source: ConfigSource,
    /// Directory that relative `path` values resolve against (ADR-0053).
    base: PathBuf,
    raw: RawConfig,
}

/// The discovered or selected configuration files, in precedence order.
#[derive(Debug, Default)]
pub struct ConfigLayers {
    layers: Vec<Layer>,
}

/// Effective per-provider settings after merging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSettings {
    pub enabled: bool,
    /// Absolute or PATH-resolved executable override; private layer or CLI
    /// only.
    pub path: Option<PathBuf>,
}

/// The merged, startup-final configuration consumed by `chakra serve`.
///
/// Every value is concrete; `sources` records which layer produced each
/// dotted key for `chakra config show`.
#[derive(Debug)]
pub struct EffectiveConfig {
    pub budgets: IndexBudgets,
    pub max_workspaces: usize,
    pub live_index_startup_timeout_millis: u64,
    pub max_active_providers: usize,
    pub max_active_providers_per_workspace: usize,
    pub max_provider_reserved_memory_bytes: u64,
    pub max_provider_reserved_memory_bytes_per_workspace: u64,
    pub max_concurrent_provider_queries: usize,
    pub max_queued_provider_queries: usize,
    pub provider_queue_timeout_millis: u64,
    pub provider_idle_timeout_millis: u64,
    pub jdtls_readiness_timeout_millis: u64,
    providers: BTreeMap<ProviderKey, ProviderSettings>,
    sources: BTreeMap<String, ConfigSource>,
}

impl EffectiveConfig {
    /// Settings of one provider after merging.
    pub fn provider(&self, key: ProviderKey) -> &ProviderSettings {
        self.providers
            .get(&key)
            .unwrap_or_else(|| unreachable!("every provider key is populated"))
    }

    /// Mutable settings of one provider, for applying CLI overrides.
    pub fn provider_mut(&mut self, key: ProviderKey) -> &mut ProviderSettings {
        self.providers
            .get_mut(&key)
            .unwrap_or_else(|| unreachable!("every provider key is populated"))
    }

    /// Source layers per dotted key, for diagnostics.
    pub fn sources(&self) -> &BTreeMap<String, ConfigSource> {
        &self.sources
    }

    /// Record that an explicit CLI option won for `key` (ADR-0053 layer 4).
    /// The caller sets the value field itself; this only tracks provenance.
    pub fn record_cli_override(&mut self, key: &str) {
        self.sources.insert(key.to_owned(), ConfigSource::Cli);
    }
}

fn validate_raw(path: &Path, raw: &RawConfig, allow_paths: bool) -> Result<(), ConfigError> {
    if raw.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(ConfigError::Validation {
            path: path.to_owned(),
            message: format!(
                "unsupported schema_version {}; this chakra supports schema version {}",
                raw.schema_version, SUPPORTED_SCHEMA_VERSION
            ),
        });
    }
    if let Some(providers) = &raw.providers {
        for key in ProviderKey::ALL {
            if let Some(table) = providers.tables.get(*key)
                && !allow_paths
                && table.path.is_some()
            {
                return Err(ConfigError::Validation {
                    path: path.to_owned(),
                    message: format!(
                        "providers.{}.path selects an executable and is not portable; move it to {} or pass the matching CLI option",
                        key.as_str(),
                        PRIVATE_CONFIG_FILENAME
                    ),
                });
            }
        }
        let nonzero: [(&str, Option<u64>); 9] = [
            ("max_active", providers.max_active),
            (
                "max_active_per_workspace",
                providers.max_active_per_workspace,
            ),
            (
                "max_reserved_memory_bytes",
                providers.max_reserved_memory_bytes,
            ),
            (
                "max_reserved_memory_bytes_per_workspace",
                providers.max_reserved_memory_bytes_per_workspace,
            ),
            ("max_concurrent_queries", providers.max_concurrent_queries),
            ("max_queued_queries", providers.max_queued_queries),
            ("queue_timeout_millis", providers.queue_timeout_millis),
            ("idle_timeout_millis", providers.idle_timeout_millis),
            (
                "jdtls_readiness_timeout_millis",
                providers.jdtls_readiness_timeout_millis,
            ),
        ];
        for (key, value) in nonzero {
            if value == Some(0) {
                return Err(ConfigError::Validation {
                    path: path.to_owned(),
                    message: format!("providers.{key} must be at least 1"),
                });
            }
        }
    }
    if let Some(index) = &raw.index {
        let nonzero: [(&str, Option<u64>); 9] = [
            ("max_files", index.max_files),
            ("max_source_file_bytes", index.max_source_file_bytes),
            (
                "max_workspace_source_bytes",
                index.max_workspace_source_bytes,
            ),
            ("max_symbols", index.max_symbols),
            ("max_edges", index.max_edges),
            ("max_call_sites", index.max_call_sites),
            ("max_workers", index.max_workers),
            ("startup_target_millis", index.startup_target_millis),
            ("memory_target_bytes", index.memory_target_bytes),
        ];
        for (key, value) in nonzero {
            if value == Some(0) {
                return Err(ConfigError::Validation {
                    path: path.to_owned(),
                    message: format!("index.{key} must be at least 1"),
                });
            }
        }
    }
    if let Some(startup) = &raw.startup
        && startup.max_workspaces == Some(0)
    {
        return Err(ConfigError::Validation {
            path: path.to_owned(),
            message: "startup.max_workspaces must be at least 1".to_owned(),
        });
    }
    Ok(())
}

/// Read one configuration file as text.
///
/// Metadata follows symlinks: a FIFO, device, socket, or directory is
/// rejected before `open`, so a special file cannot block startup reading
/// (for example a committed FIFO or a symlink to `/dev/stdin`). The byte cap
/// is enforced while reading, so a file that grows between the metadata
/// check and the read stays bounded.
fn read_config_text(path: &Path) -> Result<String, ConfigError> {
    let metadata = fs::metadata(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(ConfigError::Validation {
            path: path.to_owned(),
            message: "configuration must be a regular file; FIFOs, devices, sockets, and directories are not accepted"
                .to_owned(),
        });
    }
    let file = fs::File::open(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut limited = file.take(MAX_CONFIG_FILE_BYTES + 1);
    let mut text = String::new();
    limited
        .read_to_string(&mut text)
        .map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
    if text.len() as u64 > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::Validation {
            path: path.to_owned(),
            message: format!("configuration exceeds the {MAX_CONFIG_FILE_BYTES}-byte limit"),
        });
    }
    Ok(text)
}

fn load_layer(path: &Path, source: ConfigSource, allow_paths: bool) -> Result<Layer, ConfigError> {
    let text = read_config_text(path)?;
    let raw: RawConfig = toml::from_str(&text).map_err(|error| ConfigError::Parse {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    validate_raw(path, &raw, allow_paths)?;
    let base = path
        .parent()
        .map(Path::to_owned)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(Layer { source, base, raw })
}

/// Absolute form of `path` without resolving symlinks or requiring
/// existence, so a relative `--config` value still anchors relative paths at
/// the declaring file's real directory instead of the process working
/// directory (ADR-0053).
fn absolute_path(path: &Path) -> Result<PathBuf, ConfigError> {
    std::path::absolute(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })
}

/// True when `path` names any directory entry, including a dangling symlink.
/// Unlike `Path::exists`, a broken symlink counts as present so it fails
/// loudly instead of silently restoring built-in defaults.
fn path_present(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// A committed `chakra.local.toml` would let a repository clone select
/// provider executables, defeating the ADR-0053 trust boundary. Reject the
/// private override when Git tracks it. Outside a Git worktree there is no
/// tracking to enforce: the file is the user's own machine configuration.
fn ensure_untracked_private(private: &Path) -> Result<(), ConfigError> {
    let Some(parent) = private.parent() else {
        return Ok(());
    };
    let Some(file_name) = private.file_name() else {
        return Ok(());
    };
    // Canonicalize the parent so the comparison matches the canonical root
    // Git reports (for example macOS `/var` versus `/private/var`). The
    // parent directory exists whenever the file does.
    let Ok(canonical_parent) = fs::canonicalize(parent) else {
        return Ok(());
    };
    let Ok(root) = chakra_git::resolve_repository_root(&canonical_parent) else {
        return Ok(());
    };
    let Ok(relative_dir) = canonical_parent.strip_prefix(&root) else {
        return Ok(());
    };
    let relative = relative_dir.join(file_name);
    match chakra_git::is_worktree_path_tracked(&root, &relative) {
        Ok(true) => Err(ConfigError::Validation {
            path: private.to_owned(),
            message: format!(
                "{PRIVATE_CONFIG_FILENAME} is tracked by Git; a committed repository must not select provider executables — remove it from version control and keep it git-ignored"
            ),
        }),
        // When Git cannot answer, workspace registration will surface the
        // repository problem later; do not hard-fail configuration here.
        Ok(false) | Err(_) => Ok(()),
    }
}

impl ConfigLayers {
    /// Load the shared and private configuration for one worktree root.
    ///
    /// With `explicit`, the shared file is exactly that path and discovery is
    /// disabled; the private override is its `chakra.local.toml` sibling.
    /// Without `explicit`, the shared file is `chakra.toml` at the worktree
    /// root and the private file is its sibling. Missing files contribute no
    /// layer. Existing but unreadable or invalid files — including dangling
    /// symlinks, special files, and oversized documents — are hard errors.
    pub fn load(root: &Path, explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let mut layers = Vec::new();
        let shared = match explicit {
            Some(path) => Some(absolute_path(path)?),
            None => {
                let candidate = absolute_path(&root.join(SHARED_CONFIG_FILENAME))?;
                path_present(&candidate).then_some(candidate)
            }
        };
        if let Some(shared_path) = shared {
            layers.push(load_layer(
                &shared_path,
                ConfigSource::Shared(shared_path.clone()),
                false,
            )?);
            let private = shared_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(PRIVATE_CONFIG_FILENAME);
            if path_present(&private) {
                ensure_untracked_private(&private)?;
                layers.push(load_layer(
                    &private,
                    ConfigSource::Private(private.clone()),
                    true,
                )?);
            }
        }
        Ok(Self { layers })
    }

    /// Merge the loaded layers. Later layers override earlier ones per key;
    /// sources record the winning layer for diagnostics.
    pub fn merge(&self) -> EffectiveConfig {
        let mut sources: BTreeMap<String, ConfigSource> = BTreeMap::new();
        let mut budgets = IndexBudgets {
            max_files: DEFAULT_MAX_INDEX_FILES,
            max_source_file_bytes: DEFAULT_MAX_SOURCE_FILE_BYTES,
            max_workspace_source_bytes: DEFAULT_MAX_WORKSPACE_SOURCE_BYTES,
            max_symbols: DEFAULT_MAX_INDEX_SYMBOLS,
            max_edges: DEFAULT_MAX_INDEX_EDGES,
            max_call_sites: DEFAULT_MAX_INDEX_CALL_SITES,
            startup_target_millis: DEFAULT_STARTUP_TARGET_MILLIS,
            memory_target_bytes: DEFAULT_MEMORY_TARGET_BYTES,
            max_workers: DEFAULT_MAX_INDEX_WORKERS,
        };
        let mut max_workspaces = DEFAULT_MAX_WORKSPACES as u64;
        let mut live_index_startup_timeout_millis = DEFAULT_LIVE_INDEX_STARTUP_TIMEOUT_MILLIS;
        let mut max_active_providers = DEFAULT_MAX_ACTIVE_PROVIDERS as u64;
        let mut max_active_providers_per_workspace =
            DEFAULT_MAX_ACTIVE_PROVIDERS_PER_WORKSPACE as u64;
        let mut max_provider_reserved_memory_bytes = DEFAULT_MAX_PROVIDER_RESERVED_MEMORY_BYTES;
        let mut max_provider_reserved_memory_bytes_per_workspace =
            DEFAULT_MAX_PROVIDER_RESERVED_MEMORY_BYTES_PER_WORKSPACE;
        let mut max_concurrent_provider_queries = DEFAULT_MAX_CONCURRENT_PROVIDER_QUERIES as u64;
        let mut max_queued_provider_queries = DEFAULT_MAX_QUEUED_PROVIDER_QUERIES as u64;
        let mut provider_queue_timeout_millis = DEFAULT_PROVIDER_QUEUE_TIMEOUT_MILLIS;
        let mut provider_idle_timeout_millis = DEFAULT_PROVIDER_IDLE_TIMEOUT_MILLIS;
        let mut jdtls_readiness_timeout_millis = DEFAULT_JDTLS_READINESS_TIMEOUT_MILLIS;
        let mut providers: BTreeMap<ProviderKey, ProviderSettings> = ProviderKey::ALL
            .iter()
            .map(|key| {
                (
                    *key,
                    ProviderSettings {
                        enabled: true,
                        path: None,
                    },
                )
            })
            .collect();
        // rust-analyzer's built-in executable resolution is the PATH name;
        // every other provider discovers its command unless configured.
        if let Some(settings) = providers.get_mut(&ProviderKey::RustAnalyzer) {
            settings.path = Some(PathBuf::from("rust-analyzer"));
        }
        sources.insert(
            "providers.rust-analyzer.path".to_owned(),
            ConfigSource::Default,
        );

        for key in [
            "index.max_files",
            "index.max_source_file_bytes",
            "index.max_workspace_source_bytes",
            "index.max_symbols",
            "index.max_edges",
            "index.max_call_sites",
            "index.max_workers",
            "index.startup_target_millis",
            "index.memory_target_bytes",
            "startup.max_workspaces",
            "startup.live_index_startup_timeout_millis",
            "providers.max_active",
            "providers.max_active_per_workspace",
            "providers.max_reserved_memory_bytes",
            "providers.max_reserved_memory_bytes_per_workspace",
            "providers.max_concurrent_queries",
            "providers.max_queued_queries",
            "providers.queue_timeout_millis",
            "providers.idle_timeout_millis",
            "providers.jdtls_readiness_timeout_millis",
        ] {
            sources.insert(key.to_owned(), ConfigSource::Default);
        }
        for key in ProviderKey::ALL {
            sources.insert(
                format!("providers.{}.enabled", key.as_str()),
                ConfigSource::Default,
            );
        }

        for layer in &self.layers {
            let source = || layer.source.clone();
            if let Some(index) = &layer.raw.index {
                if let Some(value) = index.max_files {
                    budgets.max_files = value;
                    sources.insert("index.max_files".to_owned(), source());
                }
                if let Some(value) = index.max_source_file_bytes {
                    budgets.max_source_file_bytes = value;
                    sources.insert("index.max_source_file_bytes".to_owned(), source());
                }
                if let Some(value) = index.max_workspace_source_bytes {
                    budgets.max_workspace_source_bytes = value;
                    sources.insert("index.max_workspace_source_bytes".to_owned(), source());
                }
                if let Some(value) = index.max_symbols {
                    budgets.max_symbols = value;
                    sources.insert("index.max_symbols".to_owned(), source());
                }
                if let Some(value) = index.max_edges {
                    budgets.max_edges = value;
                    sources.insert("index.max_edges".to_owned(), source());
                }
                if let Some(value) = index.max_call_sites {
                    budgets.max_call_sites = value;
                    sources.insert("index.max_call_sites".to_owned(), source());
                }
                if let Some(value) = index.max_workers {
                    budgets.max_workers = value;
                    sources.insert("index.max_workers".to_owned(), source());
                }
                if let Some(value) = index.startup_target_millis {
                    budgets.startup_target_millis = value;
                    sources.insert("index.startup_target_millis".to_owned(), source());
                }
                if let Some(value) = index.memory_target_bytes {
                    budgets.memory_target_bytes = value;
                    sources.insert("index.memory_target_bytes".to_owned(), source());
                }
            }
            if let Some(startup) = &layer.raw.startup {
                if let Some(value) = startup.max_workspaces {
                    max_workspaces = value;
                    sources.insert("startup.max_workspaces".to_owned(), source());
                }
                if let Some(value) = startup.live_index_startup_timeout_millis {
                    live_index_startup_timeout_millis = value;
                    sources.insert(
                        "startup.live_index_startup_timeout_millis".to_owned(),
                        source(),
                    );
                }
            }
            if let Some(raw_providers) = &layer.raw.providers {
                if let Some(value) = raw_providers.max_active {
                    max_active_providers = value;
                    sources.insert("providers.max_active".to_owned(), source());
                }
                if let Some(value) = raw_providers.max_active_per_workspace {
                    max_active_providers_per_workspace = value;
                    sources.insert("providers.max_active_per_workspace".to_owned(), source());
                }
                if let Some(value) = raw_providers.max_reserved_memory_bytes {
                    max_provider_reserved_memory_bytes = value;
                    sources.insert("providers.max_reserved_memory_bytes".to_owned(), source());
                }
                if let Some(value) = raw_providers.max_reserved_memory_bytes_per_workspace {
                    max_provider_reserved_memory_bytes_per_workspace = value;
                    sources.insert(
                        "providers.max_reserved_memory_bytes_per_workspace".to_owned(),
                        source(),
                    );
                }
                if let Some(value) = raw_providers.max_concurrent_queries {
                    max_concurrent_provider_queries = value;
                    sources.insert("providers.max_concurrent_queries".to_owned(), source());
                }
                if let Some(value) = raw_providers.max_queued_queries {
                    max_queued_provider_queries = value;
                    sources.insert("providers.max_queued_queries".to_owned(), source());
                }
                if let Some(value) = raw_providers.queue_timeout_millis {
                    provider_queue_timeout_millis = value;
                    sources.insert("providers.queue_timeout_millis".to_owned(), source());
                }
                if let Some(value) = raw_providers.idle_timeout_millis {
                    provider_idle_timeout_millis = value;
                    sources.insert("providers.idle_timeout_millis".to_owned(), source());
                }
                if let Some(value) = raw_providers.jdtls_readiness_timeout_millis {
                    jdtls_readiness_timeout_millis = value;
                    sources.insert(
                        "providers.jdtls_readiness_timeout_millis".to_owned(),
                        source(),
                    );
                }
                for key in ProviderKey::ALL {
                    if let Some(table) = raw_providers.tables.get(*key) {
                        let settings = providers
                            .get_mut(key)
                            .unwrap_or_else(|| unreachable!("every provider key is populated"));
                        if let Some(enabled) = table.enabled {
                            settings.enabled = enabled;
                            sources.insert(format!("providers.{}.enabled", key.as_str()), source());
                        }
                        if let Some(path) = &table.path {
                            // Relative executable paths resolve against the
                            // declaring file's directory, never the process
                            // working directory (ADR-0053).
                            settings.path = Some(if path.is_absolute() {
                                path.clone()
                            } else {
                                layer.base.join(path)
                            });
                            sources.insert(format!("providers.{}.path", key.as_str()), source());
                        }
                    }
                }
            }
        }

        EffectiveConfig {
            budgets,
            max_workspaces: max_workspaces as usize,
            live_index_startup_timeout_millis,
            max_active_providers: max_active_providers as usize,
            max_active_providers_per_workspace: max_active_providers_per_workspace as usize,
            max_provider_reserved_memory_bytes,
            max_provider_reserved_memory_bytes_per_workspace,
            max_concurrent_provider_queries: max_concurrent_provider_queries as usize,
            max_queued_provider_queries: max_queued_provider_queries as usize,
            provider_queue_timeout_millis,
            provider_idle_timeout_millis,
            jdtls_readiness_timeout_millis,
            providers,
            sources,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn write(
        root: &Path,
        name: &str,
        contents: &str,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let path = root.join(name);
        fs::write(&path, contents)?;
        Ok(path)
    }

    #[test]
    fn empty_layers_resolve_to_documented_defaults() -> TestResult {
        let effective = ConfigLayers::default().merge();
        assert_eq!(effective.budgets.max_files, DEFAULT_MAX_INDEX_FILES);
        assert_eq!(effective.budgets.max_workers, DEFAULT_MAX_INDEX_WORKERS);
        assert_eq!(effective.max_workspaces, DEFAULT_MAX_WORKSPACES);
        assert_eq!(effective.max_active_providers, DEFAULT_MAX_ACTIVE_PROVIDERS);
        assert_eq!(
            effective.jdtls_readiness_timeout_millis,
            DEFAULT_JDTLS_READINESS_TIMEOUT_MILLIS
        );
        for key in ProviderKey::ALL {
            assert!(effective.provider(*key).enabled);
        }
        assert_eq!(
            effective.provider(ProviderKey::RustAnalyzer).path,
            Some(PathBuf::from("rust-analyzer"))
        );
        for key in ProviderKey::ALL {
            if *key != ProviderKey::RustAnalyzer {
                assert_eq!(effective.provider(*key).path, None);
            }
        }
        assert!(
            effective
                .sources()
                .values()
                .all(|source| *source == ConfigSource::Default)
        );
        Ok(())
    }

    #[test]
    fn shared_file_overrides_defaults_and_records_sources() -> TestResult {
        let directory = tempfile::tempdir()?;
        write(
            directory.path(),
            SHARED_CONFIG_FILENAME,
            "schema_version = 1\n\n[index]\nmax_files = 1234\n\n[providers.pyright]\nenabled = false\n",
        )?;
        let effective = ConfigLayers::load(directory.path(), None)?.merge();
        assert_eq!(effective.budgets.max_files, 1234);
        assert_eq!(
            effective.budgets.max_source_file_bytes,
            DEFAULT_MAX_SOURCE_FILE_BYTES
        );
        assert!(!effective.provider(ProviderKey::Pyright).enabled);
        assert!(effective.provider(ProviderKey::Gopls).enabled);
        let shared = directory.path().join(SHARED_CONFIG_FILENAME);
        assert_eq!(
            effective.sources()["index.max_files"],
            ConfigSource::Shared(shared.clone())
        );
        assert_eq!(
            effective.sources()["providers.pyright.enabled"],
            ConfigSource::Shared(shared)
        );
        assert_eq!(
            effective.sources()["index.max_workers"],
            ConfigSource::Default
        );
        Ok(())
    }

    #[test]
    fn private_layer_overrides_shared_and_carries_executable_paths() -> TestResult {
        let directory = tempfile::tempdir()?;
        write(
            directory.path(),
            SHARED_CONFIG_FILENAME,
            "schema_version = 1\n\n[providers]\nmax_active = 2\n",
        )?;
        write(
            directory.path(),
            PRIVATE_CONFIG_FILENAME,
            "schema_version = 1\n\n[providers]\nmax_active = 5\n\n[providers.clangd]\npath = \"tools/clangd\"\n",
        )?;
        let effective = ConfigLayers::load(directory.path(), None)?.merge();
        assert_eq!(effective.max_active_providers, 5);
        assert_eq!(
            effective.provider(ProviderKey::Clangd).path,
            Some(directory.path().join("tools/clangd")),
            "relative executable paths resolve against the declaring file"
        );
        let private = directory.path().join(PRIVATE_CONFIG_FILENAME);
        assert_eq!(
            effective.sources()["providers.max_active"],
            ConfigSource::Private(private.clone())
        );
        assert_eq!(
            effective.sources()["providers.clangd.path"],
            ConfigSource::Private(private)
        );
        Ok(())
    }

    #[test]
    fn shared_layer_rejects_executable_paths() -> TestResult {
        let directory = tempfile::tempdir()?;
        write(
            directory.path(),
            SHARED_CONFIG_FILENAME,
            "schema_version = 1\n\n[providers.gopls]\npath = \"/opt/gopls\"\n",
        )?;
        let error = ConfigLayers::load(directory.path(), None)
            .err()
            .ok_or("shared executable path must be rejected")?;
        let message = error.to_string();
        assert!(message.contains("not portable"), "{message}");
        assert!(message.contains(PRIVATE_CONFIG_FILENAME), "{message}");
        Ok(())
    }

    #[test]
    fn explicit_config_disables_discovery_and_finds_private_sibling() -> TestResult {
        let directory = tempfile::tempdir()?;
        let chosen = write(
            directory.path(),
            "custom.toml",
            "schema_version = 1\n\n[index]\nmax_symbols = 42\n",
        )?;
        write(
            directory.path(),
            PRIVATE_CONFIG_FILENAME,
            "schema_version = 1\n\n[startup]\nmax_workspaces = 2\n",
        )?;
        let effective = ConfigLayers::load(directory.path(), Some(&chosen))?.merge();
        assert_eq!(effective.budgets.max_symbols, 42);
        assert_eq!(effective.max_workspaces, 2);
        assert_eq!(
            effective.sources()["index.max_symbols"],
            ConfigSource::Shared(chosen)
        );
        Ok(())
    }

    #[test]
    fn missing_files_are_not_an_error() -> TestResult {
        let directory = tempfile::tempdir()?;
        let layers = ConfigLayers::load(directory.path(), None)?;
        assert!(layers.layers.is_empty());
        Ok(())
    }

    #[test]
    fn explicit_missing_file_is_an_io_error() -> TestResult {
        let directory = tempfile::tempdir()?;
        let missing = directory.path().join("absent.toml");
        let error = ConfigLayers::load(directory.path(), Some(&missing))
            .err()
            .ok_or("explicit path must exist")?;
        assert!(matches!(error, ConfigError::Io { .. }), "{error}");
        Ok(())
    }

    #[test]
    fn unsupported_schema_version_is_rejected() -> TestResult {
        let directory = tempfile::tempdir()?;
        write(
            directory.path(),
            SHARED_CONFIG_FILENAME,
            "schema_version = 99\n",
        )?;
        let error = ConfigLayers::load(directory.path(), None)
            .err()
            .ok_or("unsupported schema version must be rejected")?;
        assert!(error.to_string().contains("schema_version 99"), "{error}");
        Ok(())
    }

    #[test]
    fn unknown_keys_are_rejected() -> TestResult {
        let directory = tempfile::tempdir()?;
        write(
            directory.path(),
            SHARED_CONFIG_FILENAME,
            "schema_version = 1\n\n[index]\nmax_file = 10\n",
        )?;
        let error = ConfigLayers::load(directory.path(), None)
            .err()
            .ok_or("unknown key must be rejected")?;
        assert!(error.to_string().contains("unknown field"), "{error}");
        Ok(())
    }

    #[test]
    fn unknown_keys_in_provider_tables_are_rejected() -> TestResult {
        let directory = tempfile::tempdir()?;
        write(
            directory.path(),
            SHARED_CONFIG_FILENAME,
            "schema_version = 1\n\n[providers]\nmax_actve = 2\n",
        )?;
        let error = ConfigLayers::load(directory.path(), None)
            .err()
            .ok_or("unknown providers key must be rejected")?;
        assert!(error.to_string().contains("unknown field"), "{error}");
        Ok(())
    }

    #[test]
    fn malformed_toml_is_rejected() -> TestResult {
        let directory = tempfile::tempdir()?;
        write(
            directory.path(),
            SHARED_CONFIG_FILENAME,
            "schema_version = [",
        )?;
        let error = ConfigLayers::load(directory.path(), None)
            .err()
            .ok_or("malformed TOML must be rejected")?;
        assert!(matches!(error, ConfigError::Parse { .. }), "{error}");
        Ok(())
    }

    #[test]
    fn zero_limits_are_rejected_with_source_context() -> TestResult {
        let directory = tempfile::tempdir()?;
        write(
            directory.path(),
            SHARED_CONFIG_FILENAME,
            "schema_version = 1\n\n[providers]\nmax_active = 0\n",
        )?;
        let error = ConfigLayers::load(directory.path(), None)
            .err()
            .ok_or("zero limit must be rejected")?;
        let message = error.to_string();
        assert!(message.contains("providers.max_active"), "{message}");
        assert!(message.contains(SHARED_CONFIG_FILENAME), "{message}");
        Ok(())
    }

    #[test]
    fn cli_override_records_provenance() -> TestResult {
        let mut effective = ConfigLayers::default().merge();
        effective.budgets.max_files = 7;
        effective.record_cli_override("index.max_files");
        assert_eq!(effective.sources()["index.max_files"], ConfigSource::Cli);
        Ok(())
    }

    fn run_git(root: &Path, args: &[&str]) -> TestResult {
        let status = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .status()?;
        assert!(status.success(), "git {args:?} failed");
        Ok(())
    }

    #[test]
    fn tracked_private_override_is_rejected() -> TestResult {
        // A committed `chakra.local.toml` would let a repository clone select
        // provider executables; Git-tracked private files are hard errors
        // (ADR-0053 trust boundary, issue #212).
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        run_git(root, &["init", "--quiet"])?;
        write(root, SHARED_CONFIG_FILENAME, "schema_version = 1\n")?;
        write(
            root,
            PRIVATE_CONFIG_FILENAME,
            "schema_version = 1\n\n[providers.clangd]\npath = \"/opt/clangd\"\n",
        )?;
        run_git(root, &["add", PRIVATE_CONFIG_FILENAME])?;
        let error = ConfigLayers::load(root, None)
            .err()
            .ok_or("tracked private override must be rejected")?;
        assert!(error.to_string().contains("tracked by Git"), "{error}");
        Ok(())
    }

    #[test]
    fn untracked_private_override_is_accepted_in_a_repository() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        run_git(root, &["init", "--quiet"])?;
        write(root, SHARED_CONFIG_FILENAME, "schema_version = 1\n")?;
        write(
            root,
            PRIVATE_CONFIG_FILENAME,
            "schema_version = 1\n\n[providers]\nmax_active = 2\n",
        )?;
        let effective = ConfigLayers::load(root, None)?.merge();
        assert_eq!(effective.max_active_providers, 2);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_shared_config_is_a_hard_error() -> TestResult {
        // `Path::exists` would silently restore defaults; presence is
        // symlink-aware so a broken link fails loudly (issue #216).
        let directory = tempfile::tempdir()?;
        std::os::unix::fs::symlink(
            directory.path().join("missing-target"),
            directory.path().join(SHARED_CONFIG_FILENAME),
        )?;
        let error = ConfigLayers::load(directory.path(), None)
            .err()
            .ok_or("dangling symlink must be a hard error")?;
        assert!(matches!(error, ConfigError::Io { .. }), "{error}");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn fifo_shared_config_is_rejected_without_blocking() -> TestResult {
        // A FIFO must be refused before `open`, so startup cannot block on a
        // special file (issue #215).
        let directory = tempfile::tempdir()?;
        let status = std::process::Command::new("mkfifo")
            .arg(directory.path().join(SHARED_CONFIG_FILENAME))
            .status()?;
        assert!(status.success(), "mkfifo failed");
        let error = ConfigLayers::load(directory.path(), None)
            .err()
            .ok_or("FIFO must be rejected")?;
        assert!(error.to_string().contains("regular file"), "{error}");
        Ok(())
    }

    #[test]
    fn oversized_config_is_rejected() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join(SHARED_CONFIG_FILENAME),
            "x".repeat((MAX_CONFIG_FILE_BYTES + 1) as usize),
        )?;
        let error = ConfigLayers::load(directory.path(), None)
            .err()
            .ok_or("oversized configuration must be rejected")?;
        assert!(error.to_string().contains("byte limit"), "{error}");
        Ok(())
    }

    #[test]
    fn relative_explicit_config_anchors_relative_paths_at_the_file() -> TestResult {
        // A relative `--config` must resolve relative provider paths against
        // the file's real directory, not the process working directory
        // (issue #214). Changing the process directory is process-global, so
        // this test holds a lock and restores the previous directory.
        static CURRENT_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CURRENT_DIR_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let directory = tempfile::tempdir()?;
        let canonical = directory.path().canonicalize()?;
        write(&canonical, "custom.toml", "schema_version = 1\n")?;
        write(
            &canonical,
            PRIVATE_CONFIG_FILENAME,
            "schema_version = 1\n\n[providers.clangd]\npath = \"tools/clangd\"\n",
        )?;
        let previous = std::env::current_dir()?;
        std::env::set_current_dir(&canonical)?;
        let result = ConfigLayers::load(Path::new("."), Some(Path::new("custom.toml")));
        std::env::set_current_dir(previous)?;
        let effective = result?.merge();
        assert_eq!(
            effective.provider(ProviderKey::Clangd).path,
            Some(canonical.join("tools/clangd")),
            "relative executable paths resolve against the declaring file"
        );
        Ok(())
    }

    #[test]
    fn provider_keys_cover_every_registered_name() -> TestResult {
        let names: Vec<&str> = ProviderKey::ALL.iter().map(|key| key.as_str()).collect();
        assert_eq!(
            names,
            [
                "rust-analyzer",
                "vtsls",
                "pyright",
                "jdtls",
                "csharp-ls",
                "bash-language-server",
                "clangd",
                "terraform-ls",
                "gopls",
            ]
        );
        Ok(())
    }
}
