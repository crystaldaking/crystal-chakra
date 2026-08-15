# ADR-006: Optional rust-analyzer precise enrichment

Status: accepted
Date: 2026-08-15

## Context

Roadmap §§4–8 and 16 require selected queries to benefit from current
rust-analyzer facts without making a live language server the source of truth
for Chakra's syntax graph. SPEC §§5–7, 9, 15–17, 34–37, and 44–45 require
explicit provider lifecycle and synchronization, revision-aware freshness,
bounded work, honest degradation, and protocol isolation.

Current upstream rust-analyzer advertises standard LSP Call Hierarchy and an
experimental server-status notification that reports health and whether
background work is quiescent:

- <https://github.com/rust-lang/rust-analyzer/blob/master/crates/rust-analyzer/src/lsp/capabilities.rs>
- <https://github.com/rust-lang/rust-analyzer/blob/master/crates/rust-analyzer/src/handlers/request.rs>
- <https://rust-analyzer.github.io/book/contributing/lsp-extensions.html#server-status>

## Decision

- Add `chakra-provider-rust-analyzer` as a real adapter crate. It depends on
  the engine's small Chakra-native `PreciseProvider` contract; engine and
  domain crates do not depend on LSP types. URI handling, zero-based UTF-16
  positions, JSON-RPC messages, and rust-analyzer lifecycle stay inside the
  adapter.
- Use maintained `lsp-server` 0.10.0 for LSP framing/message types and
  `lsp-types` 0.97.0 for protocol structures. Use `url` 2.x for correct file
  URI conversion and `crossbeam-channel` 0.5 for bounded process/command
  channels. Versions are workspace-managed.
- Use only `textDocument/prepareCallHierarchy`,
  `callHierarchy/incomingCalls`, and `callHierarchy/outgoingCalls` for v0.1.
  These operations directly improve `callers` and the caller/callee parts of
  `context`. Do not enumerate all symbols, request all references, or write a
  precise call graph into the core graph.
- Own one rust-analyzer child in one named worker thread for the single v0.1
  worktree. Startup, initialize/initialized, health, bounded requests,
  `$/cancelRequest`, one automatic restart after transport failure,
  shutdown/exit, child termination fallback, stderr draining, and thread
  joining are explicit. Command and incoming-message queues are bounded.
- Treat the atomically published syntax snapshot as canonical. A precise
  query receives captured `Arc<str>` documents and the selected symbol from
  one pinned snapshot. It compares exact source text with its last synchronized
  document set and sends full-text `didOpen`/`didChange`, `didClose`, and
  watched-file notifications only for changed paths. Rename is represented as
  delete plus create.
- A request sent after document notifications is an LSP FIFO barrier for the
  in-memory document versions. Precise facts are returned only after a healthy
  `experimental/serverStatus` reports `quiescent`; if the bounded barrier
  expires, the adapter returns no precise facts and reports `CatchingUp`.
  Warning/error health and process/protocol failures report `Degraded`.
- Every precise result carries the exact workspace revision it enriches. The
  query layer accepts `RustAnalyzer`/`Precise` relations only when that revision
  equals its pinned syntax snapshot and provider state is `Ready`; otherwise it
  retains syntax candidates and reports the honest provider state. This
  query-relative provider state does not mutate the immutable syntax snapshot.
  Publishing provider readiness as a new workspace revision would immediately
  invalidate a result keyed to the preceding revision.
- Cache only completed ready results, keyed by workspace revision, provider
  process epoch, selected declaration, requested directions, and result limit.
  A workspace revision change removes older entries; restart increments the
  provider epoch and clears the cache.
- Start the provider opportunistically in `chakra serve`. A missing or failing
  executable cannot prevent syntax indexing or MCP service. `context` and
  `callers` run through MCP's existing bounded blocking-query executor.

## Alternatives considered

- Use rust-analyzer as the primary graph/index: rejected because it would make
  Chakra availability and architecture depend on one live provider, misstate
  historical/offline capabilities, and encourage an eager precise call graph.
- Use references or goto-definition for every indexed symbol: rejected because
  v0.1 only needs selected caller/context lookups and whole-workspace crawling
  has poor startup and invalidation behavior.
- Trust filesystem watching alone for provider currency: rejected because
  rust-analyzer may lag the already-reconciled Chakra revision. Explicit
  document versions plus a provider barrier make the claim testable.
- Return the last cached precise result while the provider catches up: rejected
  because an older precise fact must never be labeled current. Syntax fallback
  is useful and honest.
- Require rust-analyzer in the default test environment: rejected because core
  syntax intelligence and provider contract tests must be hermetic.

## Consequences

- Production dependencies added: `lsp-server` 0.10.0 (MIT OR Apache-2.0),
  `lsp-types` 0.97.0 (MIT), `crossbeam-channel` 0.5 (MIT OR Apache-2.0), and
  `url` 2.x (MIT OR Apache-2.0). URI correctness and mature protocol framing
  are accepted despite the additional compile/transitive cost.
- The first precise request in a large workspace may return syntax with
  `CatchingUp` when the bounded quiescence wait expires. A later request can
  use the now-ready provider; callers never need to sleep for correctness.
- Precise relations are bounded at the Chakra response boundary. The local
  provider protocol may still produce a larger single Call Hierarchy response;
  v0.1 trusts its owned local rust-analyzer process rather than adding a second
  custom LSP framing implementation solely for byte quotas.
- v0.1 still owns one provider for one materialized worktree. Historical
  precise indexing and provider pools remain deferred.

## Validation / follow-up

- Engine contract tests prove that a current precise caller replaces the same
  syntax candidate, an older precise revision is discarded and reported as
  `CatchingUp`, and provider degradation preserves useful syntax callers.
- Adapter unit tests cover UTF-16 conversion, repository-scoped file URIs, and
  deterministic missing-executable degradation without a global provider.
- An ignored real-provider smoke test exercises initialization, quiescence,
  incoming Call Hierarchy, conversion, and cooperative shutdown when
  rust-analyzer is installed. The default workspace suite does not run it.
- MCP contract tests invoke both `context` and `callers` through a real
  structured transport.
