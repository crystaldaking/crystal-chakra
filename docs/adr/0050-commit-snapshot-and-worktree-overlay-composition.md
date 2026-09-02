# ADR-0050: Commit snapshot and worktree overlay composition

Status: accepted
Date: 2026-09-02

## Context

The v0.3.0 milestone requires the SPEC model
`EffectiveWorkspaceGraph = CommitSnapshot + WorktreeOverlay +
WorkspaceEnrichment`. The existing engine atomically published the final
materialized graph, but it did not retain which syntax facts belonged to the
base commit, which files were supplied by the checkout, or whether a precise
provider result was reusable outside that checkout.

Treating the checked-out filesystem as the commit snapshot would make dirty,
deleted, renamed, and untracked files part of supposed commit truth. Running a
language provider while building the commit layer would create the same leak
for precise facts.

## Decision

- `chakra-git` resolves `HEAD` to an immutable commit object, inventories its
  regular supported-language blobs with `git ls-tree`, and reads retained
  contents through one bounded `git cat-file --batch` child. An unborn
  repository has an explicit empty commit layer. No checkout, temporary
  worktree, provider, or hardcoded Git administrative path is involved.
- Commit scanning uses only the captured blob inventory, source text, path
  classification, Tree-sitter adapters, and deterministic syntax resolution.
  Provider inputs and materialized project probes are absent. In particular,
  framework evidence is passed into language adapters from the captured
  project model rather than read from the mutable repository root.
- Startup cold-builds the commit syntax index, then incrementally reconciles
  the final Git-visible worktree over it. The effective index owns the current
  file graph; the immutable commit graph is retained separately for future
  compatibility-keyed reuse in issue #49.
- The existing bounded Git diff adapter supplies deterministic added,
  modified, deleted, and rename records. Exact per-file source-layer ownership
  is derived by comparing the effective source with the retained commit source,
  so a truncated public change inventory cannot mislabel a returned fact.
  The envelope reports `files_truncated` separately from the optional exact
  omitted count, including when the Git adapter cannot calculate that count.
- `WorkspaceSnapshot` atomically publishes the commit graph, effective graph,
  commit id/counts, worktree delta, indexing status, and project/provider
  inputs. Live reconciliation verifies the same Git diff twice around private
  construction and publication; a changed `HEAD` rebuilds only the immutable
  commit layer, while ordinary worktree changes retain it.
- Every query envelope reports the three layer descriptors. Precise enrichment
  has a revision only when the worktree-bound provider is ready for the exact
  observed syntax revision. File, text, symbol, and source-snippet results also
  carry `source_layer: commit_snapshot | worktree_overlay`. This additive
  contract advances the query envelope schema from 16 to 17.

## Alternatives considered

- **Label the existing filesystem graph after indexing.** Rejected because the
  alleged commit layer would already contain dirty and untracked syntax facts.
- **Create a temporary detached worktree for every commit.** Rejected because
  lifecycle and filesystem materialization are unnecessary for offline facts
  and would blur the provider/materialization boundary.
- **Store provider enrichment in the commit graph when `HEAD` is clean.**
  Rejected because provider state depends on the live root, toolchain,
  configuration, open documents, and synchronization revision.
- **Use only the bounded changed-file response to label current facts.**
  Rejected because paths beyond that response bound would be falsely labeled
  as commit-owned.

## Consequences

- Dirty, clean, detached, rebased, and unborn worktrees share one composition
  model. Rename and delete evidence remains explicit even though deleted files
  have no current syntax nodes.
- Initial startup now performs one commit cold build plus a structurally
  incremental worktree reconciliation. Identical commits are still rebuilt per
  worktree until issue #49 adds compatibility-keyed reuse.
- Commit project metadata currently degrades to deterministic path
  classification where parsing would require materialized ecosystem tooling.
  The effective worktree layer still publishes the full current project model.
- No production dependency was added.

## Validation / follow-up

- Git-object tests prove dirty, deleted, and untracked filesystem state cannot
  affect an immutable commit capture, explicit old commits survive `HEAD`
  movement, and unborn repositories produce an empty base.
- Linked-worktree integration covers modified, added, deleted, renamed,
  unchanged, detached, committed/rebased, and provider-isolation behavior.
  Query contract tests pin schema 17 and per-result source layers.
- Issue #49 may persist/reuse only the commit graph after adding the complete
  compatibility fingerprint, corruption handling, atomic publication, and
  bounded eviction required by that issue.
