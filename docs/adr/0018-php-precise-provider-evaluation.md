# ADR-018: Defer precise PHP provider integration

Status: accepted
Date: 2026-08-17

## Context

Chakra's receiver-aware PHP syntax tier remains deterministic, current, and
independent of a PHP runtime, but it deliberately does not infer PHPDoc types,
generic container returns, or arbitrary factory return flow. Issue #2 asks
whether an optional maintained provider improves those gaps enough to justify
a new adapter and lifecycle in v0.1.1.

A PHP provider must remain an adapter under SPEC §15. It cannot become graph
truth, and precise facts may be labelled current only when their document
state matches the atomically published workspace revision. The selected v0.1
product queries also need caller/context value; definition accuracy alone is
not a complete provider contract.

The reproducible corpus, method, measurements, official sources, and full
tradeoff table are recorded in
`docs/evaluation/php-provider-v0.1.1.md`.

## Decision

Do not add a precise PHP provider adapter in v0.1.1.

- Keep PHP syntax and deterministic Laravel enrichment as the always-current
  baseline.
- Do not add provider protocol types, processes, dependencies, caches, or
  configuration to Chakra for this issue.
- Treat PHPactor as the only eligible candidate for a future separately
  approved definition/reference proof of concept. It is MIT, works as an
  on-demand PHAR, improved definition recall from 62.5% to 87.5% on the
  corpus, returned references, synchronized opened documents, and restarted
  cleanly after a forced crash.
- Do not treat PHPactor results as sufficient for precise callers: the tested
  release advertised no call hierarchy and returned method-not-found for
  `textDocument/prepareCallHierarchy`.
- Do not integrate or redistribute Intelephense without a separate licence
  agreement that explicitly covers Chakra's headless use and distribution
  model. Its free server produced the best corpus definition result, but its
  licence is not an open-source redistribution licence and its intended-use
  clause is narrower than Chakra's product role.
- Do not use Psalm as Chakra's provider for the selected queries. It is MIT
  and accurate for definitions on the corpus, but it advertises no references
  or call hierarchy, and a forced-crash restart did not initialize within the
  30-second evaluation bound.

Any future PHP provider issue must define a smaller capability contract and
must implement lifecycle ownership, bounded initialization/request/cancel/
shutdown, document-version tracking, revision-keyed caching, invalidation,
and honest catching-up/degraded fallback before a provider fact is exposed.
The default test suite must remain provider-free.

## Alternatives considered

- Integrate Intelephense because it reached 100% definition recall: rejected
  because call hierarchy is absent and the evaluated server licence does not
  authorize Chakra redistribution or clearly cover headless use.
- Integrate Psalm Composer-locally: rejected because definitions alone do not
  improve `callers`, references are unavailable, large-project startup is
  explicitly expensive, and the restart probe failed its bound.
- Integrate PHPactor for definitions/references now: deferred. It is the best
  licensable candidate, but this issue intentionally evaluates rather than
  implements a provider, and its missing call hierarchy prevents the precise
  caller value expected from the current high-level queries.
- Extend Chakra into a PHP type checker: rejected. PHPDoc generics, dynamic
  dispatch, framework containers, and runtime behavior are outside Chakra's
  deterministic syntax-adapter boundary.
- Label observed `didChange` ordering as a freshness guarantee: rejected. The
  successful corpus probes do not establish an atomic Chakra workspace/
  provider barrier.

## Consequences

- PHP remains useful without PHP, Composer, Node.js, or a language server.
- The three measured syntax false negatives remain explicit unresolved call
  sites rather than becoming guessed edges.
- v0.1.1 avoids a new child-process lifecycle and a provider-specific cache
  whose product value is not yet sufficient.
- The corpus and harness make a future provider decision repeatable against
  later releases and larger repositories.
- Intelephense can still be evaluated manually by an individual under its own
  licence; Chakra does not bundle, configure, or recommend it as a managed
  runtime dependency.

## Validation / follow-up

- `evaluate_provider_corpus` anchors syntax TP/FP/FN/TN and single-file
  incremental measurements.
- `tools/evaluate_php_lsp.py` probes real initialize capabilities,
  definitions, references, call hierarchy, synchronization, cancellation,
  crash/restart, shutdown, latency, response bytes, and process RSS.
- Fixture integration tests assert the syntax baseline and one-file
  reconciliation path without requiring any global provider.
- A future PHPactor proof of concept should first evaluate definition/reference
  value on a larger real-but-non-sensitive Composer repository and define how
  those capabilities improve `context` without claiming precise callers.
