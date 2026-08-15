# ADR-007: Agent queries and Git diff scope

Status: accepted
Date: 2026-08-15

## Context

Roadmap §§3–9 and 17–19 require bounded `context`, `callers`, and
`diff_context` queries that preserve revision, freshness, provenance, and
precision. SPEC §§23–29 makes `context` and `diff_context` primary product
features, while SPEC §9 rejects an eagerly constructed precise call graph.

Git state is canonical input, but Git process/output types must not enter the
domain or MCP contracts. Joining a live Git read with an older syntax snapshot
would also violate atomic revision semantics.

## Decision

- Keep `context` and `callers` as one-hop, entity-based graph queries. A name
  resolves only when unique; ambiguous names return a typed error and require
  `symbol_search` plus a revision-scoped entity id. Tree-sitter call candidates
  retain heuristic precision. Lazy rust-analyzer Call Hierarchy facts replace
  matching candidates only when they are ready for the exact syntax revision.
- Add `chakra-git` as an outward adapter over a small Chakra-native
  `WorkspaceDiffProvider` contract. The engine and domain crates contain no Git
  command/output types, and the MCP adapter only delegates typed queries.
- Define the v0.1 default diff scope as `HEAD` to the final materialized
  worktree for indexed regular Rust files:
  - tracked staged and unstaged edits are combined; final worktree content wins;
  - an index-only edit that is undone in the worktree is absent;
  - a staged index removal whose non-ignored file remains materialized is
    compared with the HEAD blob: unchanged content is absent and changed
    content is modified;
  - untracked, non-ignored Rust files are added;
  - ignored files, `target/`, non-Rust files, and skipped symlinks are excluded;
  - in an unborn repository, every indexed Rust document is added;
  - deleted tracked Rust files are returned by their former path;
  - Git-detected staged renames use the current path plus `previous_path` and
    heuristic precision because rename detection is similarity-based;
  - an unstaged filesystem move is returned as a deletion plus an untracked
    addition when Git has not recorded enough evidence to call it a rename;
  - Git copy detection is not enabled; a copied current file is an addition.
- `diff_context` first obtains a syntax freshness barrier, pins one immutable
  revision, reads Git state, then runs the barrier again. A revision change
  retries the whole operation instead of returning a mixed graph/worktree view.
  The Git adapter also compares materialized changed sources with the captured
  snapshot source before returning them.
- The v0.1 changed-symbol slice is file-level: current declarations in returned
  changed files are selected with basis `declared_in_changed_file`. The
  selection carries heuristic provenance/precision; the nested symbol keeps its
  own Tree-sitter quality. This does not claim that every selected declaration
  overlaps a changed hunk. Deleted symbols are absent because the current graph
  intentionally contains no historical declaration nodes.
- Apply the request limit to every returned collection, cap it at the shared
  query maximum, and set the envelope `truncated` flag whenever any section,
  provider result, diff inventory, or source snippet is cut. `diff_context`
  relationships are one hop from the returned changed-symbol slice, and each
  carries the exact revision-scoped changed symbol id that explains its
  inclusion.

## Alternatives considered

- Run Git directly in the query or MCP crate: rejected because it leaks adapter
  behavior into the application or transport layer and makes contract testing
  harder.
- Add a full Git library: rejected for v0.1. Fixed-argument Git commands already
  implement repository/worktree semantics, avoid administrative-layout
  assumptions, and add no external dependency.
- Label every declaration in a modified file as precisely changed: rejected
  because file membership does not prove hunk overlap.
- Fetch precise callers for every changed declaration: rejected because this is
  an eager multi-symbol provider crawl. A user can select the important symbol
  from `diff_context` and request `context` or `callers` lazily.
- Parse historical deleted declarations into the current graph: rejected as a
  premature historical overlay and identity expansion.

## Consequences

- One high-level query returns a compact current chain from files to symbols to
  callers/tests without a repository-sized graph dump.
- Git change classification remains useful without rust-analyzer, and precise
  provider failure does not disable syntax or diff results.
- Staged renames are explicit; unstaged moves remain an honest delete/add pair.
- Future hunk-to-symbol mapping can add a stronger basis without changing the
  meaning of the v0.1 file-level heuristic.

## Validation / follow-up

- Real temporary repositories cover staged, unstaged, untracked, ignored,
  canceled, rename, delete, unborn-HEAD, and snapshot-race behavior.
- The Rust fixture exercises structured `context`, ambiguity, callers,
  `diff_context`, budgets, and provenance through an in-process MCP client.
- Fixture measurements record initial indexing, `symbol_search`, `context`, and
  `diff_context` latency.
