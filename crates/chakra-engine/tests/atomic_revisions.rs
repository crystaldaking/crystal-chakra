//! Regression tests for atomic revision publication (SPEC §5, roadmap §16):
//! queries must observe the old revision or the new one, never a hybrid.

mod common;

use std::error::Error;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};

use chakra_engine::{PublishError, SymbolGraph, WorkspaceEngine};

use common::scenario_graph;

fn engine_with_scenario() -> Result<WorkspaceEngine, Box<dyn Error>> {
    let identity = chakra_domain::identity::WorkspaceIdentity::for_primary_worktree(
        std::path::Path::new("."),
    )?;
    let engine = WorkspaceEngine::new(identity);
    let (graph, _) = scenario_graph()?;
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    engine.publish(update)?;
    Ok(engine)
}

/// A single-provider graph that differs from the scenario in every count.
fn tiny_graph() -> Result<SymbolGraph, Box<dyn Error>> {
    let mut graph = SymbolGraph::new();
    let file = chakra_domain::location::RepoRelativePath::new("src/lib.rs")?;
    let position = chakra_domain::location::TextPosition { line: 1, column: 1 };
    graph.add_symbol(
        chakra_domain::symbol::SymbolKey {
            language: chakra_domain::symbol::Language::Rust,
            qualified_name: "only".to_owned(),
            container: None,
            kind: chakra_domain::symbol::SymbolKind::Function,
            path: file.clone(),
        },
        chakra_domain::location::SourceRange {
            file,
            start: position,
            end: position,
        },
        None,
        chakra_domain::provenance::Provenance::TreeSitter,
        chakra_domain::provenance::Precision::Syntax,
    )?;
    Ok(graph)
}

#[test]
fn concurrent_publishers_have_exactly_one_winner() -> Result<(), Box<dyn Error>> {
    let engine = engine_with_scenario()?;
    // Both updates are based on the same revision before either thread runs,
    // so the outcome does not depend on scheduling.
    let first = engine.begin_update();
    let second = engine.begin_update();

    let barrier = Barrier::new(3);
    std::thread::scope(|scope| {
        let first_handle = scope.spawn(|| {
            barrier.wait();
            engine.publish(first)
        });
        let second_handle = scope.spawn(|| {
            barrier.wait();
            engine.publish(second)
        });
        barrier.wait();
        let results = [first_handle.join(), second_handle.join()];
        let mut wins = 0;
        let mut conflicts = 0;
        for result in results {
            match result {
                Ok(Ok(_snapshot)) => wins += 1,
                Ok(Err(PublishError::Conflict { .. })) => conflicts += 1,
                Err(_panic) => {
                    return Err(std::io::Error::other("publisher thread panicked").into());
                }
            }
        }
        assert_eq!(wins, 1);
        assert_eq!(conflicts, 1);
        Ok(())
    })
}

#[test]
fn readers_never_observe_partial_revisions() -> Result<(), Box<dyn Error>> {
    let engine = engine_with_scenario()?;
    let scenario_symbols = engine.snapshot().graph().symbol_count();
    let (scenario, _) = scenario_graph()?;
    let tiny = tiny_graph()?;
    let tiny_symbols = tiny.symbol_count();

    const PUBLISHES: usize = 100;
    let readers_done = AtomicBool::new(false);
    let start = Barrier::new(4); // 3 readers + publisher

    std::thread::scope(|scope| {
        let publisher = scope.spawn(|| -> Result<(), String> {
            start.wait();
            for round in 0..PUBLISHES {
                let mut update = engine.begin_update();
                if round % 2 == 0 {
                    update.replace_graph(tiny.clone());
                } else {
                    update.replace_graph(scenario.clone());
                }
                engine.publish(update).map_err(|error| error.to_string())?;
            }
            readers_done.store(true, Ordering::Release);
            Ok(())
        });

        let mut readers = Vec::new();
        for _ in 0..3 {
            readers.push(scope.spawn(|| -> Result<(), String> {
                start.wait();
                let mut last_revision = chakra_domain::revision::Revision::INITIAL;
                while !readers_done.load(Ordering::Acquire) {
                    let snapshot = engine.snapshot();
                    if snapshot.revision() < last_revision {
                        return Err("revision went backwards for a reader".to_owned());
                    }
                    last_revision = snapshot.revision();
                    let graph = snapshot.graph();
                    graph
                        .validate_consistency()
                        .map_err(|error| format!("hybrid state observed: {error}"))?;
                    let count = graph.symbol_count();
                    if count != scenario_symbols && count != tiny_symbols {
                        return Err(format!("partial graph observed: {count} symbols"));
                    }
                }
                Ok(())
            }));
        }

        publisher
            .join()
            .map_err(|_| std::io::Error::other("publisher panicked"))?
            .map_err(std::io::Error::other)?;
        for reader in readers {
            reader
                .join()
                .map_err(|_| std::io::Error::other("reader panicked"))?
                .map_err(std::io::Error::other)?;
        }
        Ok(())
    })
}
