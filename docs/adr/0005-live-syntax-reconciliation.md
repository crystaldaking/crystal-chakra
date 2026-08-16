# ADR-005: Live syntax reconciliation and freshness barriers

Status: accepted
Date: 2026-08-15
Last reviewed: 2026-08-16

## Context

Roadmap §§5, 6, 9, 10, and 16 require an edit to become queryable without a
caller sleep, while preserving atomic workspace revisions and avoiding a full
repository reindex. SPEC §§20–22, 34–37, and 44–45 make filesystem events
notifications rather than source-of-truth facts: a fresh result must be based
on reconciliation against Git plus the materialized worktree and must recover
from dropped, duplicated, or reordered events.

The upstream `notify` project provides a maintained cross-platform Rust API
over native filesystem mechanisms:

- <https://github.com/notify-rs/notify>
- <https://docs.rs/notify/8.2.0/notify/>

## Decision

- Use workspace-managed `notify` 8.2.0 and `recommended_watcher`, selecting
  the current stable release rather than a release candidate. Watch only the
  repository root and the existing ancestor directories of indexed Rust/PHP
  files, non-recursively. This avoids recursive watches inside `.git`,
  `target`, ignored, and generated trees. The directory set is capped at
  4,096; exceeding the cap degrades watcher health but does not disable exact
  freshness reconciliation.
- Treat mutation-capable watcher events as wake-up hints. Pure access/open/read
  events are ignored: Linux inotify reports the indexer's own content reads,
  and allowing those events to advance the epoch would make a stable scan
  invalidate itself. Create/modify/remove/unknown events, close-after-write,
  and watcher errors remain conservative reconciliation signals. The callback
  performs no filesystem I/O or parsing. Under a small publication gate it
  first atomically publishes stale lifecycle metadata (sharing the immutable
  graph), then advances the event epoch and uses `try_send` into a bounded
  256-item channel. The worker holds the same gate only for the final epoch
  check and fresh publication, so a newly observed event can never coexist
  with an older graph labeled fresh. A full channel increments an observable
  dropped-event counter. Correctness never depends on every event reaching the
  worker.
- Own the watcher and syntax state in one named blocking worker thread. The
  worker has an explicit shutdown signal and join path. It coalesces event
  bursts with a 50 ms quiet window capped at 250 ms, which tolerates common
  temporary-file and rename-over-target save sequences without unbounded
  delay or queue growth. Parsing, Git subprocesses, and filesystem reads do
  not block Tokio runtime workers.
- Make `FreshnessBarrier` a small language-neutral engine contract. The
  `chakra-language` workspace owner implements it with monotonic
  request/completion generations plus a
  condition variable. Every syntax query using `RequireFresh` requests a
  reconciliation and waits for its generation; MCP runs these synchronous
  domain queries on its bounded blocking executor. Static engines retain the
  previously published freshness behavior when no live owner is installed.
- Reconcile by asking Git for the current tracked plus untracked non-ignored
  Rust/PHP inventory, reading exact current contents, and requiring two identical
  scans with an unchanged watcher epoch. A bounded retry handles replacement
  races. This scan is authoritative even when events were missed or reordered.
  Unchanged source text is compared exactly and is never reparsed. If an event
  advances the epoch after a stable scan, the private candidate is discarded
  and reconciliation retries instead of publishing it.
- Cache parsed facts per file and resolved relationship contributions per
  relationship-owner file. An edit reparses only created or modified files.
  Relationship owners are recomputed only when they own a changed file or
  depend on a declaration/callable key exported by one. Revision-scoped
  `EntityId` values are assigned only while materializing a complete private
  graph from cached facts and contributions.
- Publish a changed graph with one engine compare-and-publish operation. A
  no-content-change reconciliation may update stale lifecycle metadata but
  does not invent a new graph revision when the current state is already
  fresh. Publication failure leaves the prior reusable index state installed.
  Tree-sitter error trees remain valid syntax revisions, so temporary syntax
  errors do not expose a partial graph or erase valid declarations elsewhere.
- Instrument reconciliations, publications, scanned/unchanged/reparsed files,
  recomputed relationship owners, create/modify/delete counts, syntax-error
  files, watcher events/errors/drops, and watched directories. Evidence for an
  incremental edit comes from actual reparsed-file and recomputed-owner counts;
  there is no constant-valued surrogate “full reindex” counter. The initial
  full index remains a distinct startup operation.
- Watcher degradation is current-state metadata. A later successful watch-set
  refresh can return the workspace to `Ready`; cumulative error counters remain
  available for diagnostics but do not permanently poison current health.

## Alternatives considered

- Trust individual create/modify/rename/delete events and patch paths
  directly: rejected because native backends differ, atomic saves create
  transient paths, and dropped/reordered events would make freshness claims
  unsound.
- Recursively watch the whole repository: rejected because it watches Git
  administration and excluded/generated trees, consumes more platform watch
  resources, and still cannot replace reconciliation.
- Use an unbounded async channel: rejected because event storms must not
  create unbounded memory or work. The blocking worker owns all slow work and
  the query adapter already bounds concurrent blocking queries.
- Rebuild all parsed files or all relationship contributions after every
  event: rejected because normal edits must not become repository-wide
  reindexes. Complete graph materialization remains necessary for the current
  revision-scoped arena, but it consumes cached per-file facts and cached
  unaffected relationship contributions.
- Ask callers to delay and retry: rejected because it is nondeterministic and
  makes read-your-writes correctness a client responsibility.

## Consequences

- Production dependency added: `notify` 8.2.0 (CC0-1.0). On macOS it adds the
  native FSEvents adapter; the lockfile also carries target-specific native
  adapters for supported platforms. The stable crate declares Rust 1.77 as
  its minimum, below Chakra's pinned Rust 1.97.1. Compile and transitive cost
  are accepted for a mature native watcher rather than reimplementing
  platform APIs.
- A fresh barrier performs repository inventory and content reads, but only
  changed files incur Tree-sitter parsing and only affected owners incur
  relationship resolution. Benchmarks may later justify a more selective
  reconciliation proof; v0.1 does not add a speculative persistent cache.
- Watcher degradation affects responsiveness, not correctness: a
  `RequireFresh` query still performs authoritative reconciliation. A failed
  reconciliation publishes stale/degraded metadata and returns a typed error.
- A workspace engine accepts one live freshness owner for its lifetime. The
  owner is shut down and joined when MCP serving ends.

## Validation / follow-up

- Deterministic real-Git integration tests cover immediate fresh reads,
  immutable old versus new revisions, one-file reparse/no full reindex,
  create/rename/delete, atomic replacement, temporary syntax errors, rapid
  editor-style replacement bursts, and recovery. Queries synchronize through
  the barrier; no arbitrary test sleep is used.
- A pure unit test checks both quiet and absolute debounce deadlines using
  synthetic instants.
- The hardening measurement records the fresh barrier and reparse counters for
  both one ordinary edit and a 32-replacement burst. The ordinary edit reparses
  one file and recomputes only the affected relationship owner set.
- A unit regression proves the callback revokes freshness before publishing
  its epoch, and integration tests verify old snapshots remain immutable while
  the new revision is published atomically.
- A deterministic event-classification regression proves source read/open
  access does not invalidate reconciliation while mutation-capable events do.
- Periodic background reconciliation is deferred. v0.1 recovers missed events
  on every fresh-query barrier, which is the correctness boundary required by
  the current query contract.
