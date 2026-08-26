//! Deterministic C++ syntax indexing on the shared language-neutral driver
//! (issue #94). This module keeps only the C++ seams: the Tree-sitter parser
//! hook, Git-aware discovery, worker naming, and the post-parse evidence
//! passes for qualified callables and promoted calls (issues #83/#84).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::symbol::{CallForm, CallTargetKind, SymbolKind};
use chakra_git::ClassifiedSource;
use chakra_language_index::facts::ParsedFile;
use chakra_language_index::{LanguageHooks, LanguageParser};

use crate::discovery::discover_cpp_sources;
use crate::parser::CppParser;

pub use chakra_language_index::{
    IndexMetrics, LanguageBuildMetrics, ReconcileMetrics, SyntaxFactCounts,
};

/// Failure to discover, read, parse, or validate the C++ syntax index.
pub type CppIndexError = chakra_language_index::LanguageIndexError;
/// Latest C++ source text plus role/package metadata from the same scan.
pub type CppSources = chakra_language_index::LanguageSources;
/// Reusable per-file C++ syntax facts and per-owner relationship
/// contributions.
pub type CppSyntaxIndex = chakra_language_index::LanguageSyntaxIndex<CppHooks>;
/// Complete private initial C++ index, ready for atomic publication.
pub type IndexReport = chakra_language_index::IndexReport<CppHooks>;
/// Reconcile outcome for the C++ syntax index.
pub type ReconcileReport = chakra_language_index::ReconcileReport<CppHooks>;

/// C++ seams of the shared indexing driver.
#[derive(Debug, Clone, Copy, Default)]
pub struct CppHooks;

impl LanguageHooks for CppHooks {
    type Parser = CppParser;

    const WORKER_NAME: &'static str = "cpp";

    fn new_parser() -> Result<Self::Parser, CppIndexError> {
        CppParser::new().map_err(|error| CppIndexError::Parse(error.to_string()))
    }

    fn discover_sources(root: &Path) -> Result<Vec<ClassifiedSource>, CppIndexError> {
        Ok(discover_cpp_sources(root)?)
    }

    fn post_parse(files: &mut BTreeMap<RepoRelativePath, Arc<ParsedFile>>) {
        reclassify_qualified_callables(files);
        resolve_unqualified_method_calls(files);
    }
}

impl LanguageParser for CppParser {
    fn parse(
        &mut self,
        path: RepoRelativePath,
        source: Arc<str>,
    ) -> Result<chakra_language_index::ParsedFile, CppIndexError> {
        CppParser::parse(self, path, source)
            .map_err(|error| CppIndexError::Parse(error.to_string()))
    }
}

/// Builds a complete C++ syntax index from the actual materialized Git
/// worktree. The caller owns atomic publication into `WorkspaceEngine`.
pub fn index_repository(root: &Path) -> Result<IndexReport, CppIndexError> {
    chakra_language_index::index_repository::<CppHooks>(root)
}

/// Reads the latest Git-aware C++ file inventory and exact contents.
pub fn scan_repository_sources(repository_root: &Path) -> Result<CppSources, CppIndexError> {
    chakra_language_index::scan_repository_sources::<CppHooks>(repository_root)
}

fn symbol_name(symbol: &chakra_language_index::SymbolDraft) -> &str {
    symbol
        .key
        .qualified_name
        .rsplit("::")
        .next()
        .unwrap_or(&symbol.key.qualified_name)
}

/// Reclassifies qualified callable definitions using workspace symbol
/// evidence (issue #84). The parser cannot distinguish `void ns::free()`
/// (namespace-qualified free function) from `void ns::Type::method()`
/// (out-of-line method): Tree-sitter C++ spells both qualifier scopes as
/// `namespace_identifier`. Once every file is parsed, the owner qualifier of
/// each `Method` draft is compared against known type and namespace names: a
/// qualifier that names a type keeps `Method`, one that only names a
/// namespace becomes `Function`, and an unproven qualifier keeps the
/// conservative parse-time kind. Runs over retained drafts in memory; no file
/// is reparsed and no eager workspace rescan is introduced.
fn reclassify_qualified_callables(files: &mut BTreeMap<RepoRelativePath, Arc<ParsedFile>>) {
    let mut namespaces: HashSet<String> = HashSet::new();
    let mut types: HashSet<String> = HashSet::new();
    for file in files.values() {
        for symbol in &file.symbols {
            match symbol.key.kind {
                SymbolKind::Module => {
                    namespaces.insert(symbol.key.qualified_name.clone());
                }
                SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Enum
                | SymbolKind::TypeAlias => {
                    types.insert(symbol.key.qualified_name.clone());
                }
                _ => {}
            }
        }
    }
    for file in files.values_mut() {
        let reclassifiable = file.symbols.iter().any(|symbol| {
            matches!(symbol.key.kind, SymbolKind::Function | SymbolKind::Method)
                && qualified_callable_kind(&symbol.key.qualified_name, &namespaces, &types)
                    .is_some_and(|kind| kind != symbol.key.kind)
        });
        if !reclassifiable {
            continue;
        }
        for symbol in &mut Arc::make_mut(file).symbols {
            if matches!(symbol.key.kind, SymbolKind::Function | SymbolKind::Method)
                && let Some(kind) =
                    qualified_callable_kind(&symbol.key.qualified_name, &namespaces, &types)
            {
                symbol.key.kind = kind;
            }
        }
    }
}

/// The parser's conservative kind for an explicitly qualified definition is
/// `Method`. A namespace owner upgrades it to `Function`; a type or an owner
/// no longer proven by workspace evidence restores `Method`. Applying this to
/// both retained variants makes the post-parse pass reversible across edits.
fn qualified_callable_kind(
    qualified_name: &str,
    namespaces: &HashSet<String>,
    types: &HashSet<String>,
) -> Option<SymbolKind> {
    let (owner, _) = qualified_name.rsplit_once("::")?;
    Some(if namespaces.contains(owner) && !types.contains(owner) {
        SymbolKind::Function
    } else {
        SymbolKind::Method
    })
}

/// Resolves unqualified function-form calls that the parser promoted to the
/// method tier because their caller looked like a method (issue #83). A bare
/// `helper()` inside a C++ method can denote an implicit member, a namespace
/// function found by ordinary lookup, or an ADL candidate; Tree-sitter cannot
/// tell them apart. With workspace symbol evidence the index now:
///
/// - keeps the call at the method tier when a same-type member exists and no
///   free function collides;
/// - retargets it to the function domain when no same-type member exists but
///   a free function does — a unique free function then resolves, and several
///   report ambiguity through the usual lazy candidate contract;
/// - represents a genuine member/free collision as one bounded mixed-domain
///   ambiguity, with the same-type method and unqualified free functions all
///   enumerable (clangd remains the precise path);
/// - returns calls inside callables reclassified as free functions (issue
///   #84) to the function domain.
///
/// The pass runs over retained in-memory drafts; no file is reparsed and no
/// domain/MCP contract changes.
fn resolve_unqualified_method_calls(files: &mut BTreeMap<RepoRelativePath, Arc<ParsedFile>>) {
    let mut methods: HashSet<String> = HashSet::new();
    let mut free_functions: HashMap<String, usize> = HashMap::new();
    for file in files.values() {
        for symbol in &file.symbols {
            match symbol.key.kind {
                SymbolKind::Method => {
                    methods.insert(symbol.key.qualified_name.clone());
                }
                SymbolKind::Function => {
                    *free_functions
                        .entry(symbol_name(symbol).to_owned())
                        .or_default() += 1;
                }
                _ => {}
            }
        }
    }
    for file in files.values_mut() {
        let adjustable = file.calls.iter().any(|call| call.promoted);
        if !adjustable {
            continue;
        }
        let file = Arc::make_mut(file);
        let symbols = &file.symbols;
        let calls = &mut file.calls;
        for call in calls.iter_mut() {
            if !call.promoted {
                continue;
            }
            // Restore the parser's original function form so the pass is a
            // pure, reversible function of the current workspace evidence.
            call.form = CallForm::Function;
            call.target_kind = CallTargetKind::Method;
            call.qualifier = None;
            let Some(caller) = symbols.get(call.caller) else {
                continue;
            };
            if caller.key.kind != SymbolKind::Method {
                // The caller was reclassified as a namespace-qualified free
                // function after parsing; its calls were never member calls.
                call.target_kind = CallTargetKind::Function;
                continue;
            }
            let owner = caller
                .key
                .qualified_name
                .rsplit_once("::")
                .map(|(owner, _)| owner.to_owned());
            let member_exists = owner
                .as_deref()
                .is_some_and(|owner| methods.contains(&format!("{owner}::{}", call.name)));
            let free_candidates = free_functions.get(call.name.as_str()).copied().unwrap_or(0);
            match (member_exists, free_candidates) {
                (true, 0) => call.qualifier = owner,
                // Preserve the existing bounded method-domain ambiguity when
                // workspace evidence cannot prove a free function or a
                // same-type member; inheritance may still supply the target.
                (false, 0) => {}
                (false, _) => call.target_kind = CallTargetKind::Function,
                // The qualifier narrows only the method side of this explicit
                // mixed-domain lookup; free functions remain unqualified.
                (true, _) => {
                    call.target_kind = CallTargetKind::FunctionOrMethod;
                    call.qualifier = owner;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::fs::{self, File};
    use std::process::Command;
    use std::sync::Barrier;
    use std::time::Instant;

    use chakra_domain::indexing::{IndexCancellation, IndexPhase};
    use chakra_domain::provenance::{Precision, Provenance};
    use chakra_domain::source::SourceMetadata;
    use chakra_domain::symbol::{CallResolution, EdgeKind, SymbolKind};
    use chakra_engine::{GraphBuildLimits, SymbolGraph};
    use chakra_language_index::{MAX_SOURCE_FILE_BYTES, parse_sources_scheduled_observed};
    use tempfile::TempDir;

    use super::*;

    fn graph_snapshot(graph: &SymbolGraph) -> Vec<String> {
        let mut snapshot = vec![format!("files:{:?}", graph.file_summaries())];
        for symbol in graph.symbols() {
            snapshot.push(format!("symbol:{symbol:?}"));
            snapshot.push(format!("outgoing:{:?}", graph.outgoing_edges(symbol.id)));
            snapshot.push(format!(
                "calls:{:?}",
                graph.call_sites_from(symbol.id).collect::<Vec<_>>()
            ));
        }
        snapshot
    }

    #[test]
    fn bounded_parallel_parsing_is_deterministic() -> Result<(), Box<dyn Error>> {
        let mut sources = BTreeMap::new();
        for index in 0..64 {
            sources.insert(
                RepoRelativePath::new(format!("src/generated_{index:03}.cpp"))?,
                Arc::<str>::from(format!(
                "class Generated{index:03} {{ void caller_{index}() {{ helper_{index}(); }} void helper_{index}() {{}} }};\n"
                )),
            );
        }
        let cancellation = IndexCancellation::default();
        let (_, sequential, sequential_metrics) = CppSyntaxIndex::from_sources_scheduled(
            sources.clone(),
            GraphBuildLimits::UNLIMITED,
            1,
            1,
            &cancellation,
        )?;
        let (_, parallel, parallel_metrics) = CppSyntaxIndex::from_sources_scheduled(
            sources,
            GraphBuildLimits::UNLIMITED,
            4,
            1,
            &cancellation,
        )?;

        assert_eq!(graph_snapshot(&sequential), graph_snapshot(&parallel));
        assert_eq!(sequential_metrics.facts, parallel_metrics.facts);
        assert_eq!(sequential_metrics.graph, parallel_metrics.graph);
        let parallel_parse = parallel_metrics
            .phases
            .iter()
            .find(|phase| phase.phase == IndexPhase::ParseExtraction)
            .ok_or("parallel parse phase missing")?;
        assert_eq!(parallel_parse.effective_workers, 4);
        assert!(
            (1..=parallel_parse.effective_workers).contains(&parallel_parse.peak_active_workers)
        );
        assert_eq!(parallel_parse.peak_queue_depth, 0);
        assert_eq!(parallel_parse.work_items, 64);
        Ok(())
    }

    #[test]
    fn cancelling_parallel_parse_joins_every_scoped_worker() -> Result<(), Box<dyn Error>> {
        const WORKERS: usize = 4;
        let mut sources = BTreeMap::new();
        for index in 0..64 {
            sources.insert(
                RepoRelativePath::new(format!("src/cancel_{index:03}.cpp"))?,
                Arc::<str>::from(format!(
                    "class Cancel{index:03} {{ void item_{index}() {{}} }};\n"
                )),
            );
        }
        let cancellation = IndexCancellation::default();
        let worker_cancellation = cancellation.clone();
        let started = Arc::new(Barrier::new(WORKERS + 1));
        let release = Arc::new(Barrier::new(WORKERS + 1));
        let worker_started = started.clone();
        let worker_release = release.clone();
        let indexing = std::thread::spawn(move || {
            let observer = || {
                worker_started.wait();
                worker_release.wait();
            };
            parse_sources_scheduled_observed::<CppHooks>(
                sources,
                WORKERS,
                1,
                &worker_cancellation,
                Some(&observer),
            )
        });

        started.wait();
        cancellation.cancel();
        release.wait();
        let result = indexing.join().map_err(|_| "index owner panicked")?;
        assert!(matches!(result, Err(CppIndexError::Cancelled)));
        Ok(())
    }

    #[test]
    fn qualified_free_functions_are_distinguished_from_out_of_line_methods()
    -> Result<(), Box<dyn Error>> {
        let mut sources = BTreeMap::new();
        sources.insert(
            RepoRelativePath::new("include/api.h")?,
            Arc::<str>::from(
                "namespace ns {\n\
                 struct Type { void method(); };\n\
                 void free();\n\
                 namespace inner { void deep(); }\n\
                 }\n",
            ),
        );
        sources.insert(
            RepoRelativePath::new("src/api.cpp")?,
            Arc::<str>::from(
                "namespace other { void untouched(); }\n\
                 void ns::free() {}\n\
                 void ns::Type::method() {}\n\
                 void ns::inner::deep() {}\n\
                 void unproven::ambiguous() {}\n",
            ),
        );
        let (_, graph) = CppSyntaxIndex::from_sources(sources)?;
        let kind_of = |qualified_name: &str| {
            graph
                .symbols()
                .iter()
                .find(|symbol| {
                    symbol.key.qualified_name == qualified_name
                        && symbol.key.path.as_str() == "src/api.cpp"
                })
                .map(|symbol| symbol.key.kind)
        };
        assert_eq!(kind_of("ns::free"), Some(SymbolKind::Function));
        assert_eq!(kind_of("ns::Type::method"), Some(SymbolKind::Method));
        assert_eq!(kind_of("ns::inner::deep"), Some(SymbolKind::Function));
        // No workspace evidence names `unproven`: the qualifier cannot be
        // proven to be a type or a namespace, so the conservative parse-time
        // classification is preserved.
        assert_eq!(kind_of("unproven::ambiguous"), Some(SymbolKind::Method));
        Ok(())
    }

    #[test]
    fn reconcile_reclassifies_qualified_callables_without_reparse() -> Result<(), Box<dyn Error>> {
        let mut sources = BTreeMap::new();
        sources.insert(
            RepoRelativePath::new("src/lib.cpp")?,
            Arc::<str>::from("void ns::free() {}\n"),
        );
        sources.insert(
            RepoRelativePath::new("include/evidence.h")?,
            Arc::<str>::from("namespace other { void untouched(); }\n"),
        );
        let (index, graph) = CppSyntaxIndex::from_sources(sources.clone())?;
        let kind_of = |graph: &SymbolGraph, path: &str| {
            graph
                .symbols()
                .iter()
                .find(|symbol| {
                    symbol.key.qualified_name == "ns::free" && symbol.key.path.as_str() == path
                })
                .map(|symbol| symbol.key.kind)
        };
        // Without namespace evidence the conservative kind is kept.
        assert_eq!(kind_of(&graph, "src/lib.cpp"), Some(SymbolKind::Method));

        // Modifying an already-discovered evidence file keeps the structural
        // delta eligible. The retained definition must still be republished
        // after the workspace evidence pass changes its facts.
        sources.insert(
            RepoRelativePath::new("include/evidence.h")?,
            Arc::<str>::from("namespace ns { void free(); }\n"),
        );
        let report = index.reconcile_sources(sources.clone())?;
        let next = report.next_index.ok_or("reconcile must publish a graph")?;
        assert_eq!(
            kind_of(next.graph(), "src/lib.cpp"),
            Some(SymbolKind::Function)
        );
        assert_eq!(report.metrics.reparsed_files, 1);
        assert!(report.metrics.publication.structurally_incremental);
        assert_eq!(report.metrics.publication.rebuilt_files, 2);
        next.graph().validate_consistency()?;

        // Removing the same evidence must reverse the retained definition's
        // classification without reparsing its source file (issue #117).
        sources.insert(
            RepoRelativePath::new("include/evidence.h")?,
            Arc::<str>::from("namespace other { void untouched(); }\n"),
        );
        let report = next.reconcile_sources(sources)?;
        let reverted = report.next_index.ok_or("reconcile must publish a graph")?;
        assert_eq!(
            kind_of(reverted.graph(), "src/lib.cpp"),
            Some(SymbolKind::Method)
        );
        assert_eq!(report.metrics.reparsed_files, 1);
        assert!(report.metrics.publication.structurally_incremental);
        assert_eq!(report.metrics.publication.rebuilt_files, 2);
        reverted.graph().validate_consistency()?;
        Ok(())
    }

    #[test]
    fn unqualified_calls_from_methods_keep_honest_candidates() -> Result<(), Box<dyn Error>> {
        let mut sources = BTreeMap::new();
        sources.insert(
            RepoRelativePath::new("include/api.h")?,
            Arc::<str>::from(
                "namespace ns {\n\
                 struct Type { void member_only(); void colliding(); void helper(); void run_collision(); };\n\
                 }\n\
                 void unique_free();\n\
                 void shared_name();\n\
                 namespace other { void shared_name(); }\n\
                 void colliding();\n",
            ),
        );
        sources.insert(
            RepoRelativePath::new("src/impl.cpp")?,
            Arc::<str>::from(
                "void ns::Type::member_only() { unique_free(); }\n\
                 void ns::Type::run_collision() { colliding(); }\n\
                 void ns::Type::helper() { shared_name(); }\n",
            ),
        );
        let (_, graph) = CppSyntaxIndex::from_sources(sources)?;
        graph.validate_consistency()?;

        fn single_call(
            graph: &SymbolGraph,
            caller_name: &str,
            path: &str,
        ) -> Result<chakra_domain::symbol::CallSite, Box<dyn Error>> {
            let caller = graph
                .symbols()
                .iter()
                .find(|symbol| {
                    symbol.key.qualified_name == caller_name && symbol.key.path.as_str() == path
                })
                .ok_or_else(|| format!("missing caller {caller_name}"))?;
            let calls: Vec<_> = graph.call_sites_from(caller.id).collect();
            if calls.len() != 1 {
                return Err(format!("{caller_name} must own exactly one call").into());
            }
            Ok(calls[0].clone())
        }

        // Unique free function, no same-type member: resolves (issue #83).
        let resolved = single_call(&graph, "ns::Type::member_only", "src/impl.cpp")?;
        assert_eq!(resolved.target_kind, CallTargetKind::Function);
        let CallResolution::Resolved { target } = resolved.resolution else {
            return Err(format!(
                "unique free-function call must resolve, got {:?}",
                resolved.resolution
            )
            .into());
        };
        let target = graph.symbol(target).ok_or("resolved target missing")?;
        assert_eq!(target.key.qualified_name, "unique_free");
        assert_eq!(target.key.kind, SymbolKind::Function);

        // Member/free collision: neither side may win silently and both
        // declarations remain enumerable.
        let colliding = single_call(&graph, "ns::Type::run_collision", "src/impl.cpp")?;
        assert_eq!(colliding.target_kind, CallTargetKind::FunctionOrMethod);
        assert_eq!(
            colliding.resolution,
            CallResolution::Ambiguous { candidates: 2 }
        );
        assert_eq!(colliding.name, "colliding");
        assert_eq!(colliding.provenance, Provenance::TreeSitter);
        assert_eq!(colliding.precision, Precision::Syntax);
        let (candidates, truncated) = graph.call_candidates(&colliding, 8);
        assert!(!truncated);
        let mut identities: Vec<_> = candidates
            .iter()
            .map(|symbol| {
                (
                    symbol.key.qualified_name.as_str(),
                    symbol.key.kind,
                    symbol.key.path.as_str(),
                )
            })
            .collect();
        identities
            .sort_unstable_by(|left, right| left.0.cmp(right.0).then_with(|| left.2.cmp(right.2)));
        assert_eq!(
            identities,
            [
                ("colliding", SymbolKind::Function, "include/api.h"),
                ("ns::Type::colliding", SymbolKind::Method, "include/api.h"),
            ]
        );

        // Two free functions, no member: honest ambiguity with both
        // candidates enumerable.
        let ambiguous = single_call(&graph, "ns::Type::helper", "src/impl.cpp")?;
        assert_eq!(ambiguous.target_kind, CallTargetKind::Function);
        assert_eq!(
            ambiguous.resolution,
            CallResolution::Ambiguous { candidates: 2 }
        );
        let (candidates, truncated) = graph.call_candidates(&ambiguous, 8);
        assert!(!truncated);
        let mut names: Vec<_> = candidates
            .iter()
            .map(|symbol| symbol.key.qualified_name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["other::shared_name", "shared_name"]);
        Ok(())
    }

    #[test]
    fn member_only_unqualified_calls_keep_member_resolution() -> Result<(), Box<dyn Error>> {
        let mut sources = BTreeMap::new();
        sources.insert(
            RepoRelativePath::new("src/lib.cpp")?,
            Arc::<str>::from(
                "namespace ns {\n\
                 struct Type {\n\
                 void run() { helper(); }\n\
                 void helper() {}\n\
                 };\n\
                 }\n",
            ),
        );
        let (_, graph) = CppSyntaxIndex::from_sources(sources)?;
        let caller = graph
            .symbols()
            .iter()
            .find(|symbol| symbol.key.qualified_name == "ns::Type::run")
            .ok_or("missing ns::Type::run")?;
        let calls: Vec<_> = graph.call_sites_from(caller.id).collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].target_kind, CallTargetKind::Method);
        let CallResolution::Resolved { target } = calls[0].resolution else {
            return Err(format!(
                "member-only call must resolve, got {:?}",
                calls[0].resolution
            )
            .into());
        };
        assert_eq!(
            graph
                .symbol(target)
                .ok_or("resolved target missing")?
                .key
                .qualified_name,
            "ns::Type::helper"
        );
        Ok(())
    }

    #[test]
    fn reconcile_reevaluates_promoted_calls_when_callable_evidence_changes()
    -> Result<(), Box<dyn Error>> {
        let mut sources = BTreeMap::new();
        sources.insert(
            RepoRelativePath::new("src/lib.cpp")?,
            Arc::<str>::from(
                "namespace ns {\n\
                 struct Type {\n\
                 void run() { helper(); }\n\
                 void helper() {}\n\
                 };\n\
                 }\n",
            ),
        );
        let (index, graph) = CppSyntaxIndex::from_sources(sources.clone())?;
        let resolution_of = |graph: &SymbolGraph| -> Result<CallResolution, Box<dyn Error>> {
            let caller = graph
                .symbols()
                .iter()
                .find(|symbol| symbol.key.qualified_name == "ns::Type::run")
                .map(|symbol| symbol.id)
                .ok_or("caller missing")?;
            let calls: Vec<_> = graph.call_sites_from(caller).collect();
            if calls.len() != 1 {
                return Err("caller must own exactly one call".into());
            }
            Ok(calls[0].resolution.clone())
        };
        assert!(matches!(
            resolution_of(&graph)?,
            CallResolution::Resolved { .. }
        ));

        // A new colliding free function in another file must flip the call to
        // explicit ambiguity without reparsing the call's own file.
        sources.insert(
            RepoRelativePath::new("src/free.cpp")?,
            Arc::<str>::from("void helper() {}\n"),
        );
        let report = index.reconcile_sources(sources.clone())?;
        let next = report.next_index.ok_or("reconcile must publish a graph")?;
        assert_eq!(report.metrics.reparsed_files, 1);
        assert_eq!(
            resolution_of(next.graph())?,
            CallResolution::Ambiguous { candidates: 2 }
        );
        next.graph().validate_consistency()?;

        // Removing it restores the member resolution; a deletion reparses
        // nothing at all.
        sources.remove(&RepoRelativePath::new("src/free.cpp")?);
        let report = next.reconcile_sources(sources)?;
        let next = report.next_index.ok_or("reconcile must publish a graph")?;
        assert_eq!(report.metrics.reparsed_files, 0);
        assert!(matches!(
            resolution_of(next.graph())?,
            CallResolution::Resolved { .. }
        ));
        next.graph().validate_consistency()?;
        Ok(())
    }

    #[test]
    fn duplicate_call_fanout_is_stored_linearly() -> Result<(), Box<dyn Error>> {
        const TARGETS: usize = 256;
        const CALLS: usize = 256;

        let mut source = String::new();
        for index in 0..TARGETS {
            source.push_str(&format!(
                "class Outer{index} {{ void target_{index}() {{}} void target() {{}} }};\n"
            ));
        }
        for index in 0..CALLS {
            source.push_str(&format!(
                "class Caller{index} {{ void caller_{index}() {{ target(); }} }};\n"
            ));
        }
        let mut sources = BTreeMap::new();
        sources.insert(
            RepoRelativePath::new("src/lib.cpp")?,
            Arc::<str>::from(source),
        );

        let started = Instant::now();
        let (_, graph) = CppSyntaxIndex::from_sources(sources)?;
        let elapsed = started.elapsed();
        let call_edges = graph
            .symbols()
            .iter()
            .flat_map(|symbol| graph.outgoing_edges(symbol.id))
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .count();

        assert_eq!(graph.call_site_count(), CALLS as u64);
        assert_eq!(graph.ambiguous_call_site_count(), CALLS as u64);
        assert_eq!(graph.unresolved_call_site_count(), 0);
        assert_eq!(call_edges, 0, "ambiguous calls must not fan out into edges");
        assert_eq!(graph.truncated_call_sites(), 0);
        eprintln!(
            "lazy_call_sites: targets={TARGETS}, calls={CALLS}, call_sites={}, call_edges={call_edges}, eager_edge_product={}, elapsed={elapsed:?}",
            graph.call_site_count(),
            TARGETS * CALLS,
        );
        Ok(())
    }

    #[test]
    fn truncated_catalog_never_turns_ambiguity_into_a_unique_call() -> Result<(), Box<dyn Error>> {
        let mut sources = BTreeMap::new();
        sources.insert(
            RepoRelativePath::new("src/a_caller.cpp")?,
            Arc::<str>::from("class Invoke { void invoke() { target(); } };\n"),
        );
        sources.insert(
            RepoRelativePath::new("src/b_target.cpp")?,
            Arc::<str>::from("class Target { void target() {} };\n"),
        );
        sources.insert(
            RepoRelativePath::new("src/c_target.cpp")?,
            Arc::<str>::from("class Target { void target() {} };\n"),
        );
        let cancellation = IndexCancellation::default();
        let (_, graph, metrics) = CppSyntaxIndex::from_sources_bounded(
            sources,
            GraphBuildLimits {
                max_symbols: 3,
                max_edges: 10,
                max_call_sites: 10,
            },
            &cancellation,
        )?;

        // Each C++ file declares three symbols (translation unit, class, and
        // method): with a three-symbol budget the caller file fits and both target
        // files are omitted entirely.
        assert_eq!(metrics.graph.omitted_symbols, 6);
        assert_eq!(metrics.graph.call_sites_omitted_by_symbol_budget, 1);
        assert_eq!(graph.call_site_count(), 0);
        let call_edges = graph
            .symbols()
            .iter()
            .flat_map(|symbol| graph.outgoing_edges(symbol.id))
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .count();
        assert_eq!(call_edges, 0);
        graph.validate_consistency()?;
        Ok(())
    }

    #[test]
    fn metadata_change_republishes_without_reparsing_source() -> Result<(), Box<dyn Error>> {
        let path = RepoRelativePath::new("src/lib.cpp")?;
        let source = Arc::<str>::from("class Stable { void stable() {} };\n");
        let initial = CppSources {
            files: BTreeMap::from([(path.clone(), source.clone())]),
            metadata: BTreeMap::from([(path.clone(), SourceMetadata::path_fallback(&path))]),
        };
        let (index, _) = CppSyntaxIndex::from_classified_sources(initial)?;
        let changed = CppSources {
            files: BTreeMap::from([(path.clone(), source)]),
            metadata: BTreeMap::from([(
                path.clone(),
                SourceMetadata {
                    role: chakra_domain::source::SourceRole::Production,
                    classification: chakra_domain::source::SourceClassification::MavenMetadata,
                    package: Some(chakra_domain::source::SourcePackage {
                        name: "app".to_owned(),
                        root: None,
                    }),
                },
            )]),
        };
        let reconciled = index.reconcile_classified_sources(changed)?;
        assert_eq!(reconciled.metrics.reparsed_files, 0);
        let graph = reconciled.graph.ok_or("metadata-only graph missing")?;
        assert_eq!(
            graph
                .file_metadata(&path)
                .and_then(|metadata| metadata.package.as_ref())
                .map(|package| package.name.as_str()),
            Some("app")
        );
        Ok(())
    }

    #[test]
    fn rejects_a_source_larger_than_the_file_budget() -> Result<(), Box<dyn Error>> {
        let repository = TempDir::new()?;
        let status = Command::new("git")
            .current_dir(repository.path())
            .args(["init", "--quiet"])
            .status()?;
        assert!(status.success());
        fs::create_dir_all(repository.path().join("src"))?;
        let file = File::create(repository.path().join("src/large.cpp"))?;
        file.set_len((MAX_SOURCE_FILE_BYTES + 1) as u64)?;

        let error = match scan_repository_sources(repository.path()) {
            Ok(_) => return Err("oversized source was indexed".into()),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            CppIndexError::SourceTooLarge { limit, .. } if limit == MAX_SOURCE_FILE_BYTES
        ));
        Ok(())
    }
}
