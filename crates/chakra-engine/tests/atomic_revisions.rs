//! Regression tests for atomic revision publication (SPEC §5, roadmap §16):
//! queries must observe the old revision or the new one, never a hybrid.

mod common;

use std::error::Error;
use std::sync::Barrier;

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::revision::Revision;
use chakra_domain::symbol::{Language, SymbolKey, SymbolKind};
use chakra_engine::{PublishError, SymbolGraph, WorkspaceEngine};

use common::{scenario_engine, scenario_graph};

fn tiny_graph() -> Result<SymbolGraph, Box<dyn Error>> {
    let mut graph = SymbolGraph::new();
    let file = RepoRelativePath::new("src/lib.rs")?;
    let position = TextPosition::new(1, 1)?;
    graph.add_symbol(
        SymbolKey {
            language: Language::Rust,
            qualified_name: "only".to_owned(),
            container: None,
            kind: SymbolKind::Function,
            path: file.clone(),
        },
        SourceRange::new(file, position, position)?,
        None,
        Provenance::TreeSitter,
        Precision::Syntax,
    )?;
    Ok(graph)
}

fn engine_with_scenario() -> Result<WorkspaceEngine, Box<dyn Error>> {
    Ok(scenario_engine()?.0)
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
fn readers_observe_old_snapshot_until_publish_then_new() -> Result<(), Box<dyn Error>> {
    // Deterministic phase handshake: readers MUST observe the old snapshot
    // while a private update is fully prepared, and the new one after
    // publish — regardless of scheduling. This is the regression test for
    // "queries cannot observe a partially published revision" (SPEC §5).
    //
    // Every participant records its first failure and still reaches every
    // remaining rendezvous, so a failed check can never strand the other
    // threads on a `Barrier`.
    let engine = engine_with_scenario()?;
    let base_revision = engine.snapshot().revision();
    let scenario_symbols = engine.snapshot().graph().symbol_count();
    let tiny = tiny_graph()?;
    let tiny_symbols = tiny.symbol_count();

    const READERS: usize = 3;
    let update_ready = Barrier::new(READERS + 1);
    let observed_old = Barrier::new(READERS + 1);
    let published = Barrier::new(READERS + 1);

    std::thread::scope(|scope| {
        let publisher = scope.spawn(|| -> Result<Revision, String> {
            let mut update = engine.begin_update();
            update.replace_graph(tiny);
            // The private update is fully prepared but NOT published.
            update_ready.wait();
            // While the update sits ready, readers must still see the old
            // snapshot; they signal once every reader checked.
            observed_old.wait();
            let outcome = engine
                .publish(update)
                .map(|snapshot| snapshot.revision())
                .map_err(|error| error.to_string());
            // Reached even when publish failed, so readers never wait forever.
            published.wait();
            outcome
        });

        let mut readers = Vec::new();
        for _ in 0..READERS {
            readers.push(scope.spawn(|| -> Result<(), String> {
                let mut failure: Option<String> = None;
                let mut record = |check: Result<(), String>| {
                    if failure.is_none() {
                        failure = check.err();
                    }
                };

                update_ready.wait();
                let before = engine.snapshot();
                if before.revision() != base_revision {
                    record(Err(
                        "private update leaked into the published slot".to_owned()
                    ));
                } else if before.graph().symbol_count() != scenario_symbols {
                    record(Err("reader observed a partially replaced graph".to_owned()));
                } else {
                    record(
                        before.graph().validate_consistency().map_err(|error| {
                            format!("inconsistent snapshot before publish: {error}")
                        }),
                    );
                }
                observed_old.wait();

                published.wait();
                let after = engine.snapshot();
                if after.revision() != base_revision.next() {
                    record(Err("publish not atomically visible to a reader".to_owned()));
                } else if after.graph().symbol_count() != tiny_symbols {
                    record(Err(
                        "reader observed a hybrid of old and new graphs".to_owned()
                    ));
                } else {
                    record(
                        after.graph().validate_consistency().map_err(|error| {
                            format!("inconsistent snapshot after publish: {error}")
                        }),
                    );
                }
                // The snapshot pinned before publish still observes the
                // complete old state.
                if before.graph().symbol_count() != scenario_symbols {
                    record(Err("a held snapshot changed under the reader".to_owned()));
                }
                failure.map_or(Ok(()), Err)
            }));
        }

        let published_revision = publisher
            .join()
            .map_err(|_| std::io::Error::other("publisher panicked"))?
            .map_err(std::io::Error::other)?;
        assert_eq!(published_revision, base_revision.next());
        for reader in readers {
            reader
                .join()
                .map_err(|_| std::io::Error::other("reader panicked"))?
                .map_err(std::io::Error::other)?;
        }
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
    // Every reader performs exactly this many snapshot checks, so the test
    // cannot pass vacuously regardless of how threads are scheduled.
    const OBSERVATIONS_PER_READER: usize = 500;
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
            Ok(())
        });

        let mut readers = Vec::new();
        for _ in 0..3 {
            readers.push(scope.spawn(|| -> Result<usize, String> {
                start.wait();
                let mut last_revision = Revision::INITIAL;
                let mut observations = 0;
                for _ in 0..OBSERVATIONS_PER_READER {
                    let snapshot = engine.snapshot();
                    if snapshot.revision() < last_revision {
                        return Err("revision went backwards for a reader".to_owned());
                    }
                    if snapshot.revision() < Revision(1) {
                        return Err("reader observed the unindexed initial revision".to_owned());
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
                    observations += 1;
                }
                Ok(observations)
            }));
        }

        publisher
            .join()
            .map_err(|_| std::io::Error::other("publisher panicked"))?
            .map_err(std::io::Error::other)?;
        for reader in readers {
            let observations = reader
                .join()
                .map_err(|_| std::io::Error::other("reader panicked"))?
                .map_err(std::io::Error::other)?;
            assert_eq!(observations, OBSERVATIONS_PER_READER);
        }
        Ok(())
    })
}
