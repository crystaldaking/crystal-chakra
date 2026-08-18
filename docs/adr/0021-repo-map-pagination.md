# ADR-021: Revision-scoped repository-map pagination and structural overview

Status: accepted
Date: 2026-08-18

## Context

The original `repo_map` returned one alphabetically sorted prefix. Its hard
500-result cap made most of the 1,158 PHP files in `psp-app` and most of the
Rust files in Zed unreachable. The first prefix also communicated little about
the repository's package/module structure. Raising the cap would conflict with
the MCP response budget and still provide no atomic traversal when live edits
publish a new graph revision.

## Decision

- Keep file ordering lexicographic by canonical repository-relative path and
  add a self-contained continuation cursor. A cursor contains the workspace
  id, published revision, last returned path and normalized language/source
  scope. Continuation requests provide the cursor and may change only the page
  limit; conflicting filters are rejected.
- Reject a cursor when either its workspace or revision differs from the
  pinned query snapshot. Chakra never continues an old ordering over a newer
  rename/delete/edit revision. A zero-sized page is invalid so a cursor can
  always make progress.
- Return `next_cursor` only when another eligible file exists. Each file view
  carries language, Git provenance/precision, symbol count, source role,
  classification and optional package identity. Language and source filters
  are applied before pagination, so every eligible Rust/PHP file remains
  reachable through bounded requests.
- Add a first-page-only structural `overview`, bounded by the same clamped
  limit. Groups overlap intentionally: every file contributes to a top-level
  directory/language group and, when metadata exists, to a Cargo-package or
  Composer-PSR-4 group. Groups sort by descending file count and then stable
  kind/language/root/name tie-breakers. Continuation pages do not repeat them.
- Discover Git-visible `composer.json` files inside `chakra-git`, read at most
  64 manifests of at most 1 MiB each, and parse `autoload.psr-4` and
  `autoload-dev.psr-4` without invoking Composer. The longest matching root
  supplies Composer package/root metadata; dev roots default to the test role,
  while more specific fixture/generated/vendor path roles remain visible.
- Store PHP metadata alongside reusable per-file syntax facts, as Rust already
  does for Cargo. A Composer-only change materializes a complete new graph and
  revision without reparsing unchanged PHP source.

## Alternatives considered

- Increase the single response cap: rejected because it does not guarantee
  reachability, worsens response size, and still returns an arbitrary prefix.
- Use a numeric offset: rejected because it has no revision/workspace binding
  and can silently skip or duplicate files after a publication.
- Keep cursor state inside the MCP server: rejected because the domain query
  contract should remain transport-independent and deterministic without a
  mutable per-client session cache.
- Read Composer metadata during each query: rejected because it could mix
  filesystem state with a pinned older graph revision.
- Execute Composer for package discovery: rejected because indexing must not
  require a PHP runtime, mutate the worktree, or depend on external resolution.

## Consequences

- Clients can traverse every file while each response remains bounded. The
  cursor is deliberately invalidated by any published revision, even when an
  unrelated file changed; this favors one coherent snapshot over best-effort
  continuation.
- A first page performs a bounded linear aggregation over eligible file
  summaries. ADR-025 enforces examined/allocation/wall-time work budgets, while
  ADR-012 provides cooperative cancellation during ordered inventory
  traversal.
- Composer coverage is explicit alongside Cargo and fallback counts. Invalid,
  oversized, ignored or over-budget manifests degrade to path fallback rather
  than preventing PHP syntax indexing.
- No new dependency is required: `chakra-git` already used workspace-managed
  `serde_json` for Cargo metadata.

## Validation / follow-up

- An engine regression builds 1,005 Composer-classified PHP files and 1,005
  Cargo-classified Rust files, traverses each filtered set over multiple pages,
  checks deterministic ordering/no duplicates, structural groups, conflicting
  scopes, workspace mismatch and stale revision rejection.
- Live tests rename and delete files, immediately request fresh syntax state,
  and prove the previous cursor is rejected without sleeps. Separate Cargo and
  Composer manifest tests prove metadata-only publications perform zero source
  reparses.
- The MCP contract performs two cursor pages, verifies the first-page-only
  overview and checks the serialized page against the 1 MiB transport budget.
  In the current debug test run, its first page was 1,419 bytes and the two
  in-process MCP calls completed in 0.93 ms; traversing both synthetic
  1,005-file language sets took 41 ms. These are direct measurements, not
  release-build latency guarantees.
- A read-only real MCP run on `psp-app` indexed 1,158 PHP files: 1,042 received
  Composer metadata, the first overview exposed `Modules`, `app`, `psp/app`,
  `nwidart/payment` and `tests`, and the returned cursor produced the next
  non-overlapping page with no repeated overview.
- A shallow Zed run on this isolated branch remained in initial syntax/
  relationship indexing for five minutes before it was cancelled, so it did
  not provide a `repo_map` latency sample. That is startup evidence for issues
  #11/#15, not a pagination failure; large Rust reachability is covered here by
  the deterministic 1,005-file regression and must be repeated after the
  indexing-performance branches are integrated.
