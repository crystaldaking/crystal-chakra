# ADR-001: Workspace revision publication model

Status: accepted
Date: 2026-08-15
Last reviewed: 2026-08-18

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
  publications share the immutable graph allocation. Syntax graph payloads use
  MIT-licensed `rpds` persistent maps plus `Arc` file/symbol/adjacency chunks,
  so a private one-file update clones only persistent-tree paths and the
  affected contributions. Unchanged payload objects are physically shared with
  readers of the previous snapshot. Rust/PHP workspace composition is a shallow
  immutable partition list rather than a cloned/remapped combined arena.
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
- Persistent/immutable data structures before update measurements: initially
  deferred. The measured implementation subsequently rebuilt and copied the
  complete changed-language and combined graphs for every edit, so structural
  publication was adopted for v0.1.1. `rpds` was selected over `imbl` because
  it is maintained, supports thread-safe structural sharing, and is MIT rather
  than MPL-2.0 licensed.
- Actor/owning task with message passing: no concrete benefit at this
  scale; rejected by SPEC §35's "no actor framework without cause".

## Consequences

- Publication uses `arc-swap` for the atomic snapshot pointer and `rpds` for
  persistent graph maps. `rpds` adds `archery`/`triomphe`; these are accepted
  for thread-safe structural sharing and pass the repository license/source
  policy.
- Metadata-only lifecycle/freshness updates do not clone the graph. Syntax
  updates retain private construction and atomic pointer publication while
  sharing all unchanged graph payloads. The public query/domain API does not
  expose `rpds` types.
- Query envelope schema v4 copies the pinned snapshot's `IndexingStatus`,
  including v2 coverage/degradation, v3 structural-publication metrics, and v4
  worker/CPU/RSS scheduling measurements. Every query distinguishes
  complete/degraded revisions, copied versus reused update work, and the
  effective resource policy used to construct that revision.
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
- `IndexingStatus::publication` and live counters report reused, rebuilt, and
  copied files/source bytes/symbols/edges/call sites for each graph revision.
  Pointer-identity regressions prove unchanged payloads are actually shared;
  the counters are not a constant-valued surrogate.
