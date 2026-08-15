# ADR-001: Workspace revision publication model

Status: accepted
Date: 2026-08-15

## Context

SPEC §5 requires that queries never observe partially applied updates: a
query sees revision N or N+1, never a hybrid. SPEC §35 prefers
immutable/read-mostly published revisions, private update construction, and
per-workspace ownership over a global lock. Roadmap §6 makes this
non-deferrable because retrofitting it would distort the engine.

## Decision

- `WorkspaceEngine` is the single owner of the published revision.
- State lives in immutable `WorkspaceSnapshot` values (identity, revision,
  status, provider state, symbol graph). Once published, a snapshot never
  changes.
- Publication is an atomic pointer swap: the engine holds
  `ArcSwap<WorkspaceSnapshot>`; a query pins one `Arc<WorkspaceSnapshot>`
  up front and observes exactly one revision.
- Updates are built privately in an `UpdateBuilder` (clone of the current
  graph, or a replacement graph for full rebuilds) and committed via
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
- Every update currently clones the graph. At v0.1 fixture scale this is
  noise; if measurements show it matters, the snapshot internals can switch
  to structural sharing without changing the publication model.
- A lost update race surfaces as a typed `Conflict` the updater retries
  from a fresh base; watchers/indexers must handle that path when they
  arrive.

## Validation / follow-up

- `crates/chakra-engine/tests/atomic_revisions.rs` proves: a held snapshot
  stays immutable after publish; concurrent publishers produce exactly one
  winner; concurrent readers never observe a hybrid or a backwards
  revision.
- Revisit clone-per-update only with benchmark data (roadmap §18).
