//! Deterministic Go syntax indexing on the shared language-neutral driver
//! (issue #94). This module keeps only the Go seams: the Tree-sitter parser
//! hook, Git-aware discovery, and worker naming.

use std::path::Path;
use std::sync::Arc;

use chakra_domain::location::RepoRelativePath;
use chakra_git::ClassifiedSource;
use chakra_language_index::{LanguageHooks, LanguageParser};

use crate::discovery::discover_go_sources;
use crate::parser::GoParser;

pub use chakra_language_index::{
    IndexMetrics, LanguageBuildMetrics, ReconcileMetrics, SyntaxFactCounts,
};

/// Failure to discover, read, parse, or validate the Go syntax index.
pub type GoIndexError = chakra_language_index::LanguageIndexError;
/// Latest Go source text plus role/package metadata from the same scan.
pub type GoSources = chakra_language_index::LanguageSources;
/// Reusable per-file Go syntax facts and per-owner relationship
/// contributions.
pub type GoSyntaxIndex = chakra_language_index::LanguageSyntaxIndex<GoHooks>;
/// Complete private initial Go index, ready for atomic publication.
pub type IndexReport = chakra_language_index::IndexReport<GoHooks>;
/// Reconcile outcome for the Go syntax index.
pub type ReconcileReport = chakra_language_index::ReconcileReport<GoHooks>;

/// Go seams of the shared indexing driver.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoHooks;

impl LanguageHooks for GoHooks {
    type Parser = GoParser;

    const WORKER_NAME: &'static str = "go";

    fn language() -> chakra_domain::symbol::Language {
        chakra_domain::symbol::Language::Go
    }

    fn new_parser() -> Result<Self::Parser, GoIndexError> {
        GoParser::new().map_err(|error| GoIndexError::Parse(error.to_string()))
    }

    fn discover_sources(root: &Path) -> Result<Vec<ClassifiedSource>, GoIndexError> {
        Ok(discover_go_sources(root)?)
    }
}

impl LanguageParser for GoParser {
    fn parse(
        &mut self,
        path: RepoRelativePath,
        source: Arc<str>,
    ) -> Result<chakra_language_index::ParsedFile, GoIndexError> {
        GoParser::parse(self, path, source).map_err(|error| GoIndexError::Parse(error.to_string()))
    }
}

/// Builds a complete Go syntax index from the actual materialized Git
/// worktree. The caller owns atomic publication into `WorkspaceEngine`.
pub fn index_repository(root: &Path) -> Result<IndexReport, GoIndexError> {
    chakra_language_index::index_repository::<GoHooks>(root)
}

/// Reads the latest Git-aware Go file inventory and exact contents.
pub fn scan_repository_sources(repository_root: &Path) -> Result<GoSources, GoIndexError> {
    chakra_language_index::scan_repository_sources::<GoHooks>(repository_root)
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
    use chakra_domain::source::SourceMetadata;
    use chakra_domain::symbol::EdgeKind;
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
                RepoRelativePath::new(format!("src/generated_{index:03}.go"))?,
                Arc::<str>::from(format!(
                    "package generated\nfunc caller_{index}() {{ helper_{index}() }}\nfunc helper_{index}() {{}}\n"
                )),
            );
        }
        let cancellation = IndexCancellation::default();
        let (_, sequential, sequential_metrics) = GoSyntaxIndex::from_sources_scheduled(
            sources.clone(),
            GraphBuildLimits::UNLIMITED,
            1,
            1,
            &cancellation,
        )?;
        let (_, parallel, parallel_metrics) = GoSyntaxIndex::from_sources_scheduled(
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
                RepoRelativePath::new(format!("src/cancel_{index:03}.go"))?,
                Arc::<str>::from(format!("package cancel\nfunc item_{index}() {{}}\n")),
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
            parse_sources_scheduled_observed::<GoHooks>(
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
        assert!(matches!(result, Err(GoIndexError::Cancelled)));
        Ok(())
    }

    #[test]
    fn duplicate_call_fanout_is_stored_linearly() -> Result<(), Box<dyn Error>> {
        const TARGETS: usize = 256;
        const CALLS: usize = 256;

        let mut source = String::new();
        source.push_str("package fanout\n");
        for _ in 0..TARGETS {
            source.push_str("type Common struct{}\nfunc (o Common) target() {}\n");
        }
        for index in 0..CALLS {
            source.push_str(&format!(
                "func caller_{index}() {{ Common{{}}.target() }}\n"
            ));
        }
        let mut sources = BTreeMap::new();
        sources.insert(
            RepoRelativePath::new("src/lib.go")?,
            Arc::<str>::from(source),
        );

        let started = Instant::now();
        let (_, graph) = GoSyntaxIndex::from_sources(sources)?;
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
            RepoRelativePath::new("src/a_caller.go")?,
            Arc::<str>::from(
                "package truncated\ntype Invoke struct{}\nfunc (i Invoke) invoke() { i.target() }\n",
            ),
        );
        sources.insert(
            RepoRelativePath::new("src/b_target.go")?,
            Arc::<str>::from(
                "package truncated\ntype TargetB struct{}\nfunc (t TargetB) target() {}\n",
            ),
        );
        sources.insert(
            RepoRelativePath::new("src/c_target.go")?,
            Arc::<str>::from(
                "package truncated\ntype TargetC struct{}\nfunc (t TargetC) target() {}\n",
            ),
        );
        let cancellation = IndexCancellation::default();
        let (_, graph, metrics) = GoSyntaxIndex::from_sources_bounded(
            sources,
            GraphBuildLimits {
                max_symbols: 3,
                max_edges: 10,
                max_call_sites: 10,
            },
            &cancellation,
        )?;

        // Each Go file declares three symbols (file module, struct, and method):
        // with a three-symbol budget the caller file fits and both target
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
        let path = RepoRelativePath::new("src/lib.go")?;
        let source = Arc::<str>::from("package stable\nfunc stable() {}\n");
        let initial = GoSources {
            files: BTreeMap::from([(path.clone(), source.clone())]),
            metadata: BTreeMap::from([(path.clone(), SourceMetadata::path_fallback(&path))]),
        };
        let (index, _) = GoSyntaxIndex::from_classified_sources(initial)?;
        let changed = GoSources {
            files: BTreeMap::from([(path.clone(), source)]),
            metadata: BTreeMap::from([(
                path.clone(),
                SourceMetadata {
                    role: chakra_domain::source::SourceRole::Production,
                    classification: chakra_domain::source::SourceClassification::GoModuleMetadata,
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
        let file = File::create(repository.path().join("src/large.go"))?;
        file.set_len((MAX_SOURCE_FILE_BYTES + 1) as u64)?;

        let error = match scan_repository_sources(repository.path()) {
            Ok(_) => return Err("oversized source was indexed".into()),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            GoIndexError::SourceTooLarge { limit, .. } if limit == MAX_SOURCE_FILE_BYTES
        ));
        Ok(())
    }
}
