# ADR-020: Bounded deterministic symbol search ranking

Status: accepted
Date: 2026-08-18

## Context

Real PSP and Zed evaluations showed that arena-order substring search allowed
imports, impl blocks, fixtures and generated symbols to consume the response
limit before the primary declaration. Sorting only the already-truncated
prefix could not recover a better symbol discovered later. Agents also needed
typed ways to narrow mixed Rust/PHP and monorepository results without hiding
those facts during indexing.

## Decision

- Keep `symbol_search` case-insensitive and assign every matching symbol a
  deterministic relevance tuple, in this order:
  1. exact simple or qualified name;
  2. simple-name prefix;
  3. qualified-name prefix;
  4. simple-name substring;
  5. qualified-name substring;
  6. ordinary declaration before impl block before import;
  7. production before test, example, bench, fixture, generated and vendor;
  8. language, qualified name, path, source range, kind and revision-local id
     as deterministic tie-breakers.
- Use a bounded binary-heap top-k selection. The scan retains at most the
  caller's clamped result limit, replaces the current worst result when a
  better later candidate appears, and marks the envelope truncated after the
  first additional eligible match. It never allocates one response object per
  graph symbol.
- Add bounded `include_languages`, `include_kinds`, `exclude_kinds` and
  case-sensitive segment-aware `namespace_prefix` request filters. Reuse the
  language-neutral source filter from ADR-010 for package, path and source
  role. Empty include lists preserve all indexed candidates; include filters
  are applied first and exclude filters afterward.
- Filtering occurs before ranking and the response limit. Imports and noisy
  source roles remain indexed and are reachable through explicit include
  filters. Ranking does not resolve ambiguity: entity-based follow-up still
  requires a revision-scoped id, while an ambiguous human-readable name
  remains a typed error.
- Do not introduce a fuzzy-search dependency. The graph scan is deterministic
  and allocation-bounded; ADR-025 also enforces an examined-symbol work budget
  without changing ranking semantics. Exact-name lookup has its own persistent
  index for ambiguity resolution.

## Alternatives considered

- Sort only the first `limit` arena matches: rejected because early noise can
  permanently hide a better declaration.
- Collect and sort every matching symbol: rejected because response limits
  must also bound intermediate memory on large repositories.
- Remove imports, fixtures or generated sources from the graph: rejected
  because filtering and ranking must not erase repository facts.
- Guess one symbol when names collide: rejected by SPEC §24; ambiguity is a
  property clients must resolve explicitly.
- Add fuzzy matching now: deferred until evaluation proves substring ranking
  insufficient and supplies a measurable quality target.

## Consequences

- Default searches become useful without requiring callers to know every
  filter, while explicit filters remain available for precise agent workflows.
- Search CPU is linear only in ADR-025's bounded examined-symbol prefix;
  retained memory is O(limit) and the limit remains capped at 500.
- Source-role relevance depends on revision-scoped metadata from ADR-010; PHP
  and non-Cargo Rust use the same path-fallback roles without Cargo coupling.
- Stable content produces stable ordering, but entity ids remain valid only in
  the response revision even when their numeric value happens to repeat.

## Validation / follow-up

- Mixed Rust/PHP engine tests insert imports and fixtures before declarations,
  assert a limit-one query still returns the primary production declaration,
  exercise every filter and preserve explicit ambiguity.
- MCP contract coverage sends the new typed filters and verifies structured
  source-role output.
- Qualified-name ordering is compared across two revisions built from
  unchanged content.
- A read-only MCP acceptance run against `psp-app` returned the production
  `TransactionStatusService` class at rank 1, its test class at rank 2, and
  import symbols from rank 3 onward under the default relevance policy.
