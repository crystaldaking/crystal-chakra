# ADR-0044: Atomically published project model and dependency-scoped invalidation

Status: accepted
Date: 2026-08-30

## Context

Per-file source metadata identifies a package, but it cannot represent
workspace membership, package dependencies, ambiguous ownership, or the
external inputs that caused derived facts to change. Chakra needs explicit
Cargo and Composer project scopes for queries and must react to manifest or
configuration changes without turning an ordinary metadata edit into a
language-wide rebuild.

The model and its invalidation evidence must remain Chakra-owned, bounded,
Git-aware, and revision-exact. Cargo/Composer protocol values must not cross
into the domain or query layers, and a fresh query must never combine a graph
from one revision with a project model from another.

## Decision

- `chakra-domain` owns a bounded `ProjectModel` containing typed Cargo,
  Composer, and path-fallback units; workspace membership; source roots and
  roles; dependency edges; and manifest issues. The Git adapter converts
  bounded offline Cargo metadata and Git-visible Composer manifests into
  these types. It records failed, malformed, and deliberately bounded-out
  probes distinctly instead of guessing or silently omitting evidence.
- A project unit id is derived from kind, repository-relative root, and name.
  Delimiter characters in components are percent-escaped (including `%`
  itself), preserving the readable common form while preventing component
  collisions.
- Ownership is structural and language-aware. The deepest claiming source
  root wins; equal-depth claims are ambiguous and match no unit selector.
  Sources no retained ecosystem unit claims use deterministic path-fallback
  units.
- The project model is stored in the immutable `WorkspaceSnapshot` and is
  published by the same compare-and-swap update as the graph, indexing
  status, provider inputs, freshness, and revision. Construction and
  reconciliation remain private until that atomic publication succeeds.
- `ProjectModel::impact_since` reports a bounded typed diff for unit
  additions/removals, definition/source-root/dependency/membership changes,
  affected dependents, and manifest-issue transitions. A retained file whose
  source metadata changes has only its graph file record replaced; its
  syntax-derived symbols, edges, call sites, and entity ids are preserved.
  Framework facts whose configuration changed are re-derived only in the
  affected language adapter.
- `repo_map`, `symbol_search`, `context`, `callers`, and `diff_context` accept
  typed project selectors. Related-item filters are applied during bounded
  traversal, before the response item limit, so out-of-scope prefixes cannot
  hide later in-scope results. The anchor of `context` or `callers` is not
  filtered. Unknown selectors are typed invalid requests.
- The additive project-query contract advances the envelope schema from 14
  to 15. No project model or derived graph snapshot is persisted in this
  release.

## Alternatives considered

- **Keep package data only on files.** Rejected because dependencies,
  workspaces, ambiguity, and metadata-only revisions cannot be represented
  honestly.
- **Expose Cargo metadata or Composer JSON directly.** Rejected because it
  leaks ecosystem protocol types into stable domain and MCP contracts.
- **Rebuild an entire language graph after any manifest change.** Rejected
  because source syntax facts remain a function of content and extractor
  version; metadata-only changes should not reparse unrelated files.
- **Choose one owner for ambiguous paths.** Rejected because ordering would
  fabricate precision.
- **Publish the model separately from the graph.** Rejected because a fresh
  query could observe a mixed revision and violate read-your-writes.
- **Persist the model or add a syntax-fact cache now.** Rejected pending the
  v0.2.0 persistence and cache acceptance evidence; atomically rebuilt
  in-memory state is sufficient for this decision.

## Consequences

- Fresh queries can scope and summarize Cargo/Composer units while other
  ecosystems degrade to explicit path-fallback units.
- Manifest edits may publish a new revision and update file metadata or
  framework relationships without reparsing source content. Typed metrics
  expose exactly which work occurred and why a conservative full scan was
  selected.
- Cargo probing requires the local Cargo binary and shares one bounded
  command deadline; Composer reads are count- and byte-bounded. Reaching a
  bound reduces coverage but leaves observable `probe_omitted` evidence.
- Project unit ids are stable for unchanged manifest evidence, but package
  moves intentionally appear as a removed unit plus an added unit.

## Validation / follow-up

- Domain tests cover ownership, ambiguity, selector validation, collision-free
  ids, dependency impact, package moves, source-root changes, membership, and
  manifest-issue transitions.
- Git adapter tests cover nested Cargo workspaces and path dependencies,
  Composer PSR-4 roles, malformed manifests, ambiguous ownership,
  metadata-only changes, and observable manifest probe bounds.
- Engine/MCP tests cover atomic project publication, wire schemas, scoped
  queries, unknown selectors, and filtering before related-item limits.
- Live invalidation tests compare incremental graph fingerprints with cold
  rebuilds and assert that ordinary one-file edits and metadata-only edits do
  not escalate to repository-wide reparsing.
