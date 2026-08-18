# ADR-023: Query truncation contract

Status: accepted
Date: 2026-08-18

## Context

The original query envelope exposed one `truncated` boolean. That flag could
not tell an agent whether callers, tests, source text, provider data, Git
inventory, or unresolved syntax candidates were incomplete. Before the lazy
call-site model, a workspace-global candidate counter also marked unrelated
queries truncated: an empty clean-worktree `diff_context` could therefore
claim incompleteness because another file contained an ambiguous call.

SPEC §§28–29 require structured bounded responses, and issue #9 requires
query-local completeness rather than workspace-global contamination.

## Decision

- Envelope schema v5 adds a bounded typed `truncation` list. Each detail has a
  response section, cause, configured limit, and an exact omitted amount when
  it is available without performing unbounded work.
- `QueryEnvelope::new` derives `truncated` from whether that list is empty. A
  producer cannot claim truncation without an explanation or return details
  while claiming the result is complete.
- Causes distinguish item limits, source line/character limits, precise
  provider limits, response-byte limits, unresolved candidate fan-out, and
  the Git adapter's change-inventory limit. Sections are language-neutral and
  name the public query response field that is incomplete.
- Precise-provider incoming and outgoing truncation are separate adapter
  facts. A context query can therefore attribute a provider cut to callers,
  callees, or both instead of setting an unexplained combined flag.
- Git diff inventory truncation is a typed adapter-neutral value with its own
  limit. Query-local `limit` truncation remains a separate detail even when
  both bounds affect `changed_files`.
- `status.counts.call_sites_with_truncated_candidates` exposes any index-time
  workspace candidate loss. Ambiguous and unresolved call-site counts remain
  separate. None of these workspace counters change a query envelope's
  completeness.
- Omitted counts may be `null` when determining the exact remainder would
  require continuing a deliberately bounded provider, Git, or query scan.
  This is more honest than inventing a count or doing the work after the
  budget has already stopped traversal.

## Alternatives considered

- Keep the boolean and add free-form messages: rejected because clients could
  not exhaustively interpret causes and section names would drift.
- Put truncation metadata inside each data structure: rejected because it
  duplicates the same contract across all seven queries and makes a complete
  response harder to inspect uniformly.
- Let workspace ambiguity set every high-level response's flag: rejected
  because unrelated graph state says nothing about whether the selected
  response sections are complete.
- Compute every omitted count exactly: rejected because it defeats early
  provider, Git, and traversal bounds. Exact counts are retained when the
  bounded vector is already available.

## Consequences

- Schema-v4 clients must accept schema v5 before relying on the new field.
  Existing `truncated` semantics become stricter rather than disappearing.
- Schema v6 adds ADR-024's caller aggregation and byte-first section budgets;
  schema v7 and ADR-025 add examined, graph-traversal,
  intermediate-allocation, and wall-time causes through the same
  language-neutral contract without another unstructured flag.
- The provider adapter contract carries directional truncation but still does
  not leak LSP types into the engine or domain.

## Validation / follow-up

- Domain tests prove the summary flag is derived from typed details and the
  current schema-v7 JSON contract round-trips.
- Engine regressions cover item, source-snippet, provider, unresolved-fanout,
  and Git-inventory causes.
- A clean empty `diff_context` remains complete even when status reports 66
  unrelated workspace call sites with truncated candidate sets.
- Release MCP smoke runs on 2026-08-18 returned `truncated=false` and an empty
  detail list for clean `psp-app` and Zed worktrees.
- MCP contract tests require schema v7 and expose the structured details.
- ADR-024 enforces response bytes within individual sections; ADR-025 applies
  execution-work causes while retaining this envelope contract.
