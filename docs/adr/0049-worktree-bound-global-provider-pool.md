# ADR-0049: Worktree-bound instances in a global provider pool

Status: accepted
Date: 2026-09-02

## Context

ADR-0035 bounded precise-provider resources inside one workspace. ADR-0048
then introduced several independent workspace engines in one process and
deferred shared provider orchestration to issue #47. Reusing one active
language-server instance across engines would mix document synchronization
and materialized filesystem state; starting an independent unbounded pool per
engine would multiply heavyweight processes and defeat the process limit.

## Decision

- One `ProviderPool` owns process-global provider-count, deterministic-memory,
  concurrent-query, and queued-query limits for every registered worktree.
- Provider registrations are process-global templates. `providers_for` creates
  lazy slots bound to one complete `WorkspaceIdentity`; a wrapper rejects a
  `ProviderWorkspace` whose repository root differs from that binding before
  admission or activation.
- Active-provider and memory limits also apply independently to each
  worktree. When a local limit is exhausted, reclamation selects only an
  inactive LRU provider from that worktree. A global shortage may select the
  oldest inactive provider from any worktree. In-flight or activating
  providers are never resource- or idle-evicted.
- Provider-owned document synchronization, crash recovery, revision state,
  and child processes remain instance-local. Command discovery may be shared
  only when its result is materialization-independent. In particular, jdtls
  command construction remains per worktree because its mandatory `-data`
  directory is root-derived.
- Global counters and the selected worktree's resource envelope are reported
  once through `ProviderOrchestrationMetrics`. This additive public shape
  advances the query envelope schema from 15 to 16.
- `chakra serve` accepts repeated `--repo` paths. The workspace registry first
  proves that they are linked worktrees of one repository; each returned
  engine then receives only the wrappers bound to its own identity.

## Alternatives considered

- **One provider process per language shared across worktrees.** Rejected:
  mainstream language servers bind open documents, project discovery, and
  caches to one materialized root, so reuse would leak precise facts.
- **One independent provider pool per engine.** Rejected because global
  process and memory bounds would be unenforceable.
- **Bind a lazy wrapper on its first query.** Rejected because concurrent first
  queries could make ownership order-dependent and an accidentally reused
  wrapper could silently attach to the wrong worktree.

## Consequences

- Cold worktrees consume lightweight slots but no language-server process.
  Warm instances can be reclaimed globally while preserving strict document
  isolation.
- A local quota can cause syntax fallback even when unused global capacity
  remains; status identifies both envelopes so that degradation is
  diagnosable.
- ADR-0050 composes snapshots and overlays separately from this pool. Provider
  enrichment remains materialization-dependent and is never stored as an
  intrinsic commit fact.

## Validation / follow-up

- Hermetic tests reject cross-worktree requests before activation, exercise
  provider-count and memory limits independently with local-only LRU
  eviction, and run simultaneous Rust/Python queries in two worktrees through
  one global admission/resource state.
- CLI tests pin repeated `--repo` parsing and the independent global/local
  limits. Real linked-worktree routing remains covered by ADR-0048 tests.
