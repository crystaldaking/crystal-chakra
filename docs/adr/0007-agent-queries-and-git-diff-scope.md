# ADR-007: Agent queries and Git diff scope

Status: accepted
Date: 2026-08-15
Last reviewed: 2026-08-17

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
  `symbol_search` plus a revision-scoped entity id. Under ADR-010,
  Tree-sitter ambiguity is returned in dedicated bounded call-site candidate
  collections with heuristic precision rather than as one edge per target.
  Lazy rust-analyzer Call Hierarchy facts replace
  matching candidates only when they are ready for the exact syntax revision.
- Add `chakra-git` as an outward adapter over a small Chakra-native
  `WorkspaceDiffProvider` contract. The engine and domain crates contain no Git
  command/output types, and the MCP adapter only delegates typed queries.
- Git commands use fixed structured arguments and stream their pipes. The
  adapter drains stdout while retaining at most 8 MiB and captures only a
  bounded stderr prefix, so an oversized diff becomes a typed unavailable
  result without unbounded output allocation. Each child has a 30-second
  process deadline. Change-record parsing stops after enough records to prove
  the 10,000-file work-inventory cap instead of processing the retained output
  only to truncate its response.
  ADR-012 shortens that process deadline to the query's remaining end-to-end
  deadline when necessary; cancellation kills and waits for the child and
  joins both pipe readers before returning.
- Keep the v0.1 `worktree` scope as the backward-compatible default and add two
  typed feature-review scopes. Every scope compares an immutable commit
  baseline with the final materialized worktree:
  - `worktree`: resolve `HEAD` as the baseline; an unborn repository has no
    baseline and reports every indexed document as added;
  - `base_ref { reference }`: resolve the named commit-ish directly, matching
    two-dot-style review semantics;
  - `merge_base { reference }`: resolve the unique merge-base of the named
    commit-ish and `HEAD`, matching three-dot-style review semantics.
- Explicit base scopes include committed divergence plus the final index and
filesystem overlay. A clean feature branch therefore still reports its
committed changes relative to the selected base. For all three scopes:
  - tracked staged and unstaged edits are combined; final worktree content wins;
  - materialized changes hidden by `assume-unchanged` or `skip-worktree` index
    bits are still compared with the baseline blob;
  - an index-only edit that is undone in the worktree is absent;
  - a staged index removal whose non-ignored file remains materialized is
    compared with the baseline blob: unchanged content is absent and changed
    content is modified;
  - untracked, non-ignored Rust/PHP files are added;
  - ignored files, `target/`, unsupported-language files, and skipped symlinks are excluded;
  - in an unborn repository, every indexed Rust/PHP document is added;
  - deleted tracked Rust/PHP files are returned by their former path;
  - Git-detected staged renames use the current path plus `previous_path` and
    heuristic precision because rename detection is similarity-based;
  - an unstaged filesystem move is returned as a deletion plus an untracked
    addition when Git has not recorded enough evidence to call it a rename;
  - Git copy detection is not enabled; a copied current file is an addition.
  - unrelated non-UTF-8 non-Rust paths are ignored before path decoding;
  - a `.rs` symlink deleted from the baseline is excluded because it was never an
    indexed regular source, and a deleted path that reappears during the Git
    read makes the join retry rather than returning mixed state.
- Resolve user-provided refs with fixed-argument Git plumbing and
  `--end-of-options`. Empty, over-budget, invalid, and ambiguous refs fail
  explicitly. A merge-base scope also fails for an unborn `HEAD`, unrelated
  histories, or multiple merge bases; Chakra never guesses one candidate.
  The adapter uses only resolved object ids for the diff and returns the exact
  `base_commit` in the public result.
- `diff_context` first obtains a syntax freshness barrier, pins one immutable
  revision, reads Git state, then runs the barrier again. A revision change
  retries the whole operation instead of returning a mixed graph/worktree view.
  The Git adapter also compares materialized changed sources with the captured
  snapshot source before returning them. It resolves `HEAD`/the requested ref
  before reading the diff and verifies that those refs still resolve to the
  same commits afterward. The tracked diff, staged index entries/flags, and
  untracked inventory are captured before classification and compared with a
  second bounded read afterward; any inventory or ref movement causes a retry
  instead of a mixed Git view.
- The v0.1 changed-symbol slice is file-level: current declarations in returned
  changed files are selected with basis `declared_in_changed_file`. The
  selection carries heuristic provenance/precision; the nested symbol keeps its
  own Tree-sitter quality. This does not claim that every selected declaration
  overlaps a changed hunk. Deleted symbols are absent because the current graph
  intentionally contains no historical declaration nodes.
- Apply the request limit to every returned collection, cap it at the shared
  query maximum, and set the envelope `truncated` flag whenever any section,
  provider result, diff inventory, source snippet, or syntax call-candidate set
  is cut. Under ADR-014, every cut is attached to its response section and
  typed cause; workspace-global call-site state never sets a query envelope's
  truncation flag. `diff_context`
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
- Compare only commit trees for explicit base scopes: rejected because it
  would hide the staged, unstaged, and untracked edits an agent just made. All
  scopes deliberately target the effective materialized worktree.

## Consequences

- One high-level query returns a compact current chain from files to symbols to
  callers/tests without a repository-sized graph dump.
- Git change classification remains useful without rust-analyzer, and precise
  provider failure does not disable syntax or diff results.
- Staged renames are explicit; unstaged moves remain an honest delete/add pair.
- `base_ref` exposes direct-base divergence while `merge_base` isolates changes
  since the branch point; clients select the review meaning rather than Chakra
  guessing from branch topology.
- Future hunk-to-symbol mapping can add a stronger basis without changing the
  meaning of the v0.1 file-level heuristic.

## Validation / follow-up

- Real temporary repositories cover staged, unstaged, untracked, ignored,
  canceled, rename, delete, unborn-HEAD, hidden index bits, unrelated non-UTF-8
  paths, skipped symlink deletion, direct-base and merge-base divergence,
  invalid/ambiguous refs, and snapshot/ref-race behavior.
- Rust and PHP fixtures exercise structured `context`, ambiguity, callers,
  `diff_context`, budgets, explicit symbol language, and provenance through
  an in-process MCP client.
- Fixture measurements record initial indexing, `symbol_search`, `context`, and
  `diff_context` latency; `docs/evaluation/v0.1-readiness.md` records the
  current local values and reproduction commands.
