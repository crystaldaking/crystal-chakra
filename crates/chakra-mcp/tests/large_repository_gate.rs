//! Release-only generated multi-language regression and performance gate.
//!
//! The corpus is created in a temporary Git repository. No evaluated source
//! is copied into Chakra, and the same command works without network access.

use std::error::Error;
use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chakra_domain::envelope::{TruncationCause, TruncationSection};
use chakra_domain::indexing::{
    IndexCoverage, IndexMemoryMetrics, IndexPhase, IndexPhaseMeasurement, IndexSchedulingMetrics,
};
use chakra_domain::operation::OperationContext;
use chakra_domain::query::{
    CallersRequest, ContextRequest, DiffContextRequest, QueryError, QueryService, RepoMapRequest,
    SymbolRef, SymbolSearchRequest,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::{Freshness, FreshnessRequirement, ProviderState, WorkspaceStatus};
use chakra_domain::symbol::Language;
use chakra_engine::{PreciseProvider, PreciseQueryRequest, PreciseQueryResult, WorkspaceEngine};
use chakra_language::{index_repository, start_live_index};
use chakra_mcp::ChakraMcpServer;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde::Serialize;
use tempfile::TempDir;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context as LayerContext;
use tracing_subscriber::prelude::*;

const RUST_FILES: usize = 96;
const PHP_FILES: usize = 96;
const HIGH_DEGREE_CALLERS: usize = 2_300;
const COLD_START_LIMIT: Duration = Duration::from_secs(30);
const RESTART_LIMIT: Duration = Duration::from_secs(30);
const FRESH_BARRIER_LIMIT: Duration = Duration::from_secs(5);
const EDIT_LIMIT: Duration = Duration::from_secs(5);
const DIFF_LIMIT: Duration = Duration::from_secs(5);
const PROVIDER_LIMIT: Duration = Duration::from_secs(5);
const MCP_LIMIT: Duration = Duration::from_secs(5);
const MCP_RESPONSE_LIMIT: u64 = 1024 * 1024;

type GateResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug)]
struct DegradedRustProvider;

impl PreciseProvider for DegradedRustProvider {
    fn name(&self) -> &'static str {
        "degraded-rust-provider"
    }

    fn supports(&self, language: Language) -> bool {
        language == Language::Rust
    }

    fn state_for(&self, _revision: Revision) -> ProviderState {
        ProviderState::Degraded
    }

    fn last_error(&self) -> Option<String> {
        Some("generated gate provider degradation".to_owned())
    }

    fn enrich_with_context(
        &self,
        request: PreciseQueryRequest,
        _operation: &OperationContext,
    ) -> PreciseQueryResult {
        PreciseQueryResult::unavailable(request.workspace.revision, ProviderState::Degraded)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
struct QueryTraceMetric {
    construction_micros: u64,
    files_examined: u64,
    symbols_examined: u64,
    candidates_examined: u64,
    edges_visited: u64,
    call_sites_visited: u64,
    intermediate_items_retained: u64,
    diff_wait_micros: u64,
    provider_wait_micros: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
struct McpTraceMetric {
    response_bytes: u64,
    serialization_micros: u64,
    budget_check_micros: u64,
}

#[derive(Debug, Default)]
struct TraceStore {
    queries: Vec<QueryTraceMetric>,
    mcp: Vec<McpTraceMetric>,
}

#[derive(Clone)]
struct GateMetricsLayer {
    store: Arc<Mutex<TraceStore>>,
}

#[derive(Default)]
struct GateMetricVisitor {
    query: QueryTraceMetric,
    mcp: McpTraceMetric,
    saw_query: bool,
    saw_mcp: bool,
}

impl Visit for GateMetricVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "construction_micros" => {
                self.query.construction_micros = value;
                self.saw_query = true;
            }
            "files_examined" => self.query.files_examined = value,
            "symbols_examined" => self.query.symbols_examined = value,
            "candidates_examined" => self.query.candidates_examined = value,
            "edges_visited" => self.query.edges_visited = value,
            "call_sites_visited" => self.query.call_sites_visited = value,
            "intermediate_items_retained" => self.query.intermediate_items_retained = value,
            "diff_wait_micros" => self.query.diff_wait_micros = value,
            "provider_wait_micros" => self.query.provider_wait_micros = value,
            "response_bytes" => {
                self.mcp.response_bytes = value;
                self.saw_mcp = true;
            }
            "serialization_micros" => self.mcp.serialization_micros = value,
            "budget_check_micros" => self.mcp.budget_check_micros = value,
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

impl<S> Layer<S> for GateMetricsLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _context: LayerContext<'_, S>) {
        let mut visitor = GateMetricVisitor::default();
        event.record(&mut visitor);
        if let Ok(mut store) = self.store.lock() {
            if visitor.saw_query {
                store.queries.push(visitor.query);
            }
            if visitor.saw_mcp {
                store.mcp.push(visitor.mcp);
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct MachineContext {
    chakra_commit: String,
    git_worktree_clean: bool,
    build_profile: &'static str,
    rust_toolchain: String,
    operating_system: String,
    architecture: &'static str,
    available_parallelism: u64,
    cpu_model: Option<String>,
    physical_memory_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
struct CorpusSummary {
    rust_files: u64,
    php_files: u64,
    high_degree_callers: u64,
    discovered_files: u64,
    retained_source_bytes: u64,
}

#[derive(Debug, Serialize)]
struct IndexRunSummary {
    wall_micros: u64,
    graph_fingerprint: String,
    coverage: IndexCoverage,
    scheduling: IndexSchedulingMetrics,
    memory: IndexMemoryMetrics,
    phases: Vec<IndexPhaseMeasurement>,
}

#[derive(Debug, Serialize)]
struct FreshnessSummary {
    runs: u64,
    allow_stale_query_micros: u64,
    require_fresh_min_micros: u64,
    require_fresh_median_micros: u64,
    require_fresh_max_micros: u64,
    files_inspected: u64,
    source_bytes_inspected: u64,
    metadata_files_inspected: u64,
    metadata_bytes_inspected: u64,
    git_subprocesses: u64,
    files_read: u64,
    source_bytes_read: u64,
    no_op_reconciliations: u64,
    full_reconciliations: u64,
}

#[derive(Debug, Serialize)]
struct EditSummary {
    wall_micros: u64,
    files_reparsed: u64,
    relationship_files_recomputed: u64,
    targeted_reconciliations: u64,
    full_reconciliations: u64,
    rebuilt_files: u64,
    copied_source_bytes: u64,
    copied_symbols: u64,
    copied_edges: u64,
    copied_call_sites: u64,
}

#[derive(Debug, Serialize)]
struct QuerySummary {
    ambiguity_candidates: u64,
    high_degree_callers_returned: u64,
    high_degree_truncation_cause: &'static str,
    cancelled_before_work: bool,
    degraded_provider_state: ProviderState,
    degraded_provider_fallback: bool,
    clean_diff_files: u64,
    clean_diff_micros: u64,
    changed_diff_files: u64,
    changed_diff_micros: u64,
    query_trace: QueryTraceMetric,
    provider_trace: QueryTraceMetric,
    provider_query_micros: u64,
}

#[derive(Debug, Serialize)]
struct McpSummary {
    round_trip_micros: u64,
    response_bytes: u64,
    adapter_trace: McpTraceMetric,
}

#[derive(Debug, Serialize)]
struct GateReport {
    schema_version: u32,
    machine: MachineContext,
    corpus: CorpusSummary,
    cold_start: IndexRunSummary,
    unchanged_restart: IndexRunSummary,
    no_op_freshness: FreshnessSummary,
    one_file_edit: EditSummary,
    editor_rename_replacement: EditSummary,
    queries: QuerySummary,
    mcp: McpSummary,
}

fn command_output(root: &Path, program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .current_dir(root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn git(root: &Path, args: &[&str]) -> GateResult<()> {
    let status = Command::new("git").current_dir(root).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("generated gate Git command failed: git {}", args.join(" ")).into())
    }
}

fn machine_context() -> GateResult<MachineContext> {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cpu_model = if cfg!(target_os = "macos") {
        command_output(&repository, "sysctl", &["-n", "machdep.cpu.brand_string"])
    } else {
        fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|contents| {
                contents.lines().find_map(|line| {
                    line.strip_prefix("model name")
                        .and_then(|value| value.split_once(':').map(|(_, value)| value.trim()))
                        .map(str::to_owned)
                })
            })
    };
    let physical_memory_bytes = if cfg!(target_os = "macos") {
        command_output(&repository, "sysctl", &["-n", "hw.memsize"])
            .and_then(|value| value.parse().ok())
    } else {
        fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|contents| {
                contents.lines().find_map(|line| {
                    line.strip_prefix("MemTotal:").and_then(|value| {
                        value
                            .split_whitespace()
                            .next()
                            .and_then(|kib| kib.parse::<u64>().ok())
                            .map(|kib| kib.saturating_mul(1024))
                    })
                })
            })
    };
    Ok(MachineContext {
        chakra_commit: command_output(&repository, "git", &["rev-parse", "HEAD"])
            .unwrap_or_else(|| "unknown".to_owned()),
        git_worktree_clean: Command::new("git")
            .current_dir(&repository)
            .args(["status", "--porcelain"])
            .output()
            .is_ok_and(|output| output.status.success() && output.stdout.is_empty()),
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        rust_toolchain: command_output(&repository, "rustc", &["-Vv"])
            .unwrap_or_else(|| "unknown".to_owned()),
        operating_system: command_output(&repository, "uname", &["-a"])
            .unwrap_or_else(|| std::env::consts::OS.to_owned()),
        architecture: std::env::consts::ARCH,
        available_parallelism: std::thread::available_parallelism()
            .map(|value| value.get() as u64)
            .unwrap_or(1),
        cpu_model,
        physical_memory_bytes,
    })
}

fn write_generated_repository(root: &Path) -> GateResult<PathBuf> {
    fs::create_dir_all(root.join("src/modules"))?;
    fs::create_dir_all(root.join("app/Modules"))?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"chakra-generated-gate\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[lib]\npath = \"src/lib.rs\"\n",
    )?;
    fs::write(root.join("src/lib.rs"), "pub fn gate_root() {}\n")?;

    let mut high_degree = String::from("pub fn hot() {}\n");
    for index in 0..HIGH_DEGREE_CALLERS {
        high_degree.push_str(&format!("pub fn caller_{index:04}() {{ hot(); }}\n"));
    }
    fs::write(root.join("src/high_degree.rs"), high_degree)?;

    for index in 0..RUST_FILES {
        fs::write(
            root.join(format!("src/modules/item_{index:04}.rs")),
            format!(
                "pub fn callee_{index:04}() {{}}\npub fn item_{index:04}() {{ callee_{index:04}(); }}\n"
            ),
        )?;
    }

    fs::write(
        root.join("composer.json"),
        "{\"name\":\"chakra/generated-gate\",\"autoload\":{\"psr-4\":{\"App\\\\\":\"app/\"}}}",
    )?;
    for index in 0..PHP_FILES {
        fs::write(
            root.join(format!("app/Modules/Service{index:04}.php")),
            format!(
                "<?php\nnamespace App\\Modules;\nfinal class Service{index:04} {{\n    public function run(): void {{}}\n    public function invoke(): void {{ $this->run(); }}\n}}\n"
            ),
        )?;
    }

    git(root, &["init", "--quiet"])?;
    git(
        root,
        &["config", "user.email", "chakra-gate@example.invalid"],
    )?;
    git(root, &["config", "user.name", "Chakra Gate"])?;
    git(root, &["add", "--", "."])?;
    git(root, &["commit", "--quiet", "-m", "generated scale corpus"])?;
    Ok(root.join("src/modules/item_0000.rs"))
}

fn graph_fingerprint(graph: &chakra_engine::SymbolGraph) -> String {
    let mut fingerprint = std::collections::hash_map::DefaultHasher::new();
    fingerprint.write(format!("{:?}", graph.file_summaries()).as_bytes());
    for symbol in graph.symbols() {
        fingerprint.write(format!("{symbol:?}").as_bytes());
        fingerprint.write(format!("{:?}", graph.outgoing_edges(symbol.id)).as_bytes());
        fingerprint.write(
            format!("{:?}", graph.call_sites_from(symbol.id).collect::<Vec<_>>()).as_bytes(),
        );
    }
    format!("{:016x}", fingerprint.finish())
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn metric_at_most(name: &str, observed: u64, limit: u64) -> GateResult<()> {
    if observed <= limit {
        Ok(())
    } else {
        Err(
            format!("large-repository gate exceeded `{name}`: observed={observed}, limit={limit}")
                .into(),
        )
    }
}

fn require(condition: bool, message: impl Into<String>) -> GateResult<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

fn index_summary(report: &chakra_language::IndexReport) -> IndexRunSummary {
    IndexRunSummary {
        wall_micros: micros(report.metrics.elapsed),
        graph_fingerprint: graph_fingerprint(&report.graph),
        coverage: report.metrics.indexing.coverage.clone(),
        scheduling: report.metrics.indexing.scheduling.clone(),
        memory: report.metrics.indexing.memory.clone(),
        phases: report.metrics.indexing.phases.clone(),
    }
}

fn edit_summary(
    elapsed: Duration,
    before: chakra_language::LiveIndexMetrics,
    after: chakra_language::LiveIndexMetrics,
    snapshot: &chakra_engine::WorkspaceSnapshot,
) -> EditSummary {
    let publication = snapshot.indexing().publication;
    EditSummary {
        wall_micros: micros(elapsed),
        files_reparsed: after.files_reparsed.saturating_sub(before.files_reparsed),
        relationship_files_recomputed: after
            .relationship_files_recomputed
            .saturating_sub(before.relationship_files_recomputed),
        targeted_reconciliations: after
            .targeted_reconciliations
            .saturating_sub(before.targeted_reconciliations),
        full_reconciliations: after
            .full_reconciliations
            .saturating_sub(before.full_reconciliations),
        rebuilt_files: publication.rebuilt_files,
        copied_source_bytes: publication.copied_source_bytes,
        copied_symbols: publication.copied_symbols,
        copied_edges: publication.copied_edges,
        copied_call_sites: publication.copied_call_sites,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "release-only generated v0.1.1 large-repository gate"]
async fn generated_multi_language_release_gate() -> GateResult<()> {
    require(
        !cfg!(debug_assertions),
        "large-repository gate must run with `cargo test --release`",
    )?;
    let traces = Arc::new(Mutex::new(TraceStore::default()));
    tracing_subscriber::registry()
        .with(GateMetricsLayer {
            store: traces.clone(),
        })
        .try_init()
        .map_err(|error| std::io::Error::other(error.to_string()))?;

    let temporary = TempDir::new()?;
    let repository = temporary.path().join("generated-repository");
    fs::create_dir_all(&repository)?;
    let edit_target = write_generated_repository(&repository)?;

    let cold = index_repository(&repository)?;
    metric_at_most(
        "cold_start_micros",
        micros(cold.metrics.elapsed),
        micros(COLD_START_LIMIT),
    )?;
    cold.graph.validate_consistency()?;
    let status = &cold.metrics.indexing;
    require(
        status.coverage.indexed_files == status.coverage.discovered_files,
        format!(
            "generated corpus file coverage is incomplete: indexed={}, discovered={}",
            status.coverage.indexed_files, status.coverage.discovered_files
        ),
    )?;
    require(
        !status.is_degraded(),
        format!("generated cold index degraded: {:?}", status.degradations),
    )?;
    for phase in [
        IndexPhase::GitInventory,
        IndexPhase::SourceRead,
        IndexPhase::ParseExtraction,
        IndexPhase::SymbolCatalog,
        IndexPhase::Relationships,
        IndexPhase::GraphMaterialization,
        IndexPhase::LanguageComposition,
        IndexPhase::GraphValidation,
    ] {
        require(
            status
                .phases
                .iter()
                .any(|measurement| measurement.phase == phase),
            format!("generated cold index omitted phase metric {phase:?}"),
        )?;
    }
    if let Some(peak_rss) = status.memory.observed_phase_peak_rss_bytes {
        metric_at_most(
            "observed_phase_peak_rss_bytes",
            peak_rss,
            status.budgets.memory_target_bytes,
        )?;
    }

    let restart = index_repository(&repository)?;
    metric_at_most(
        "unchanged_restart_micros",
        micros(restart.metrics.elapsed),
        micros(RESTART_LIMIT),
    )?;
    require(
        graph_fingerprint(&cold.graph) == graph_fingerprint(&restart.graph),
        "unchanged restart produced a different graph fingerprint",
    )?;

    let corpus = CorpusSummary {
        rust_files: cold.metrics.rust_files,
        php_files: cold.metrics.php_files,
        high_degree_callers: HIGH_DEGREE_CALLERS as u64,
        discovered_files: status.coverage.discovered_files,
        retained_source_bytes: status.coverage.source_bytes,
    };
    let cold_summary = index_summary(&cold);
    let restart_summary = index_summary(&restart);

    let identity = chakra_git::resolve_workspace_identity(&repository)?;
    let engine = Arc::new(WorkspaceEngine::new(identity));
    engine.install_diff_provider(Arc::new(chakra_git::GitWorkspaceDiff))?;
    engine.install_precise_provider(Arc::new(DegradedRustProvider))?;
    let mut update = engine.begin_update();
    update.replace_graph(cold.graph);
    update.set_indexing(cold.metrics.indexing);
    update.set_status(WorkspaceStatus::Indexing);
    update.set_freshness(Freshness::Stale);
    engine.publish(update)?;
    let live = start_live_index(repository.clone(), cold.syntax_index, engine.clone())?;

    engine.repo_map(RepoMapRequest {
        limit: Some(20),
        freshness: FreshnessRequirement::RequireFresh,
        ..RepoMapRequest::default()
    })?;
    let clean_diff_started = Instant::now();
    let clean_diff = engine.diff_context(DiffContextRequest::default())?;
    let clean_diff_elapsed = clean_diff_started.elapsed();
    metric_at_most(
        "clean_diff_micros",
        micros(clean_diff_elapsed),
        micros(DIFF_LIMIT),
    )?;
    require(
        clean_diff.data.changed_files.is_empty(),
        "clean generated worktree returned changed files",
    )?;

    let allow_stale_started = Instant::now();
    let pure_query = engine.symbol_search(SymbolSearchRequest {
        query: "hot".to_owned(),
        limit: Some(1),
        freshness: FreshnessRequirement::AllowStale,
        ..SymbolSearchRequest::default()
    })?;
    let allow_stale_query_micros = micros(allow_stale_started.elapsed());
    require(
        pure_query.data.candidates.len() == 1,
        "allow-stale high-degree target lookup failed",
    )?;

    const FRESH_RUNS: usize = 5;
    let no_op_before = live.metrics();
    let mut fresh_samples = Vec::with_capacity(FRESH_RUNS);
    for _ in 0..FRESH_RUNS {
        let started = Instant::now();
        engine.symbol_search(SymbolSearchRequest {
            query: "hot".to_owned(),
            limit: Some(1),
            freshness: FreshnessRequirement::RequireFresh,
            ..SymbolSearchRequest::default()
        })?;
        fresh_samples.push(started.elapsed());
    }
    fresh_samples.sort_unstable();
    let no_op_after = live.metrics();
    metric_at_most(
        "require_fresh_max_micros",
        micros(*fresh_samples.last().ok_or("freshness samples missing")?),
        micros(FRESH_BARRIER_LIMIT),
    )?;
    require(
        no_op_after.files_read == no_op_before.files_read
            && no_op_after.source_bytes_read == no_op_before.source_bytes_read,
        "no-op freshness read stable source bodies",
    )?;
    require(
        no_op_after.full_reconciliations == no_op_before.full_reconciliations,
        "no-op freshness triggered full reconciliation",
    )?;
    let freshness = FreshnessSummary {
        runs: FRESH_RUNS as u64,
        allow_stale_query_micros,
        require_fresh_min_micros: micros(fresh_samples[0]),
        require_fresh_median_micros: micros(fresh_samples[FRESH_RUNS / 2]),
        require_fresh_max_micros: micros(*fresh_samples.last().ok_or("freshness samples missing")?),
        files_inspected: no_op_after
            .files_inspected
            .saturating_sub(no_op_before.files_inspected),
        source_bytes_inspected: no_op_after
            .source_bytes_inspected
            .saturating_sub(no_op_before.source_bytes_inspected),
        metadata_files_inspected: no_op_after
            .metadata_files_inspected
            .saturating_sub(no_op_before.metadata_files_inspected),
        metadata_bytes_inspected: no_op_after
            .metadata_bytes_inspected
            .saturating_sub(no_op_before.metadata_bytes_inspected),
        git_subprocesses: no_op_after
            .git_subprocesses
            .saturating_sub(no_op_before.git_subprocesses),
        files_read: no_op_after
            .files_read
            .saturating_sub(no_op_before.files_read),
        source_bytes_read: no_op_after
            .source_bytes_read
            .saturating_sub(no_op_before.source_bytes_read),
        no_op_reconciliations: no_op_after
            .no_op_reconciliations
            .saturating_sub(no_op_before.no_op_reconciliations),
        full_reconciliations: no_op_after
            .full_reconciliations
            .saturating_sub(no_op_before.full_reconciliations),
    };

    let ambiguous = engine.symbol_search(SymbolSearchRequest {
        query: "run".to_owned(),
        limit: Some(20),
        freshness: FreshnessRequirement::AllowStale,
        ..SymbolSearchRequest::default()
    })?;
    require(
        ambiguous.data.candidates.len() > 1,
        "generated PHP ambiguity was not retained",
    )?;
    require(
        matches!(
            engine.context(ContextRequest {
                symbol: Some(SymbolRef::ByName("run".to_owned())),
                freshness: FreshnessRequirement::AllowStale,
                ..ContextRequest::default()
            }),
            Err(QueryError::AmbiguousSymbol { .. })
        ),
        "ambiguous name was guessed instead of rejected",
    )?;

    let callers = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName("hot".to_owned())),
        limit: Some(20),
        freshness: FreshnessRequirement::AllowStale,
    })?;
    require(
        callers.truncation.iter().any(|detail| {
            detail.section == TruncationSection::CallersCallers
                && detail.cause == TruncationCause::GraphTraversalLimit
        }),
        "high-degree callers did not report graph traversal truncation",
    )?;
    let high_degree_trace = traces
        .lock()
        .ok()
        .and_then(|store| store.queries.last().cloned())
        .ok_or("query instrumentation event missing")?;
    require(
        high_degree_trace.edges_visited > 0 && high_degree_trace.edges_visited <= 2_048,
        format!(
            "high-degree query visited unexpected edge count: {}",
            high_degree_trace.edges_visited
        ),
    )?;

    let provider_started = Instant::now();
    let degraded_context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("hot".to_owned())),
        limit: Some(20),
        freshness: FreshnessRequirement::RequireFresh,
    })?;
    let provider_elapsed = provider_started.elapsed();
    metric_at_most(
        "degraded_provider_query_micros",
        micros(provider_elapsed),
        micros(PROVIDER_LIMIT),
    )?;
    let degraded_provider = degraded_context
        .data
        .provider
        .as_ref()
        .ok_or("degraded provider metadata missing")?;
    let degraded_provider_state = degraded_provider.state;
    let degraded_provider_fallback =
        degraded_provider_state == ProviderState::Degraded && degraded_provider.fallback_used;
    require(
        degraded_provider_fallback,
        "degraded provider did not preserve an explicit syntax fallback",
    )?;
    let provider_trace = traces
        .lock()
        .ok()
        .and_then(|store| store.queries.last().cloned())
        .ok_or("provider query instrumentation event missing")?;

    let cancelled = OperationContext::unbounded();
    cancelled.cancel();
    let cancelled_before_work = matches!(
        engine.repo_map_with_context(RepoMapRequest::default(), &cancelled),
        Err(QueryError::Cancelled)
    );
    require(cancelled_before_work, "cancelled query executed work")?;

    let edit_before = live.metrics();
    let edited_source = fs::read_to_string(&edit_target)?.replace("callee_0000();", "hot();");
    fs::write(&edit_target, &edited_source)?;
    let edit_started = Instant::now();
    engine.symbol_search(SymbolSearchRequest {
        query: "item_0000".to_owned(),
        limit: Some(1),
        freshness: FreshnessRequirement::RequireFresh,
        ..SymbolSearchRequest::default()
    })?;
    let edit_elapsed = edit_started.elapsed();
    metric_at_most(
        "one_file_edit_micros",
        micros(edit_elapsed),
        micros(EDIT_LIMIT),
    )?;
    let edit_after = live.metrics();
    let edit_snapshot = engine.snapshot();
    let one_file_edit = edit_summary(edit_elapsed, edit_before, edit_after, &edit_snapshot);
    require(
        one_file_edit.files_reparsed == 1
            && one_file_edit.relationship_files_recomputed > 0
            && one_file_edit.rebuilt_files == 1
            && one_file_edit.full_reconciliations == 0,
        format!("one-file edit was not structurally incremental: {one_file_edit:?}"),
    )?;
    require(
        one_file_edit.copied_source_bytes == 0
            && one_file_edit.copied_symbols == 0
            && one_file_edit.copied_call_sites == 0,
        format!("one-file edit copied retained heavy graph payloads: {one_file_edit:?}"),
    )?;
    let changed_diff_started = Instant::now();
    let changed_diff = engine.diff_context(DiffContextRequest::default())?;
    let changed_diff_elapsed = changed_diff_started.elapsed();
    metric_at_most(
        "changed_diff_micros",
        micros(changed_diff_elapsed),
        micros(DIFF_LIMIT),
    )?;
    require(
        changed_diff
            .data
            .changed_files
            .iter()
            .any(|change| change.path.as_str() == "src/modules/item_0000.rs"),
        "changed diff omitted the edited source",
    )?;

    let rename_before = live.metrics();
    let replacement = edit_target.with_extension("rs.chakra-replacement");
    fs::write(
        &replacement,
        format!("{edited_source}\n// editor-style atomic replacement\n"),
    )?;
    fs::rename(&replacement, &edit_target)?;
    let rename_started = Instant::now();
    engine.repo_map(RepoMapRequest {
        limit: Some(20),
        freshness: FreshnessRequirement::RequireFresh,
        ..RepoMapRequest::default()
    })?;
    let rename_elapsed = rename_started.elapsed();
    metric_at_most(
        "editor_rename_replacement_micros",
        micros(rename_elapsed),
        micros(EDIT_LIMIT),
    )?;
    let rename_after = live.metrics();
    let rename_snapshot = engine.snapshot();
    let editor_rename_replacement = edit_summary(
        rename_elapsed,
        rename_before,
        rename_after,
        &rename_snapshot,
    );
    require(
        editor_rename_replacement.files_reparsed == 1
            && editor_rename_replacement.rebuilt_files == 1
            && editor_rename_replacement.full_reconciliations == 0,
        format!("editor rename replacement was not targeted: {editor_rename_replacement:?}"),
    )?;

    let server = ChakraMcpServer::new(engine.clone());
    let (server_transport, client_transport) = tokio::io::duplex(1024 * 1024);
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });
    let client = ().serve(client_transport).await?;
    let mcp_started = Instant::now();
    let mcp_response = client
        .call_tool(
            CallToolRequestParams::new("repo_map")
                .with_arguments(serde_json::from_value(serde_json::json!({ "limit": 20 }))?),
        )
        .await?
        .structured_content
        .ok_or("generated gate MCP response missing")?;
    let mcp_round_trip = mcp_started.elapsed();
    metric_at_most(
        "mcp_round_trip_micros",
        micros(mcp_round_trip),
        micros(MCP_LIMIT),
    )?;
    let mcp_response_bytes = serde_json::to_vec(&mcp_response)?.len() as u64;
    metric_at_most("mcp_response_bytes", mcp_response_bytes, MCP_RESPONSE_LIMIT)?;
    let mcp_trace = traces
        .lock()
        .ok()
        .and_then(|store| store.mcp.last().cloned())
        .ok_or("MCP serialization instrumentation event missing")?;
    require(
        mcp_trace.response_bytes == mcp_response_bytes,
        format!(
            "MCP response metric mismatch: trace={}, encoded={mcp_response_bytes}",
            mcp_trace.response_bytes
        ),
    )?;
    client.cancel().await?;
    let running = server_task
        .await
        .map_err(|error| std::io::Error::other(format!("server task join: {error}")))?
        .map_err(|error| std::io::Error::other(format!("server serve: {error}")))?;
    running.cancel().await?;

    live.shutdown()?;
    let report = GateReport {
        schema_version: 1,
        machine: machine_context()?,
        corpus,
        cold_start: cold_summary,
        unchanged_restart: restart_summary,
        no_op_freshness: freshness,
        one_file_edit,
        editor_rename_replacement,
        queries: QuerySummary {
            ambiguity_candidates: ambiguous.data.candidates.len() as u64,
            high_degree_callers_returned: callers.data.callers.len() as u64,
            high_degree_truncation_cause: "graph_traversal_limit",
            cancelled_before_work,
            degraded_provider_state,
            degraded_provider_fallback,
            clean_diff_files: clean_diff.data.changed_files.len() as u64,
            clean_diff_micros: micros(clean_diff_elapsed),
            changed_diff_files: changed_diff.data.changed_files.len() as u64,
            changed_diff_micros: micros(changed_diff_elapsed),
            query_trace: high_degree_trace,
            provider_trace,
            provider_query_micros: micros(provider_elapsed),
        },
        mcp: McpSummary {
            round_trip_micros: micros(mcp_round_trip),
            response_bytes: mcp_response_bytes,
            adapter_trace: mcp_trace,
        },
    };
    println!("CHAKRA_V0_1_1_GATE={}", serde_json::to_string(&report)?);
    Ok(())
}
