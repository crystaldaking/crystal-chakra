# ADR-024: Caller aggregation and byte-first response budgets

Status: accepted
Date: 2026-08-18

## Context

The lazy call-site model removed eager ambiguity edges, but query responses
still returned one entry per call expression. Repeated calls from one caller
could consume every result slot, and a Zed `context(limit=20)` for
`save_buffer` measured about 87 KiB. Item limits alone did not bound long
paths, multibyte source, signatures, or representative evidence.

The MCP adapter also serialized every typed envelope into a non-retaining
counting writer, then `rmcp::Json<T>` serialized the same envelope into the
protocol `Value`. The second pass allocated no response buffer, but still paid
full serialization CPU.

## Decision

- Envelope schema v6 aggregates ordinary `CALLS`, `TESTS`, implementation,
  and precise-provider relations by `(related symbol, relationship kind)`.
  Each entry carries `occurrence_count`, at most three deterministic
  `representative_locations`, and the count of known locations omitted.
- Ambiguous/unresolved syntax evidence is aggregated by caller and candidate
  target. Targetless calls additionally retain their syntax identity so
  unrelated unresolved expressions are never merged. Each entry carries the
  total occurrence count and at most three full representative call-site
  evidence records.
- Precise provider relations carry a total occurrence count and at most three
  call-site ranges across the provider boundary. Multiple provider relations
  resolving to the same revision-scoped symbol are aggregated before they
  replace lower-precision syntax facts.
- Every variable-size public response section has both an item limit and an
  exact compact-JSON byte limit. The main allocations are:

  | Query section | Byte limit |
  | --- | ---: |
  | `repo_map.files`, `search.matches` | 256 KiB each |
  | `symbol_search.candidates` | 128 KiB |
  | `context.source` | 16 KiB |
  | `context.callers`, `context.callees`, syntax candidates | 96 KiB each |
  | `context.implementations`, `context.tests` | 64 KiB each |
  | `context.related_files` | 32 KiB |
  | `callers.callers`, syntax candidates | 128 / 192 KiB |
  | `diff_context` collections | 96–128 KiB each |

  These independent allocations prevent a noisy caller/source section from
  starving declarations, tests, related files, or envelope metadata. A byte
  cut uses ADR-023's `response_byte_limit` cause and reports exact omitted
  encoded bytes when the candidate vector is already available.
- Selected `context.symbol`, `callers.target`, workspace identity, and envelope
  metadata are required fixed overhead rather than optional sections. A final
  1 MiB MCP envelope guard remains authoritative for pathological fixed
  overhead.
- `chakra-mcp` replaces `rmcp::Json<T>` with a schema-preserving wrapper that
  serializes the typed envelope once into the protocol `serde_json::Value`.
  Chakra computes exact compact JSON length by walking that value without a
  second serialization or encoded-response allocation, then returns a ready
  `CallToolResult` with `structured_content` and no duplicate narrative text.
  `rmcp` still owns the unavoidable final protocol transport encoding; it does
  not expose a supported pre-encoded structured-result path.
- Structured trace events separate query construction time/bounded-section
  bytes, typed-envelope serialization time, budget-walk time, final envelope
  bytes, and the fact that transport serialization is `rmcp`-owned. Source
  bodies are never logged.

ADR-025 moves work limits into traversal. A completed section has exact
occurrence counts; a section stopped by an execution-work cause reports counts
for its deterministic examined prefix and does not claim repository-total
completeness.

## Alternatives considered

- Return only the first call site: rejected because agents need the total
  frequency and a small spread of source evidence.
- Group targetless calls only by caller: rejected because unrelated dynamic
  calls would become one misleading relationship.
- Keep only a final 1 MiB rejection: rejected because a noisy early section
  could still waste construction work and crowd useful sections.
- Serialize the final envelope into bytes and inject raw JSON into rmcp:
  rejected because rmcp's supported tool contract accepts structured values,
  not pre-encoded result fragments; bypassing it would couple Chakra to
  protocol framing.
- Estimate bytes from string lengths: rejected because JSON escaping and
  multibyte text make estimates either unsafe or needlessly conservative.

## Consequences

- Schema-v5 clients must accept schema v6 before reading relation/call-site
  evidence. Symbol ids, provenance, precision, freshness, and typed truncation
  retain their meanings.
- One caller or test consumes one result slot per relationship target while
  preserving total occurrences and bounded evidence.
- The largest configured query section combinations remain comfortably below
  the transport guard in ordinary responses. Pathological fixed overhead is
  rejected rather than partially emitted.
- Per-item JSON sizing adds bounded CPU to response construction; it replaces a
  complete duplicate-envelope serialization and is observable separately.

## Validation / follow-up

- Engine regressions cover repeated exact calls/tests, ambiguous candidates,
  provider aggregation, long multibyte paths, and multibyte source snippets.
- MCP regressions prove exact size accounting matches serde_json for control
  characters and multibyte values, and that the typed payload is serialized
  once at the budget boundary.
- MCP end-to-end fixtures validate the schema-v6 evidence shape.
- Release Zed measurements compare `save_buffer` context size with the prior
  approximately 87 KiB observation.
- ADR-025 covers traversal-time examined/allocation/wall-time budgets;
  ADR-012 covers cooperative cancellation and deadline propagation.
