# ADR-016: Deduplicated test relationships

Status: accepted
Date: 2026-08-17

## Context

ADR-010 materializes a heuristic `TESTS` relation when a resolved syntax call
is owned by a test symbol. Treating that relation as occurrence-level created
one identical test result for every repeated call site. More importantly, PHP
tests with unresolved receivers previously risked inheriting the old
same-name fanout semantics that ADR-010 and ADR-011 removed from `CALLS`.

Agents need a bounded list of actual related test entities. Individual source
occurrences remain useful evidence, but they must not consume separate test
result slots or turn an ambiguous/dynamic receiver into a claimed test target.

## Decision

- `CALLS` remains occurrence-level: every uniquely resolved syntax call keeps
  its own edge and source range.
- Syntax-derived `TESTS` is entity-level: one test symbol and one resolved
  target produce at most one relation in a workspace revision.
- The first deterministic resolved call site is the representative range and
  evidence for that `TESTS` relation. All repeated occurrences remain in the
  revision-local call-site arena; later response aggregation may expose a
  bounded representative set without rebuilding graph topology.
- Ambiguous and unresolved call sites, including dynamic PHP receivers, never
  create `TESTS` edges. They remain syntax call-site evidence on the test
  caller and are not promoted by test names or textual similarity.
- Query results deduplicate test entities defensively across relationship
  sources. Higher-precision evidence wins; at equal precision a direct
  resolved call with call-site evidence ranks before a relation without such
  evidence. Weak candidates remain in their dedicated candidate collection.
- Deterministic framework relations may add `TESTS` independently in future,
  but must retain their own provenance and heuristic precision unless a
  precise provider justifies a stronger claim.

## Alternatives considered

- Keep one `TESTS` edge per call occurrence and deduplicate only after query
  collection: rejected because the graph would retain redundant topology and
  every query would need to rediscover the entity-level invariant.
- Drop repeated call sites after creating the first test relation: rejected
  because occurrence ranges are useful evidence for callers, diagnostics, and
  bounded aggregation.
- Infer test targets from method names or textual mentions: rejected because a
  name match is neither a reference nor receiver evidence.

## Consequences

- Test result count is proportional to distinct `(test, target)` pairs rather
  than the number of calls made by a test.
- A test can still appear in ordinary caller results once per call occurrence;
  ADR-024 aggregates that evidence under byte-first response budgets.
- The graph consistency audit accepts one representative `TESTS` edge for all
  resolved call sites in the same test/target pair while still requiring every
  resolved test call to have that relationship.
- The rule is language-neutral, while PHP regression coverage specifically
  proves typed receivers resolve and dynamic receivers remain unrelated.

## Validation

- Graph tests prove two resolved calls create two `CALLS` edges, one `TESTS`
  edge, and retain two call sites.
- PHP index tests cover typed job/service receivers, duplicate calls,
  same-named unrelated methods, and an unresolved dynamic receiver.
- MCP coverage verifies `context` and `diff_context` expose one related PHP
  test with receiver provenance and representative call-site evidence.
- A release-mode `psp-app` evaluation found six distinct directly calling
  tests for `ExpirePendingTransactionsJob::handle` and 16 for
  `TransactionStatusService::syncStatus`; no unrelated same-name `handle`
  method produced a test relationship.
