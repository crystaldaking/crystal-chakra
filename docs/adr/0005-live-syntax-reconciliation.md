# ADR-005: Live syntax reconciliation and freshness barriers

Status: accepted
Date: 2026-08-15
Last reviewed: 2026-08-21

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
  the current stable release rather than a release candidate. On macOS,
  compile the documented `macos_kqueue` backend instead of the default
  FSEvents backend: repeated `FSEventStreamStart` calls can block indefinitely
  during watcher registration, while kqueue avoids that API and reports
  registration errors through the existing typed startup/degradation paths.
  Watch only the repository root and the existing ancestor directories of
  indexed source files, non-recursively. This avoids recursive watches inside
  `.git`, `target`, ignored, and generated trees. The directory set is capped at
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
- Resolve the worktree-specific and common Git administrative directories at
  startup with Git's absolute `rev-parse --git-dir` and `--git-common-dir`
  results. Events wholly inside those directories are ignored without assuming
  that administration is a `.git` directory below the worktree. Source event
  paths are retained only as a bounded 32-path hint set; an empty,
  out-of-worktree, invalid, or overflowing set is conservative uncertainty
  rather than proof of what changed. An in-worktree non-source path still wakes
  reconciliation but is not itself uncertainty: the two Git inventory
  checkpoints prove source/metadata membership, so ordinary editor temporary
  paths do not force a full body reread before their source rename target is
  reconciled.
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
- Capture the latest requested barrier generation after the candidate content
  snapshot but before its final verification. One successful verification
  completes every generation covered by that snapshot; later generations stay
  pending. A barrier arriving during an editor burst does not cut the bounded
  quiet window short, so write/metadata/rename sequences converge to one latest
  state without requiring a caller sleep.
- Reconcile from one Git-aware tracked plus untracked non-ignored inventory.
  It contains admitted registered-language sources and the Git-visible
  ecosystem manifests, lockfiles, toolchain files, and configuration that can
  change query-visible classification. Partition its source set by language;
  do not rediscover a language or its metadata separately. A stable
  reconciliation has two identical shared-inventory checkpoints around one
  content/classification snapshot, unchanged watcher epoch, and identical
  pre/post filesystem identities for every admitted source and metadata input.
  It does not perform the former two complete body scans. A bounded retry
  retains the already observed candidate cache, so a delayed notification for
  state already captured does not reread that body. The scan remains
  authoritative even when a notification has not reached user space.
- Cache source bodies by repository-relative path and a strong filesystem
  identity. On Unix that identity includes length, device, inode, mode, mtime,
  ctime, and their nanosecond components; mtime alone is never sufficient. A
  matching identity reuses the immutable body, while a mismatch rereads that
  file and exact source comparison prevents an unchanged body from being
  reparsed. Platforms without that identity strength conservatively reread
  admitted bodies rather than weakening `RequireFresh`.
- Force a full bounded body reread when the cache is uninitialized, a watcher
  error or dropped event advanced, an uncovered watcher epoch is
  non-contiguous/reordered, an event hint is uncertain, a Git inventory
  checkpoint is uncertain, or the configurable periodic
  checkpoint is due. `LiveIndexOptions` defaults the checkpoint interval to
  256 successful reconciliations and rejects zero. Ordinary known changes and
  no-op barriers use non-full paths. A stable partial watch set caused by the
  4,096-directory cap keeps lifecycle status degraded but does not itself
  force full body rereads or repeated watch reinstallation: every fresh
  barrier still verifies the complete Git inventory and every retained source
  identity. Watch directories are recomputed only when the indexed path set
  changes or a newly observed watcher error requires reinstallation.
- Reconciliation reuses the initial revision's validated indexing budgets and
  one shared source/metadata Git inventory per scan (ADR-011). Budget
  coverage and degradation are published atomically with changed graph
  contents. A reconciled budget-limited graph is truthfully `Degraded` and `Fresh`; adding
  an over-budget file may publish metadata without pretending it was indexed.
- Cache parsed facts per file and resolved relationship contributions per
  relationship-owner file. An edit reparses only created or modified files.
  Relationship owners are recomputed only when they own a changed file or
  depend on a declaration/callable key exported by one. Revision-scoped
  `EntityId` values are assigned inside the private language partition. An
  ordinary edit clones persistent roots, removes only affected relationship and
  caller contributions, replaces changed-file declarations, and adds those
  contributions back. Unchanged file/symbol/edge/call payloads remain shared
  with the prior immutable revision. A budget rebalance or previously degraded
  graph may deliberately take the observable full-build fallback.
- Publish a changed graph with one engine compare-and-publish operation. A
  no-content-change reconciliation may update stale lifecycle metadata but
  does not invent a new graph revision when the current state is already
  fresh. Publication failure leaves the prior reusable index state installed.
  Tree-sitter error trees remain valid syntax revisions, so temporary syntax
  errors do not expose a partial graph or erase valid declarations elsewhere.
- Instrument barrier requests/completed/coalesced generations, reconciliation
  kind, Git subprocesses, source and metadata filesystem identities/bytes
  inspected, source bodies and bytes read, full/targeted/no-op reconciliations,
  watch-set recomputations, publications, scanned/unchanged/reparsed files,
  recomputed relationship owners, create/modify/delete counts, syntax-error
  files, watcher events/errors/drops, watched directories, and graph
  files/source bytes/symbols/edges/call sites reused, rebuilt, or copied.
  Evidence for an
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
  reindexes. Complete graph materialization was also removed from the ordinary
  path: persistent file-owned deltas and shallow language composition publish
  the next complete graph without visiting/copying all retained facts.
- Ask callers to delay and retry: rejected because it is nondeterministic and
  makes read-your-writes correctness a client responsibility.

## Consequences

- Production dependency added: `notify` 8.2.0 (CC0-1.0). On macOS Chakra
  enables its documented kqueue feature (with the target-specific `kqueue`
  and `mio` dependencies) instead of FSEvents; other supported platforms keep
  their native adapters. The stable crate declares Rust 1.77 as its minimum,
  below Chakra's pinned Rust 1.97.1. Compile and transitive cost are accepted
  for a mature native watcher rather than reimplementing platform APIs.
- A warmed no-op fresh barrier performs two shared Git inventory checkpoints
  and two filesystem identity passes but reads zero source bodies. Both
  identity passes cover sources and classification inputs and are required to
  prove that no query-visible fact changed while the snapshot was assembled;
  watcher silence cannot replace either proof. On the recorded
  `psp-app` corpus this costs 23.9–26.2 ms in release mode versus the former
  91.7–98.8 ms (3.5–4.1× faster). The issue's preferred 5× target is not used as
  a reason to weaken read-your-writes: 27 ms is the explicitly measured accepted
  correctness budget; further identity-scan tuning requires a separately
  measured change. Persistent graph nodes solve publication copying; they do not
  replace Git/worktree reconciliation and are not an on-disk cache.
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
- A same-length write with restored mtime proves that ctime-backed identity
  invalidation exposes the latest source immediately and reparses only that
  file. A configured checkpoint regression proves the full-reread policy, and
  pure policy regressions cover watcher degradation, errors, dropped events,
  uncertain hints, and periodic checkpoints; an epoch-sequence regression
  covers gaps and reordering.
- A clean `diff_context` regression proves its two revision-pin barriers issue
  four lightweight Git inventory subprocesses in total, read zero source
  bodies, and perform no full reconciliation. The second barrier remains
  necessary to reject a mixed syntax/Git join.
- The opt-in large-workspace harness records warmed no-op latency, Git
  subprocesses, identities/bytes inspected, and source bodies/bytes read. The
  2026-08-18 release-mode `psp-app` run used 1,158 admitted sources, ten samples,
  23,898/25,690/26,217 µs min/median/max, twenty Git subprocesses, and zero
  source bodies or bytes read.
- A pure unit test checks both quiet and absolute debounce deadlines using
  synthetic instants.
- A macOS-only unit test pins `RecommendedWatcher` to kqueue, and the parallel
  conformance suite exercises repeated owned watcher startup and shutdown
  without the former FSEvents registration stall (issue #65).
- The pinned Java Spring Boot corpus exceeds the non-recursive watch-directory
  cap. Its freshness scenarios prove that stable partial notification coverage
  remains bounded and degraded while fresh barriers complete through
  authoritative inventory/identity reconciliation instead of repeatedly
  reinstalling 4,096 watches and forcing whole-workspace body rereads.
- The hardening measurement records the fresh barrier and reparse counters for
  both one ordinary edit and a 32-replacement burst. The ordinary edit reparses
  one file and recomputes only the affected relationship owner set.
- The same regression pins the old snapshot, verifies physical sharing of the
  unchanged file and symbol, and checks publication metrics report one rebuilt
  file, zero copied source/symbol/call payloads, and fewer copied adjacency
  entries than a complete two-direction graph copy.
- A unit regression proves the callback revokes freshness before publishing
  its epoch, and integration tests verify old snapshots remain immutable while
  the new revision is published atomically.
- A deterministic event-classification regression proves source read/open
  access does not invalidate reconciliation while mutation-capable events do.
- Periodic background reconciliation remains deferred. Every fresh-query
  barrier recovers missed events, and periodic *full checkpoints within those
  successful barriers* limit retained-identity risk without adding an
  autonomous scan loop.
