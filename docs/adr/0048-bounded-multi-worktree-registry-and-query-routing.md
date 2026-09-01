# ADR-0048: Bounded multi-worktree registry and query routing

Status: accepted
Date: 2026-09-02

## Context

Chakra's repository and worktree identities already distinguish one Git
object database from each materialized checkout, but the runtime previously
owned exactly one `WorkspaceEngine` and one live watcher. Milestone v0.3.0
issue #46 requires concurrent worktree indexing without allowing uncommitted
syntax facts, provider facts, or publication state to leak between worktrees.

The domain query contract is deliberately workspace-local. Turning it into a
multi-workspace graph would weaken the invariant that every query observes one
atomically published workspace revision. Selecting a default worktree from
registration order would also make an omitted selector nondeterministic.

## Decision

- `chakra-workspace` is the application orchestration boundary. One
  `WorkspaceRegistry` is permanently scoped to one stable `RepositoryId` and
  admits at most its configured number of worktrees. A `Starting` reservation
  counts against that bound before indexing begins, while expensive indexing
  and watcher construction run without holding the registry lock.
- Every ready worktree owns a separate `WorkspaceEngine`, `LiveIndex`, Git
  diff adapter, revision sequence, and publication state. The registry derives
  repository/worktree identity only through `chakra-git`; it never assumes
  that `.git` is a directory.
- `QueryService` remains a single-workspace domain contract. The separate,
  transport-neutral `WorkspaceQueryRouter` resolves an optional
  `WorkspaceId` to exactly one service before a query begins. An omitted id is
  accepted only when exactly one worktree is registered; otherwise routing
  returns a typed no-workspace or selection-required error.
- MCP exposes a read-only `workspaces` discovery tool and adds an optional,
  flat `workspace_id` field to existing requests. Existing one-worktree
  clients retain their request shape and behavior.
- Unregistration moves the entry to a reserved `Stopping` state, synchronously
  stops and joins the owned live index, then atomically publishes the retained
  engine as stale before releasing the identity. The registry never detaches
  watcher tasks. Failed registration rolls back its reservation.
- Precise provider instances are still installed on one workspace engine and
  cannot contribute to another. A globally bounded provider pool across all
  registered worktrees is follow-up issue #47. Commit snapshots and overlay
  sharing are follow-up issues #48 and #49; this decision does not introduce
  either early.

## Alternatives considered

- **One engine containing several worktrees.** Rejected because revision,
  freshness, provider synchronization, and worktree-local Git diff state could
  be partially published or confused across checkouts.
- **One independent unbounded runtime per requested path.** Rejected because
  watcher and index work would grow without a global admission limit.
- **Implicitly route to the first or primary worktree.** Rejected because
  clients could silently receive facts from the wrong uncommitted checkout.
- **Put workspace selection into every domain request type.** Rejected because
  it would mix orchestration with workspace-local query semantics and duplicate
  selection logic across all tools.

## Consequences

- Syntax facts, revision counters, freshness barriers, Git diffs, and precise
  provider installations are isolated by construction at the engine boundary.
- Callers that register multiple worktrees must first discover identities and
  select one explicitly. Single-worktree callers remain compatible.
- The initial registry rebuilds syntax state independently for each worktree.
  Snapshot reuse is intentionally deferred until the commit/overlay model is
  implemented and benchmarked.
- The registry owns worktree lifecycle but does not yet make the CLI accept
  several paths in one invocation; the public routing contract and registry
  are ready for the provider-global orchestration added by issue #47.

## Validation / follow-up

- Real linked-worktree tests verify stable shared repository identity,
  distinct workspace identity, independent edits and revisions, explicit
  routing, provider-fact isolation, bounded admission, cross-repository
  rejection, and stale publication after unregister.
- MCP contract tests verify the additive tool/request surface and explicit
  selection failure with multiple worktrees.
- Issue #47 must make provider admission global across registered worktrees
  before the CLI exposes multi-worktree startup as a supported beta workflow.
