# ADR-010: Lazy syntax call-site resolution

Status: accepted
Date: 2026-08-17

## Context

Tree-sitter identifies call expressions but does not determine their runtime
targets. The original v0.1 index expanded every same-name declaration into a
heuristic `CALLS` edge. On Zed this produced 10,140,102 syntax edges from
116,197 symbols, still truncated 38,585 call sites, and made unrelated methods
such as `Project::save_buffer` and `BufferStore::save_buffer` appear to have the
same callers. PHP had the same name-fanout behavior.

Chakra must retain ambiguous syntax evidence without turning it into false
graph topology. The representation must remain bounded, revision-local,
language-aware, incrementally maintainable, and compatible with optional lazy
precise enrichment.

## Decision

- Keep declarations and non-call relationships in the immutable
  `SymbolGraph`. Store each syntax call expression once within the graph in a
  separate compact call-site arena with its caller, form, target domain, name,
  optional qualifier, bounded receiver hint, range, resolution, provenance,
  and precision.
- Index call sites by caller and index ambiguous sites by a lookup key. The
  callable catalog and lookup key include language and one of the distinct
  function, method, or test declaration domains. Rust and PHP names never
  cross-resolve, and a free function never becomes a method candidate solely
  because its text matches.
- When syntax plus workspace evidence proves that one unqualified C++ call
  can honestly denote either a same-type member or an unqualified free
  function, record the explicit `FunctionOrMethod` target domain. Candidate
  lookup unions those two independently indexed declaration domains; a known
  method qualifier narrows only the method half. This does not permit ordinary
  function and method names to cross-resolve.
- Resolve against the complete callable catalog while materializing a private
  graph revision:
  - exactly one syntax candidate creates one heuristic `CALLS` edge;
  - a resolved call owned by a test also creates a heuristic `TESTS` edge,
    deduplicated per test/target pair as refined by ADR-016;
  - multiple candidates record `ambiguous { candidates }` without candidate
    edges;
  - no candidate records `unresolved`;
  - member/nullsafe/scoped syntax without a defensible qualifier remains
    unresolved instead of matching every same-name method.
- A receiver hint is diagnostic syntax evidence, not type identity. It is
  capped at 128 Unicode scalar values and never converts an unknown receiver
  into a global method lookup.
- Expand ambiguous candidates only for the selected symbol in `context`,
  `callers`, and `diff_context`. Expansion uses per-section item/byte budgets
  and typed envelope truncation metadata. Under ADR-024, repeated sites are
  aggregated by caller and candidate target with an exact occurrence count
  and at most three representative evidence records. Candidate facts are
  returned in dedicated `syntax_call_candidates`, `syntax_candidates`, and
  `related_call_candidates` collections with heuristic precision; they are not
  mixed into materialized/precise caller or callee collections.
- Precise provider results for the pinned syntax revision replace matching
  syntax candidates in a response. Unresolved syntax evidence remains visible
  when no precise fact justifies a target.
- Reusable per-file parsed facts retain compact call sites once; relationship
  contributions contain only declaration-dependent non-call edges. A
  declaration-only edit therefore reparses and recomputes its affected file,
  while graph materialization re-resolves unchanged caller contributions
  against the new catalog. There is no callable-name dependency that
  invalidates every same-name caller file.
- Publish the declaration graph, call-site arena, indexes, resolution states,
  and any uniquely resolved edges together in one workspace revision. The
  consistency audit recomputes the callable catalog and every call-site
  resolution before publication.
- Compose only independently resolved, disjoint language graphs. `merge`
  rejects overlapping language domains because adding same-language
  declarations after a call site was resolved could otherwise stale its
  revision-local resolution.
- Expose total, ambiguous, and unresolved call-site counts through `status`
  and initial-index measurements. The old eager-build truncation counter is
  retained temporarily for API compatibility and is zero for the lazy model;
  query-time cuts are reported by the query envelope.

## Alternatives considered

- Keep the eager edge fanout with a higher cap: rejected because it consumes
  memory while preserving false relationships and merely moves truncation.
- Keep only uniquely resolved calls and discard ambiguity: rejected because
  the candidate evidence is useful to agents and to future type-aware PHP/Rust
  refinement.
- Build an eager complete precise graph through rust-analyzer or a PHP language
  server: rejected by SPEC §9, unavailable in degraded/provider-free operation,
  and unnecessary for selected one-hop queries.
- Infer receiver types from variable names: rejected because a textual hint is
  not a reference or type fact.

## Consequences

- Graph size is proportional to declarations, exact relationships, and call
  sites rather than the product of calls and same-name declarations.
- `callers` can distinguish materialized/precise callers from possible syntax
  candidates. ADR-024's schema-v6 aggregation changes the evidence shape;
  clients must continue treating provenance and precision as authoritative.
- Some calls that previously appeared as heuristic edges now appear as
  unresolved evidence until a qualifier or precise provider result is
  available. This is an intentional correctness improvement.
- Genuine C++ member/free collisions remain bounded ambiguous evidence instead
  of being dropped as unresolved merely because neither single declaration
  domain is sufficient.
- ADR-015 refines PHP receiver/type hints without changing graph ownership or
  query contracts.

## Validation / follow-up

- Engine tests cover function/method/test domain separation, unique edge
  materialization, unresolved receivers, bounded hints, and candidate budgets.
- Rust and PHP fixture/MCP tests cover duplicate names and prove unknown
  receivers do not create false callers.
- A synthetic Rust regression stores 256 calls against 256 same-name targets
  as 256 call sites and zero `CALLS` edges instead of 65,536 candidate edges.
- A deterministic live-update regression renames one of two target
  declarations, reparses/recomputes one file, and proves an unchanged caller's
  call site resolves in the next atomic fresh revision.
- Real Zed before/after wall-time and RSS measurements remain tracked by the
  dedicated large-repository benchmark issue.
