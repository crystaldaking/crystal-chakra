//! Corpus scenario implementations: one pinned public repository at a time.
//!
//! Edit scenarios mutate the cached checkout directly (copying multi-hundred-
//! MB checkouts per run would dominate wall time and disk). Every mutation is
//! reverted before the run ends: tracked files via `git checkout -- .`,
//! runner-created untracked files by explicit removal (never `git clean`, so
//! the fetch tool's `.chakra-corpus.json` metadata survives). The
//! `cache-restore` scenario proves the checkout is back at the pinned SHA
//! with a clean worktree.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use chakra_domain::envelope::TruncationSection;
use chakra_domain::indexing::{IndexBudgetKind, IndexBudgets, IndexCancellation};
use chakra_domain::operation::OperationContext;
use chakra_domain::query::{
    CallersRequest, ChangeKind, ContextRequest, DiffContextRequest, QueryError, QueryService,
    RepoMapRequest, StatusRequest, SymbolRef, SymbolSearchRequest,
};
use chakra_domain::state::{Freshness, FreshnessRequirement, WorkspaceStatus};
use chakra_domain::symbol::{EdgeKind, SymbolKind};
use chakra_engine::{SymbolGraph, WorkspaceEngine};
use chakra_language::{
    IndexOptions, IndexReport, LiveIndex, LiveIndexMetrics, WorkspaceIndexError, index_repository,
    index_repository_with_options, start_live_index,
};

use super::manifest::{CorpusBudgets, CorpusManifest, CorpusRepository, LanguageBudgets};
use super::report::{
    BudgetStatus, BudgetVerdict, CorpusRepoReport, CorpusScenarioReport, CorpusScenarioStatus,
    PhaseTiming, RepoStatus, reject,
};
use super::{SCENARIO_IDS, supported_languages};
use crate::{Check, failure};

/// Untracked metadata file written by `tools/fetch_corpus.py`; excluded from
/// the clean-worktree assertion.
const CACHE_METADATA_FILE: &str = ".chakra-corpus.json";

/// Runs every repository of one manifest language. Unsupported languages and
/// uncached/mismatched checkouts produce skipped reports, never errors.
pub fn evaluate_language(
    language: &str,
    manifest: &CorpusManifest,
    budgets: &CorpusBudgets,
    cache_root: &Path,
) -> Check<Vec<CorpusRepoReport>> {
    let entry = manifest.languages.get(language).ok_or_else(|| {
        failure(format!(
            "language `{language}` is not in the corpus manifest"
        ))
    })?;
    if !supported_languages()
        .iter()
        .any(|supported| supported == language)
    {
        return Ok(entry
            .repositories
            .iter()
            .map(|repo| {
                skipped_repo(
                    language,
                    repo,
                    format!(
                        "unsupported language: `{language}` has no chakra-language adapter yet"
                    ),
                )
            })
            .collect());
    }
    let budgets = budgets.for_language(language);
    Ok(entry
        .repositories
        .iter()
        .map(|repo| evaluate_repository(language, repo, budgets, cache_root))
        .collect())
}

fn skipped_repo(language: &str, repo: &CorpusRepository, reason: String) -> CorpusRepoReport {
    let scenarios = SCENARIO_IDS
        .iter()
        .map(|id| CorpusScenarioReport {
            id: (*id).to_owned(),
            status: CorpusScenarioStatus::Skipped,
            details: reason.clone(),
            phases: Vec::new(),
            measurements: BTreeMap::new(),
            budget_verdicts: Vec::new(),
        })
        .collect();
    CorpusRepoReport::new(
        language,
        &repo.name,
        &repo.sha,
        RepoStatus::Skipped,
        reason,
        scenarios,
    )
}

/// Accumulates one report per catalog id; [`Slots::finish`] emits the
/// catalog order and marks unreached scenarios as skipped.
struct Slots {
    reports: BTreeMap<String, CorpusScenarioReport>,
}

impl Slots {
    fn new() -> Self {
        Self {
            reports: BTreeMap::new(),
        }
    }

    fn put(&mut self, report: CorpusScenarioReport) {
        self.reports.insert(report.id.clone(), report);
    }

    /// Runs `body`, converting any error into a `fail` report.
    fn run(&mut self, id: &'static str, body: impl FnOnce(&mut ScenarioBuilder) -> Check<()>) {
        let mut builder = ScenarioBuilder::new(id);
        let report = match body(&mut builder) {
            Ok(()) => builder.finish(),
            Err(error) => builder.fail(error.to_string()),
        };
        self.put(report);
    }

    fn skip(&mut self, id: &'static str, reason: &str) {
        self.put(CorpusScenarioReport {
            id: id.to_owned(),
            status: CorpusScenarioStatus::Skipped,
            details: reason.to_owned(),
            phases: Vec::new(),
            measurements: BTreeMap::new(),
            budget_verdicts: Vec::new(),
        });
    }

    fn skip_all_missing(&mut self, reason: &str) {
        for id in SCENARIO_IDS {
            if !self.reports.contains_key(*id) {
                self.skip(id, reason);
            }
        }
    }

    fn finish(mut self) -> Vec<CorpusScenarioReport> {
        self.skip_all_missing("scenario not reached");
        SCENARIO_IDS
            .iter()
            .filter_map(|id| self.reports.remove(*id))
            .collect()
    }
}

/// Collects phases, measurements, budget verdicts, and notes for one
/// scenario while its body runs.
struct ScenarioBuilder {
    id: &'static str,
    phases: Vec<PhaseTiming>,
    measurements: BTreeMap<String, Value>,
    verdicts: Vec<BudgetVerdict>,
    notes: Vec<String>,
}

impl ScenarioBuilder {
    fn new(id: &'static str) -> Self {
        Self {
            id,
            phases: Vec::new(),
            measurements: BTreeMap::new(),
            verdicts: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Records a phase wall time measured from `started`.
    fn phase(&mut self, name: &str, started: Instant) {
        self.phases.push(PhaseTiming {
            name: name.to_owned(),
            wall_micros: micros(started.elapsed()),
        });
    }

    fn measure(&mut self, key: &str, value: impl Into<Value>) {
        self.measurements.insert(key.to_owned(), value.into());
    }

    fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    /// Records a budget comparison. A missing per-language budget is a note,
    /// not a verdict.
    fn budget(
        &mut self,
        budgets: Option<LanguageBudgets>,
        name: &str,
        observed: u64,
        limit: impl FnOnce(&LanguageBudgets) -> u64,
    ) {
        match budgets {
            Some(budgets) => {
                let limit = limit(&budgets);
                self.verdicts.push(BudgetVerdict {
                    budget: name.to_owned(),
                    observed,
                    limit,
                    status: if observed <= limit {
                        BudgetStatus::Pass
                    } else {
                        BudgetStatus::Fail
                    },
                });
            }
            None => self.note(format!("no budget configured for {name}")),
        }
    }

    fn build(self, status: CorpusScenarioStatus, details: String) -> CorpusScenarioReport {
        CorpusScenarioReport {
            id: self.id.to_owned(),
            status,
            details,
            phases: self.phases,
            measurements: self.measurements,
            budget_verdicts: self.verdicts,
        }
    }

    fn finish(self) -> CorpusScenarioReport {
        let exceeded: Vec<String> = self
            .verdicts
            .iter()
            .filter(|verdict| verdict.status == BudgetStatus::Fail)
            .map(|verdict| {
                format!(
                    "budget {} exceeded in scenario {}: observed={} limit={}",
                    verdict.budget, self.id, verdict.observed, verdict.limit
                )
            })
            .collect();
        let notes = self.notes.join("; ");
        if exceeded.is_empty() {
            self.build(CorpusScenarioStatus::Pass, notes)
        } else {
            self.build(CorpusScenarioStatus::Fail, exceeded.join("; "))
        }
    }

    fn fail(self, error: String) -> CorpusScenarioReport {
        self.build(CorpusScenarioStatus::Fail, error)
    }
}

/// A running live workspace over the cached checkout.
struct LiveWorkspace {
    engine: Arc<WorkspaceEngine>,
    live: LiveIndex,
}

/// Facts about the indexed workspace, captured before any mutation.
struct WorkspaceFacts {
    /// Up to three uniquely-named high-degree callees: `(qualified, callers)`.
    targets: Vec<(String, u64)>,
    /// `(qualified, simple)` name of one symbol declared in the rename file.
    rename_symbol: Option<(String, String)>,
}

impl WorkspaceFacts {
    /// A symbol name that must survive an unrelated file's syntax error.
    fn retained_query(&self) -> Option<String> {
        self.targets
            .first()
            .map(|(qualified, _)| simple_name(qualified).to_owned())
            .or_else(|| {
                self.rename_symbol
                    .as_ref()
                    .map(|(_, simple)| simple.clone())
            })
    }
}

/// Per-language probe plan: which cached files the edit scenarios touch and
/// what they append.
struct ProbePlan {
    edit_file: String,
    rename_file: String,
    renamed_file: String,
    syntax_file: String,
    swap_file: String,
    declaration_one: String,
    declaration_two: String,
    broken: String,
}

impl ProbePlan {
    /// Picks three suitable source files of `language` from the cold index.
    /// PHP probe files must not close the `?>` tag, otherwise appended probe
    /// code would be inline HTML.
    fn select(language: &str, checkout: &Path, cold: &IndexReport) -> Check<Self> {
        let extension = match language {
            "rust" => "rs",
            "php" => "php",
            "typescript" => "ts",
            other => return Err(failure(format!("no probe plan for language `{other}`")).into()),
        };
        let mut paths: Vec<String> = cold
            .syntax_index
            .paths()
            .iter()
            .map(|path| path.as_str().to_owned())
            .filter(|path| path.ends_with(&format!(".{extension}")))
            .collect();
        paths.sort();
        let mut suitable = Vec::new();
        for path in paths {
            if suitable.len() == 3 {
                break;
            }
            let content = fs::read_to_string(checkout.join(&path))?;
            let usable = !content.trim().is_empty()
                && (language != "php" || !content.trim_end().ends_with("?>"));
            if usable {
                suitable.push(path);
            }
        }
        if suitable.len() < 3 {
            return Err(failure(format!(
                "corpus probe needs at least three indexable {language} source files, found {}",
                suitable.len()
            ))
            .into());
        }
        let edit_file = suitable[0].clone();
        let rename_file = suitable[1].clone();
        let syntax_file = suitable[2].clone();
        let renamed_file = renamed_sibling(&rename_file)?;
        let swap_file = match edit_file.rsplit_once('/') {
            Some((directory, name)) => format!("{directory}/.{name}.chakra-swap"),
            None => format!(".{edit_file}.chakra-swap"),
        };
        let (declaration_one, declaration_two, broken) = match language {
            "rust" => (
                "\npub fn chakra_corpus_probe_one() {}\n".to_owned(),
                "\npub fn chakra_corpus_probe_two() {}\n".to_owned(),
                "\npub fn chakra_corpus_broken( {\n".to_owned(),
            ),
            "php" => (
                "\nfunction chakra_corpus_probe_one(): void {}\n".to_owned(),
                "\nfunction chakra_corpus_probe_two(): void {}\n".to_owned(),
                "\nfunction chakra_corpus_broken( {\n".to_owned(),
            ),
            "typescript" => (
                "\nexport function chakra_corpus_probe_one(): void {}\n".to_owned(),
                "\nexport function chakra_corpus_probe_two(): void {}\n".to_owned(),
                "\nexport function chakra_corpus_broken( {\n".to_owned(),
            ),
            other => return Err(failure(format!("no probe plan for language `{other}`")).into()),
        };
        Ok(Self {
            edit_file,
            rename_file,
            renamed_file,
            syntax_file,
            swap_file,
            declaration_one,
            declaration_two,
            broken,
        })
    }

    /// Untracked paths the runner may have created (best-effort cleanup).
    fn created_paths(&self) -> [&str; 2] {
        [self.renamed_file.as_str(), self.swap_file.as_str()]
    }
}

/// `<dir>/<stem>__chakra_moved.<ext>` for the rename scenario.
fn renamed_sibling(path: &str) -> Check<String> {
    let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
    let (stem, extension) = name
        .rsplit_once('.')
        .ok_or_else(|| failure(format!("rename file `{path}` has no extension")))?;
    let renamed = format!("{stem}__chakra_moved.{extension}");
    Ok(if directory.is_empty() {
        renamed
    } else {
        format!("{directory}/{renamed}")
    })
}

fn evaluate_repository(
    language: &str,
    repo: &CorpusRepository,
    budgets: Option<LanguageBudgets>,
    cache_root: &Path,
) -> CorpusRepoReport {
    let checkout = cache_root.join(repo.slug());
    let mut slots = Slots::new();
    if !checkout.is_dir() {
        return skipped_repo(
            language,
            repo,
            format!(
                "checkout not cached at {}; fetch with `python3 tools/fetch_corpus.py --language {language}`",
                checkout.display()
            ),
        );
    }
    let head = match git(&checkout, &["rev-parse", "HEAD"]) {
        Ok(head) => head,
        Err(error) => {
            return skipped_repo(
                language,
                repo,
                format!("cannot read checkout HEAD: {error}"),
            );
        }
    };
    if head != repo.sha {
        return skipped_repo(
            language,
            repo,
            format!(
                "checkout HEAD {head} does not match pinned SHA {}; refusing to evaluate (re-fetch with tools/fetch_corpus.py)",
                repo.sha
            ),
        );
    }

    // --- cold-index -------------------------------------------------------
    let cold_started = Instant::now();
    let cold = match index_repository_with_options(&checkout, IndexOptions::default()) {
        Ok(report) => report,
        Err(error) => {
            let mut builder = ScenarioBuilder::new("cold-index");
            builder.phase("index", cold_started);
            slots.put(builder.fail(format!("cold index failed: {error}")));
            slots.skip_all_missing("cold index failed");
            slots.run("cache-restore", |scenario| {
                verify_clean_cache(&checkout, repo, scenario)
            });
            return finish_repo(language, repo, slots);
        }
    };
    record_cold_index(&mut slots, cold_started, &cold, budgets);

    // --- fingerprint ------------------------------------------------------
    let fingerprint = graph_fingerprint(&cold.graph);
    slots.run("fingerprint", |scenario| {
        let started = Instant::now();
        let rerun = index_repository(&checkout)?;
        scenario.phase("reindex", started);
        let rerun_fingerprint = graph_fingerprint(&rerun.graph);
        scenario.measure("fingerprint", fingerprint.clone());
        scenario.measure("rerun_symbols", rerun.metrics.symbols);
        scenario.measure("rerun_edges", rerun.metrics.edges);
        reject(
            rerun_fingerprint == fingerprint,
            format!("non-deterministic index: {fingerprint} vs {rerun_fingerprint}"),
        )?;
        reject(
            rerun.metrics.symbols == cold.metrics.symbols
                && rerun.metrics.edges == cold.metrics.edges,
            format!(
                "second run counts differ: symbols {} vs {}, edges {} vs {}",
                rerun.metrics.symbols,
                cold.metrics.symbols,
                rerun.metrics.edges,
                cold.metrics.edges
            ),
        )?;
        scenario.note("two cold indexes produce identical fingerprints and counts");
        Ok(())
    });

    // --- probe plan and live workspace ------------------------------------
    let live_setup = ProbePlan::select(language, &checkout, &cold)
        .and_then(|plan| start_live_workspace(&checkout, cold).map(|live| (plan, live)));
    let (plan, workspace) = match live_setup {
        Ok(pair) => pair,
        Err(error) => {
            let reason = format!("live workspace setup failed: {error}");
            for id in [
                "warm-noop",
                "one-file-edit",
                "atomic-replace",
                "rename-delete",
                "syntax-error",
                "diff-context",
                "queries",
                "cancellation",
            ] {
                slots.skip(id, &reason);
            }
            slots.run("cache-restore", |scenario| {
                verify_clean_cache(&checkout, repo, scenario)
            });
            return finish_repo(language, repo, slots);
        }
    };
    let facts = collect_facts(&workspace.engine, &plan);

    // --- warm-noop --------------------------------------------------------
    slots.run("warm-noop", |scenario| {
        let before = workspace.live.metrics();
        let started = Instant::now();
        workspace.engine.symbol_search(SymbolSearchRequest {
            query: "chakra".to_owned(),
            limit: Some(1),
            freshness: FreshnessRequirement::RequireFresh,
            ..SymbolSearchRequest::default()
        })?;
        scenario.phase("fresh_barrier", started);
        scenario.measure("wall_micros", micros(started.elapsed()));
        let after = workspace.live.metrics();
        scenario.measure(
            "files_inspected",
            after.files_inspected.saturating_sub(before.files_inspected),
        );
        scenario.measure(
            "git_subprocesses",
            after
                .git_subprocesses
                .saturating_sub(before.git_subprocesses),
        );
        scenario.measure(
            "no_op_reconciliations",
            after
                .no_op_reconciliations
                .saturating_sub(before.no_op_reconciliations),
        );
        reject(
            after.files_read == before.files_read
                && after.source_bytes_read == before.source_bytes_read,
            "warm no-op re-read source bodies",
        )?;
        reject(
            after.full_reconciliations == before.full_reconciliations,
            "warm no-op triggered a full reconciliation",
        )?;
        scenario.budget(
            budgets,
            "warm_noop_wall_micros",
            micros(started.elapsed()),
            |budget| budget.warm_noop_wall_micros,
        );
        scenario.note("unchanged checkout: no reindex work behind the freshness barrier");
        Ok(())
    });

    // --- queries ----------------------------------------------------------
    slots.run("queries", |scenario| {
        run_query_scenario(scenario, &workspace, &facts)
    });

    // --- edit scenarios, diff-context, cache restore ----------------------
    run_mutation_scenarios(&mut slots, &checkout, &workspace, &plan, &facts, repo);

    // --- syntax-error -----------------------------------------------------
    slots.run("syntax-error", |scenario| {
        run_syntax_error_scenario(scenario, &checkout, &workspace, &plan, &facts)
    });

    // --- cancellation -----------------------------------------------------
    slots.run("cancellation", |scenario| {
        // Index-level: a pre-cancelled token must stop the cold index before
        // any scan work, promptly.
        let token = IndexCancellation::default();
        token.cancel();
        let started = Instant::now();
        let outcome = index_repository_with_options(
            &checkout,
            IndexOptions::new(IndexBudgets::default(), token)?,
        );
        scenario.phase("cancelled_cold_index", started);
        scenario.measure("cancelled_index_wall_micros", micros(started.elapsed()));
        reject(
            matches!(outcome, Err(WorkspaceIndexError::Cancelled)),
            "pre-cancelled cold index did not stop with WorkspaceIndexError::Cancelled",
        )?;
        // Query-level: a cancelled context must fail the query without
        // publishing a partial revision.
        let revision_before = workspace.engine.snapshot().revision();
        let operation = OperationContext::unbounded();
        operation.cancel();
        let started = Instant::now();
        let outcome = workspace
            .engine
            .repo_map_with_context(RepoMapRequest::default(), &operation);
        scenario.phase("cancelled_query", started);
        reject(
            matches!(outcome, Err(QueryError::Cancelled)),
            "cancelled repo_map did not fail with QueryError::Cancelled",
        )?;
        reject(
            workspace.engine.snapshot().revision() == revision_before,
            "cancelled query published a partial revision",
        )?;
        scenario.note("cancelled index and query stop promptly; no partial publication");
        Ok(())
    });

    workspace.shutdown();
    finish_repo(language, repo, slots)
}

fn finish_repo(language: &str, repo: &CorpusRepository, slots: Slots) -> CorpusRepoReport {
    CorpusRepoReport::new(
        language,
        &repo.name,
        &repo.sha,
        RepoStatus::Evaluated,
        String::new(),
        slots.finish(),
    )
}

fn record_cold_index(
    slots: &mut Slots,
    started: Instant,
    cold: &IndexReport,
    budgets: Option<LanguageBudgets>,
) {
    let wall_micros = micros(started.elapsed());
    let mut builder = ScenarioBuilder::new("cold-index");
    for phase in &cold.metrics.indexing.phases {
        let name = match phase.language {
            Some(language) => format!("{:?}/{language:?}", phase.phase),
            None => format!("{:?}", phase.phase),
        };
        builder.phases.push(PhaseTiming {
            name,
            wall_micros: phase.elapsed_micros,
        });
    }
    builder.phases.push(PhaseTiming {
        name: "total".to_owned(),
        wall_micros,
    });
    builder.measure("wall_micros", wall_micros);
    builder.measure("discovered_files", cold.metrics.discovered_files);
    builder.measure("parsed_files", cold.metrics.parsed_files);
    builder.measure("syntax_error_files", cold.metrics.syntax_error_files);
    builder.measure("symbols", cold.metrics.symbols);
    builder.measure("edges", cold.metrics.edges);
    builder.measure("call_sites", cold.metrics.call_sites);
    builder.measure("ambiguous_call_sites", cold.metrics.ambiguous_call_sites);
    builder.measure("unresolved_call_sites", cold.metrics.unresolved_call_sites);
    builder.measure("rust_files", cold.metrics.rust_files);
    builder.measure("php_files", cold.metrics.php_files);
    builder.measure("typescript_files", cold.metrics.typescript_files);
    builder.measure("source_bytes", cold.metrics.indexing.coverage.source_bytes);
    builder.measure("degraded", cold.metrics.indexing.is_degraded());
    builder.measure(
        "degradations",
        u64::try_from(cold.metrics.indexing.degradations.len()).unwrap_or(u64::MAX),
    );
    match cold.metrics.indexing.memory.observed_phase_peak_rss_bytes {
        Some(rss) => {
            builder.measure("peak_rss_bytes", rss);
            builder.budget(budgets, "cold_index_peak_rss_bytes", rss, |budget| {
                budget.cold_index_peak_rss_bytes
            });
        }
        None => {
            builder.measure("peak_rss_bytes", Value::from("unavailable"));
            builder.note("peak RSS unavailable on this platform");
        }
    }
    builder.budget(budgets, "cold_index_wall_micros", wall_micros, |budget| {
        budget.cold_index_wall_micros
    });
    let checks = reject(cold.metrics.parsed_files > 0, "cold index parsed no files")
        .and_then(|()| reject(cold.metrics.symbols > 0, "cold index extracted no symbols"));
    let report = match checks {
        Ok(()) => {
            builder.note("full index from scratch; coverage counts recorded");
            builder.finish()
        }
        Err(error) => builder.fail(error.to_string()),
    };
    slots.put(report);
}

fn start_live_workspace(checkout: &Path, report: IndexReport) -> Check<LiveWorkspace> {
    let identity = chakra_git::resolve_workspace_identity(checkout)?;
    let engine = Arc::new(WorkspaceEngine::new(identity));
    engine.install_diff_provider(Arc::new(chakra_git::GitWorkspaceDiff))?;
    let mut update = engine.begin_update();
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Indexing);
    update.set_freshness(Freshness::Stale);
    engine.publish(update)?;
    let live = start_live_index(
        report.repository_root.clone(),
        report.syntax_index,
        engine.clone(),
    )?;
    let workspace = LiveWorkspace { engine, live };
    // Warmup barrier: the first fresh query settles the initial stale
    // publication, so `warm-noop` measures a true second-run no-op.
    workspace.barrier_search("chakra_warmup")?;
    Ok(workspace)
}

impl LiveWorkspace {
    fn shutdown(self) {
        let Self { live, .. } = self;
        if let Err(error) = live.shutdown() {
            eprintln!("corpus: live index shutdown failed: {error}");
        }
    }

    /// A fresh `symbol_search`; doubles as the live-index freshness barrier.
    fn barrier_search(&self, query: &str) -> Check<()> {
        let response = self.engine.symbol_search(SymbolSearchRequest {
            query: query.to_owned(),
            limit: Some(20),
            freshness: FreshnessRequirement::RequireFresh,
            ..SymbolSearchRequest::default()
        })?;
        reject(
            response.freshness == Freshness::Fresh,
            format!("barrier search `{query}` did not observe a fresh revision"),
        )?;
        Ok(())
    }

    fn find_symbol(&self, query: &str) -> Check<bool> {
        Ok(!self
            .engine
            .symbol_search(SymbolSearchRequest {
                query: query.to_owned(),
                limit: Some(20),
                freshness: FreshnessRequirement::RequireFresh,
                ..SymbolSearchRequest::default()
            })?
            .data
            .candidates
            .is_empty())
    }
}

fn collect_facts(engine: &Arc<WorkspaceEngine>, plan: &ProbePlan) -> WorkspaceFacts {
    let snapshot = engine.snapshot();
    let graph = snapshot.graph();
    let targets = high_degree_targets(graph, 3);
    let rename_symbol = graph
        .symbols()
        .iter()
        .find(|symbol| symbol.location.file().as_str() == plan.rename_file)
        .map(|symbol| (symbol.key.qualified_name.clone(), symbol.name().to_owned()));
    WorkspaceFacts {
        targets,
        rename_symbol,
    }
}

/// Top uniquely-named callees by incoming `Calls` edges, chosen from the
/// index itself (deterministic: count desc, then qualified name asc).
fn high_degree_targets(graph: &SymbolGraph, count: usize) -> Vec<(String, u64)> {
    let mut scored: Vec<(u64, String)> = graph
        .symbols()
        .iter()
        .filter(|symbol| matches!(symbol.key.kind, SymbolKind::Function | SymbolKind::Method))
        .map(|symbol| {
            let callers = graph
                .incoming_edges(symbol.id)
                .iter()
                .filter(|edge| edge.kind == EdgeKind::Calls)
                .count();
            (
                u64::try_from(callers).unwrap_or(u64::MAX),
                symbol.key.qualified_name.clone(),
            )
        })
        .filter(|(callers, _)| *callers > 0)
        .collect();
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    let mut seen = HashSet::new();
    scored
        .into_iter()
        // Unique across the whole graph: `ByName` queries reject ambiguous
        // references instead of guessing, so targets must resolve to exactly
        // one symbol.
        .filter(|(_, name)| {
            let simple = simple_name(name);
            seen.insert(simple.to_owned()) && graph.resolve_name(simple).len() == 1
        })
        .take(count)
        .map(|(callers, name)| (name, callers))
        .collect()
}

fn run_query_scenario(
    scenario: &mut ScenarioBuilder,
    workspace: &LiveWorkspace,
    facts: &WorkspaceFacts,
) -> Check<()> {
    reject(
        !facts.targets.is_empty(),
        "index contains no called function or method symbols",
    )?;
    let chosen: Vec<Value> = facts
        .targets
        .iter()
        .map(|(qualified, callers)| {
            serde_json::json!({ "qualified_name": qualified, "incoming_calls": callers })
        })
        .collect();
    scenario.measure("targets", Value::Array(chosen));
    let (qualified, callers) = &facts.targets[0];
    let simple = simple_name(qualified);
    scenario.measure("target", qualified.clone());
    scenario.measure("target_incoming_calls", *callers);

    let started = Instant::now();
    let search = workspace.engine.symbol_search(SymbolSearchRequest {
        query: simple.to_owned(),
        limit: Some(5),
        freshness: FreshnessRequirement::AllowStale,
        ..SymbolSearchRequest::default()
    })?;
    scenario.phase("symbol_search", started);
    reject(
        search.data.candidates.len() <= 5,
        "symbol_search exceeded its explicit limit",
    )?;
    let candidate = search
        .data
        .candidates
        .iter()
        .find(|candidate| candidate.qualified_name == *qualified);
    reject(
        candidate.is_some(),
        format!("high-degree target `{qualified}` missing from symbol_search"),
    )?;
    if let Some(candidate) = candidate {
        scenario.measure(
            "target_precision",
            format!("{:?}/{:?}", candidate.provenance, candidate.precision),
        );
    }

    let started = Instant::now();
    let callers_response = workspace.engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName(simple.to_owned())),
        limit: Some(5),
        freshness: FreshnessRequirement::AllowStale,
    })?;
    scenario.phase("callers", started);
    reject(
        callers_response.data.callers.len() <= 5,
        "callers exceeded its explicit limit",
    )?;
    reject(
        !callers_response.data.callers.is_empty(),
        format!("high-degree target `{qualified}` reported no callers"),
    )?;
    scenario.measure(
        "callers_returned",
        u64::try_from(callers_response.data.callers.len()).unwrap_or(u64::MAX),
    );
    if callers_response.truncated {
        reject(
            callers_response
                .truncation
                .iter()
                .any(|detail| detail.section == TruncationSection::CallersCallers),
            format!(
                "truncated callers lacks section detail: {:?}",
                callers_response.truncation
            ),
        )?;
    }
    // Force truncation: a multi-caller target at limit 1 must truncate with
    // an explicit section/cause detail.
    if *callers > 1 {
        let paged = workspace.engine.callers(CallersRequest {
            symbol: Some(SymbolRef::ByName(simple.to_owned())),
            limit: Some(1),
            freshness: FreshnessRequirement::AllowStale,
        })?;
        reject(
            paged.truncated
                && paged
                    .truncation
                    .iter()
                    .any(|detail| detail.section == TruncationSection::CallersCallers),
            format!(
                "limit-1 callers of `{qualified}` did not truncate explicitly: {:?}",
                paged.truncation
            ),
        )?;
    }

    let started = Instant::now();
    let context = workspace.engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName(simple.to_owned())),
        limit: Some(5),
        freshness: FreshnessRequirement::AllowStale,
    })?;
    scenario.phase("context", started);
    reject(
        context.data.symbol.qualified_name == *qualified,
        "context resolved a different symbol than the high-degree target",
    )?;
    reject(
        context.data.callers.len() <= 5 && context.data.callees.len() <= 5,
        "context exceeded its explicit limit",
    )?;
    scenario.note(
        "high-degree targets chosen from the index; bounded responses with explicit truncation",
    );
    Ok(())
}

/// Runs the edit scenarios against the cached checkout and always restores
/// it afterwards (`cache-restore` proves the restore).
fn run_mutation_scenarios(
    slots: &mut Slots,
    checkout: &Path,
    workspace: &LiveWorkspace,
    plan: &ProbePlan,
    facts: &WorkspaceFacts,
    repo: &CorpusRepository,
) {
    // diff-context, clean part: captured before any edit.
    let clean_started = Instant::now();
    let clean = workspace.engine.diff_context(DiffContextRequest::default());
    let clean_wall = clean_started.elapsed();

    // When the workspace source-byte budget is degraded, the indexed set is a
    // greedy fill at the budget boundary: appending bytes shifts which small
    // boundary files fit, so a one-file edit can legitimately reparse the
    // edited file plus a few (de)materialized boundary files. The strict
    // "exactly N reparsed" assertion only holds for non-degraded indexes.
    let source_budget_degraded = workspace
        .engine
        .snapshot()
        .indexing()
        .degradations
        .iter()
        .any(|degradation| degradation.cause == IndexBudgetKind::WorkspaceSourceBytes);

    slots.run("one-file-edit", |scenario| {
        let original = fs::read_to_string(checkout.join(&plan.edit_file))?;
        fs::write(
            checkout.join(&plan.edit_file),
            format!("{original}{}", plan.declaration_one),
        )?;
        let before = workspace.live.metrics();
        let started = Instant::now();
        reject(
            workspace.find_symbol("chakra_corpus_probe_one")?,
            "appended probe function is not queryable (read-your-writes broken)",
        )?;
        scenario.phase("edit_and_barrier", started);
        scenario.measure("wall_micros", micros(started.elapsed()));
        record_incremental(
            scenario,
            &before,
            &workspace.live.metrics(),
            1,
            source_budget_degraded,
        )?;
        scenario.note("one-file edit: targeted reconcile, fresh query sees the change");
        Ok(())
    });

    slots.run("atomic-replace", |scenario| {
        let current = fs::read_to_string(checkout.join(&plan.edit_file))?;
        fs::write(
            checkout.join(&plan.swap_file),
            format!("{current}{}", plan.declaration_two),
        )?;
        fs::rename(
            checkout.join(&plan.swap_file),
            checkout.join(&plan.edit_file),
        )?;
        let before = workspace.live.metrics();
        let started = Instant::now();
        reject(
            workspace.find_symbol("chakra_corpus_probe_two")?,
            "atomically replaced content is not queryable",
        )?;
        reject(
            workspace.find_symbol("chakra_corpus_probe_one")?,
            "atomically replaced file lost the earlier probe",
        )?;
        scenario.phase("replace_and_barrier", started);
        scenario.measure("wall_micros", micros(started.elapsed()));
        record_incremental(
            scenario,
            &before,
            &workspace.live.metrics(),
            1,
            source_budget_degraded,
        )?;
        scenario.note("write-temp-then-rename: targeted reconcile, fresh query sees the change");
        Ok(())
    });

    slots.run("rename-delete", |scenario| {
        let rename_symbol = facts.rename_symbol.clone();
        fs::rename(
            checkout.join(&plan.rename_file),
            checkout.join(&plan.renamed_file),
        )?;
        fs::remove_file(checkout.join(&plan.edit_file))?;
        let before = workspace.live.metrics();
        let started = Instant::now();
        reject(
            !workspace.find_symbol("chakra_corpus_probe_two")?,
            "deleted file content survived the delete",
        )?;
        scenario.phase("rename_delete_and_barrier", started);
        scenario.measure("wall_micros", micros(started.elapsed()));
        let after = workspace.live.metrics();
        reject(
            after.full_reconciliations == before.full_reconciliations,
            "rename/delete triggered a full reconciliation",
        )?;
        scenario.measure(
            "created_files",
            after.created_files.saturating_sub(before.created_files),
        );
        scenario.measure(
            "deleted_files",
            after.deleted_files.saturating_sub(before.deleted_files),
        );
        if let Some((_qualified, simple)) = rename_symbol {
            // The qualified name may change with the file name (module
            // path), and a substring search could truncate the match away on
            // large repos — inspect the reconciled graph directly (the
            // barrier above already published the rename).
            let snapshot = workspace.engine.snapshot();
            reject(
                snapshot.graph().symbols().iter().any(|symbol| {
                    symbol.name() == simple && symbol.location.file().as_str() == plan.renamed_file
                }),
                format!("symbol `{simple}` lost after renaming its file"),
            )?;
        }
        scenario.note("rename + delete: inventory correct after the freshness barrier");
        Ok(())
    });

    // diff-context, dirty part: the tree still carries every edit above.
    slots.run("diff-context", |scenario| {
        let clean = clean
            .as_ref()
            .map_err(|error| crate::failure(format!("clean diff_context failed: {error}")))?;
        scenario.phase("clean_diff", clean_started);
        scenario.measure("clean_diff_wall_micros", micros(clean_wall));
        reject(
            clean.data.changed_files.is_empty(),
            format!(
                "clean checkout reported changed files: {:?}",
                clean
                    .data
                    .changed_files
                    .iter()
                    .map(|file| file.path.as_str())
                    .collect::<Vec<_>>()
            ),
        )?;
        let started = Instant::now();
        let dirty = workspace
            .engine
            .diff_context(DiffContextRequest::default())?;
        scenario.phase("dirty_diff", started);
        let changes: Vec<Value> = dirty
            .data
            .changed_files
            .iter()
            .map(|file| {
                serde_json::json!({
                    "path": file.path.as_str(),
                    "change": format!("{:?}", file.change),
                    "previous_path": file.previous_path.as_ref().map(|path| path.as_str()),
                })
            })
            .collect();
        scenario.measure("changed_files", Value::Array(changes));
        reject(
            dirty.data.changed_files.iter().any(|file| {
                file.path.as_str() == plan.edit_file && file.change == ChangeKind::Deleted
            }),
            "edited-then-deleted file missing from diff_context",
        )?;
        reject(
            dirty.data.changed_files.iter().any(|file| {
                file.path.as_str() == plan.renamed_file
                    && matches!(file.change, ChangeKind::Added | ChangeKind::Renamed)
            }),
            "renamed file missing from diff_context",
        )?;
        scenario.note("clean tree: empty diff; after edits: correct changed-file attribution");
        Ok(())
    });

    slots.run("cache-restore", |scenario| {
        // Remove runner-created untracked files explicitly; never `git clean`
        // (the fetch tool's `.chakra-corpus.json` must survive).
        for created in plan.created_paths() {
            let path = checkout.join(created);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        git(checkout, &["checkout", "--", "."])?;
        workspace.barrier_search("chakra_corpus_probe")?;
        reject(
            !workspace.find_symbol("chakra_corpus_probe_one")?,
            "probe survived the cache restore",
        )?;
        verify_clean_cache(checkout, repo, scenario)
    });
}

/// Asserts a targeted, incremental reconcile happened between the two live
/// metric snapshots (never a full reindex). `source_budget_degraded` relaxes
/// the exact reparse count: with the workspace source-byte budget degraded,
/// the greedy fill at the budget boundary (de)materializes small boundary
/// files when an edit changes total source bytes, so the edited file's
/// reparse may be joined by boundary files. Read-your-writes still requires
/// at least the edited file to be reparsed, and a full reconciliation stays
/// forbidden either way.
fn record_incremental(
    scenario: &mut ScenarioBuilder,
    before: &LiveIndexMetrics,
    after: &LiveIndexMetrics,
    expected_reparsed: u64,
    source_budget_degraded: bool,
) -> Check<()> {
    let reparsed = after.files_reparsed.saturating_sub(before.files_reparsed);
    scenario.measure("files_reparsed", reparsed);
    scenario.measure(
        "full_reconciliations",
        after
            .full_reconciliations
            .saturating_sub(before.full_reconciliations),
    );
    if source_budget_degraded {
        reject(
            reparsed >= expected_reparsed,
            format!("expected at least {expected_reparsed} reparsed file(s), observed {reparsed}"),
        )?;
        if reparsed > expected_reparsed {
            scenario.note(format!(
                "source-byte budget degraded: {} extra reparsed file(s) are budget-boundary churn",
                reparsed - expected_reparsed
            ));
        }
    } else {
        reject(
            reparsed == expected_reparsed,
            format!("expected {expected_reparsed} reparsed file(s), observed {reparsed}"),
        )?;
    }
    reject(
        after.full_reconciliations == before.full_reconciliations,
        "edit triggered a full reconciliation instead of incremental work",
    )?;
    Ok(())
}

fn run_syntax_error_scenario(
    scenario: &mut ScenarioBuilder,
    checkout: &Path,
    workspace: &LiveWorkspace,
    plan: &ProbePlan,
    facts: &WorkspaceFacts,
) -> Check<()> {
    let original = fs::read_to_string(checkout.join(&plan.syntax_file))?;
    // Real corpora carry pre-existing diagnostics (fixtures with deliberately
    // broken sources), so every assertion is relative to the baseline.
    workspace.barrier_search("chakra")?;
    let baseline = workspace.engine.status(StatusRequest)?;
    let baseline_total = baseline.data.syntax_diagnostics.total_diagnostics;
    let baseline_attributed = baseline
        .data
        .syntax_diagnostics
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.range.file().as_str() == plan.syntax_file)
        .count();
    scenario.measure("baseline_total_diagnostics", baseline_total);
    fs::write(
        checkout.join(&plan.syntax_file),
        format!("{original}{}", plan.broken),
    )?;
    let started = Instant::now();
    if let Some(retained) = facts.retained_query() {
        reject(
            workspace.find_symbol(&retained)?,
            format!("intact symbol `{retained}` lost while another file is broken"),
        )?;
        scenario.note(format!("last-good revision: `{retained}` stays queryable"));
    } else {
        workspace.barrier_search("chakra")?;
    }
    scenario.phase("break_and_barrier", started);
    let broken = workspace.engine.status(StatusRequest)?;
    let broken_attributed = broken
        .data
        .syntax_diagnostics
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.range.file().as_str() == plan.syntax_file)
        .count();
    reject(
        broken_attributed > baseline_attributed,
        format!(
            "breaking `{}` added no diagnostic attributed to it (baseline {baseline_attributed}, broken {broken_attributed})",
            plan.syntax_file
        ),
    )?;
    // Restore by writing the exact original bytes back, then confirm the
    // diagnostics return to baseline on a newer published revision.
    fs::write(checkout.join(&plan.syntax_file), &original)?;
    workspace.barrier_search("chakra")?;
    let healed = workspace.engine.status(StatusRequest)?;
    let healed_attributed = healed
        .data
        .syntax_diagnostics
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.range.file().as_str() == plan.syntax_file)
        .count();
    reject(
        healed.data.syntax_diagnostics.total_diagnostics == baseline_total
            && healed_attributed == baseline_attributed,
        format!(
            "diagnostics did not return to baseline after restoring the file (baseline {baseline_total}, healed {})",
            healed.data.syntax_diagnostics.total_diagnostics
        ),
    )?;
    reject(
        healed.revision > broken.revision,
        "recovery did not publish a newer revision",
    )?;
    scenario.note("temporary syntax error: attributed diagnostics; restore returns to baseline on a newer revision");
    Ok(())
}

/// Proves the cached checkout is back at the pinned SHA with a clean
/// worktree (the fetch tool's untracked metadata file excepted).
fn verify_clean_cache(
    checkout: &Path,
    repo: &CorpusRepository,
    scenario: &mut ScenarioBuilder,
) -> Check<()> {
    let head = git(checkout, &["rev-parse", "HEAD"])?;
    reject(
        head == repo.sha,
        format!(
            "checkout HEAD {head} no longer matches pinned SHA {}",
            repo.sha
        ),
    )?;
    let status = git(checkout, &["status", "--porcelain"])?;
    let dirty: Vec<&str> = status
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|line| line.trim_start_matches("?? ").trim() != CACHE_METADATA_FILE)
        .collect();
    reject(
        dirty.is_empty(),
        format!("checkout worktree is dirty after restore: {dirty:?}"),
    )?;
    scenario.note("checkout restored to the pinned SHA with a clean worktree");
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Check<String> {
    let output = Command::new("git").current_dir(root).args(args).output()?;
    if !output.status.success() {
        return Err(failure(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn simple_name(qualified_name: &str) -> &str {
    qualified_name.rsplit("::").next().unwrap_or(qualified_name)
}

/// Deterministic digest over file summaries, symbols, edges, and call sites
/// (same approach as the large-repository gate).
fn graph_fingerprint(graph: &SymbolGraph) -> String {
    use std::hash::Hasher;
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    /// Creates `tiny/tiny` (four small Rust files with one high-degree
    /// callee) as a real Git repository inside a temporary cache root.
    fn tiny_cache() -> Check<(TempDir, PathBuf, CorpusRepository)> {
        let cache = TempDir::new()?;
        let checkout = cache.path().join("tiny__tiny");
        fs::create_dir_all(checkout.join("src"))?;
        fs::write(
            checkout.join("src/lib.rs"),
            "pub fn hot() {}\npub fn alpha() { hot(); }\n",
        )?;
        fs::write(
            checkout.join("src/more.rs"),
            "pub fn beta() { hot(); }\npub fn gamma() { hot(); }\n",
        )?;
        fs::write(checkout.join("src/extra.rs"), "pub fn delta() { hot(); }\n")?;
        fs::write(checkout.join("src/spare.rs"), "pub fn epsilon() {}\n")?;
        git(&checkout, &["init", "--quiet"])?;
        git(&checkout, &["add", "-A"])?;
        git(
            &checkout,
            &[
                "-c",
                "user.email=corpus@example.invalid",
                "-c",
                "user.name=Chakra Corpus",
                "commit",
                "--quiet",
                "-m",
                "tiny corpus",
            ],
        )?;
        let sha = git(&checkout, &["rev-parse", "HEAD"])?;
        let repo = CorpusRepository {
            name: "tiny/tiny".to_owned(),
            url: "https://example.invalid/tiny/tiny".to_owned(),
            branch: "main".to_owned(),
            sha,
            license: "MIT".to_owned(),
            size_kb: 1,
            rationale: "test double".to_owned(),
        };
        Ok((cache, checkout, repo))
    }

    fn generous_budgets() -> LanguageBudgets {
        LanguageBudgets {
            cold_index_wall_micros: 600_000_000,
            cold_index_peak_rss_bytes: u64::MAX,
            warm_noop_wall_micros: 600_000_000,
        }
    }

    #[test]
    fn evaluates_a_tiny_rust_repository() -> Check<()> {
        let (cache, checkout, repo) = tiny_cache()?;
        let report = evaluate_repository("rust", &repo, Some(generous_budgets()), cache.path());
        assert_eq!(report.status, RepoStatus::Evaluated);
        assert_eq!(report.scenario_count, SCENARIO_IDS.len());
        for scenario in &report.scenarios {
            assert_eq!(
                scenario.status,
                CorpusScenarioStatus::Pass,
                "{} failed: {}",
                scenario.id,
                scenario.details
            );
        }
        let cold = report
            .scenario("cold-index")
            .ok_or("cold-index scenario missing")?;
        let symbols = cold.measurements["symbols"]
            .as_u64()
            .ok_or("symbols measurement is not a number")?;
        assert!(symbols >= 6, "expected at least 6 symbols, found {symbols}");
        // The cache is still valid after the run.
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"])?, repo.sha);
        assert_eq!(git(&checkout, &["status", "--porcelain"])?, "");
        Ok(())
    }

    #[test]
    fn missing_checkout_is_a_graceful_skip() -> Check<()> {
        let cache = TempDir::new()?;
        let (_, _, repo) = tiny_cache()?;
        let report = evaluate_repository("rust", &repo, None, cache.path());
        assert_eq!(report.status, RepoStatus::Skipped);
        assert!(report.skip_reason.contains("not cached"));
        assert!(
            report
                .scenarios
                .iter()
                .all(|scenario| scenario.status == CorpusScenarioStatus::Skipped)
        );
        Ok(())
    }

    #[test]
    fn sha_mismatch_is_refused() -> Check<()> {
        let (cache, _checkout, mut repo) = tiny_cache()?;
        repo.sha = "0".repeat(40);
        let report = evaluate_repository("rust", &repo, None, cache.path());
        assert_eq!(report.status, RepoStatus::Skipped);
        assert!(report.skip_reason.contains("does not match pinned SHA"));
        Ok(())
    }

    #[test]
    fn unsupported_language_skips_every_repository() -> Check<()> {
        let manifest = CorpusManifest::load(&super::super::default_manifest_path())?;
        let budgets = CorpusBudgets {
            schema_version: 1,
            note: String::new(),
            languages: BTreeMap::new(),
        };
        let cache = TempDir::new()?;
        let reports = evaluate_language("go", &manifest, &budgets, cache.path())?;
        assert!(!reports.is_empty());
        assert!(
            reports
                .iter()
                .all(|report| report.status == RepoStatus::Skipped
                    && report.skip_reason.contains("unsupported language"))
        );
        Ok(())
    }

    #[test]
    fn high_degree_targets_prefers_the_most_called_unique_symbol() -> Check<()> {
        let (_cache, checkout, _repo) = tiny_cache()?;
        let cold = index_repository(&checkout)?;
        let targets = high_degree_targets(&cold.graph, 3);
        let first = targets.first().ok_or("no high-degree targets")?;
        assert_eq!(first.0, "hot");
        assert_eq!(first.1, 4);
        Ok(())
    }

    #[test]
    fn renamed_sibling_keeps_the_extension() -> Check<()> {
        assert_eq!(renamed_sibling("src/lib.rs")?, "src/lib__chakra_moved.rs");
        assert_eq!(renamed_sibling("Service.php")?, "Service__chakra_moved.php");
        Ok(())
    }
}
