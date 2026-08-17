# ADR-001: Workspace revision publication model

Status: accepted
Date: 2026-08-15
Last reviewed: 2026-08-16

## Context

SPEC §5 requires that queries never observe partially applied updates: a
query sees revision N or N+1, never a hybrid. SPEC §35 prefers
immutable/read-mostly published revisions, private update construction, and
per-workspace ownership over a global lock. Roadmap §6 makes this
non-deferrable because retrofitting it would distort the engine.

## Decision

- `WorkspaceEngine` is the single owner of the published revision.
- State lives in immutable `WorkspaceSnapshot` values (identity, revision,
  status, freshness, provider state, indexing coverage, symbol graph). Once
  published, a snapshot never changes. Budgets, capability completeness,
  degradation reasons, phase measurements, and memory samples therefore
  cannot race ahead of or lag behind the graph they describe (ADR-011).
- Freshness is a snapshot axis of its own, independent from the lifecycle
  status (SPEC §6): `Ready` does not imply `Fresh`, and a `Degraded`
  workspace may still hold a reconciled syntax snapshot. Only the publisher
  claims freshness — the query layer never derives it. An update inherits
  the base snapshot's freshness while the graph is untouched; touching the
  graph (`graph_mut` / `replace_graph`) revokes it, so the reconciling
  publisher must re-claim `Fresh` explicitly after its edits, once
  reconciliation against the worktree is confirmed. Queries enforce each
  request's `FreshnessRequirement` against the pinned snapshot and reject
  unmet ones with a typed error.
- Publication is an atomic pointer swap: the engine holds
  `ArcSwap<WorkspaceSnapshot>`; a query pins one `Arc<WorkspaceSnapshot>`
  up front and observes exactly one revision.
- Snapshots and update builders hold `Arc<SymbolGraph>`. Metadata-only
  publications share the immutable graph allocation; the first graph mutation
  in a builder uses copy-on-write, while a rebuilt graph replaces the `Arc`.
  Updates are committed via
  compare-and-publish: `publish` fails with `PublishError::Conflict` when
  the builder's base revision no longer matches, using
  `ArcSwap::compare_and_swap` so even concurrent publishers cannot
  clobber each other.
- The query contract is synchronous: a query holds an `Arc` to a complete
  snapshot, so it never blocks on or races with publication.

## Alternatives considered

- `RwLock<Arc<WorkspaceSnapshot>>`: equivalent semantics, but readers take
  a lock and poisoning turns a writer panic into a reader-visible error
  path. `arc-swap` gives lock-free reads and has no poisoning mode.
- Global `Mutex<Everything>`: rejected by SPEC §35 and the phase-01
  constraints.
- Persistent/immutable data structures (e.g. `im`): would make incremental
  updates cheaper, but add a dependency and design complexity before any
  measurement justifies it (SPEC §33, roadmap §12).
- Actor/owning task with message passing: no concrete benefit at this
  scale; rejected by SPEC §35's "no actor framework without cause".

## Consequences

- One extra dependency: `arc-swap` (small, mature, no transitive weight of
  note).
- Metadata-only lifecycle/freshness updates do not clone the graph. Graph
  mutations still clone on first write, which keeps private construction
  simple without adding a persistent-collection dependency.
- Query envelope schema v2 copies the pinned snapshot's `IndexingStatus`, so
  every query distinguishes complete and deliberately degraded revisions.
- A lost update race surfaces as a typed `Conflict`. The live reconciliation
  publisher retries from a fresh base with a fixed attempt bound rather than
  overwriting a newer revision.

## Validation / follow-up

- `crates/chakra-engine/tests/atomic_revisions.rs` proves: a held snapshot
  stays immutable after publish; concurrent publishers produce exactly one
  winner; readers observe the old snapshot while a private update is
  prepared and the new one after publish (deterministic barrier handshake);
  concurrent readers never observe a hybrid or a backwards revision.
- `crates/chakra-engine/tests/freshness.rs` proves the freshness contract:
  `RequireFresh` is rejected with a typed error until reconciliation claims
  `Fresh`, `AllowStale` is served with a stale envelope, and status/freshness
  combinations stay independent.
- Revisit copy-on-write graph mutations only with benchmark data (roadmap
  §18).
