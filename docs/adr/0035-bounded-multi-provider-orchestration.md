# ADR-0035: bounded multi-provider orchestration

Status: accepted
Date: 2026-08-20

## Context

Chakra has independent rust-analyzer, vtsls, pyright, jdtls, and csharp-ls
adapters, but the workspace engine could install only one provider. Starting
every configured provider eagerly would also make polyglot workspaces consume
processes and memory without a query needing precise enrichment. Issue #26
requires at least three simultaneous providers while retaining deterministic
resource bounds, exact-revision routing, syntax fallback, cancellation, and
owned shutdown.

## Decision

- `WorkspaceEngine` stores multiple language-neutral `PreciseProvider`
  adapters. Every supported language has at most one installed owner;
  overlapping registrations are rejected instead of depending on install
  order.
- `chakra-provider-pool` owns provider activation policy. Its lazy wrappers
  are installed in the engine, while factories start adapter workers from
  the exact `ProviderWorkspace` pinned by the query.
- Enabled adapters are registered independently of the startup file
  inventory. This preserves precise routing when live reconciliation adds a
  language later, without discovering commands or starting processes eagerly.
- Admission is bounded by concurrent-query and queued-query limits. Waiting
  requests are ordered by explicit background/normal/interactive priority,
  then FIFO. Saturation, queue timeout, or cancellation returns honest syntax
  fallback metadata and never stale precise facts.
- Active provider count and deterministic memory reservations are hard
  limits. An inactive least-recently-used provider may be stopped to admit a
  different language; an idle reaper stops inactive providers after a
  configured timeout. A provider with an in-flight query is never evicted.
- Activation failure uses bounded exponential backoff. Adapter-owned crash
  recovery remains inside each adapter, so the pool does not learn LSP or
  process protocol types.
- `PreciseProvider::shutdown` is the generic, idempotent lifecycle boundary.
  The pool owns its reaper thread, stops admission before shutdown, waits
  boundedly for admitted work, joins the reaper, and invokes every active
  adapter shutdown. Existing adapter tests remain the proof that their child
  process groups do not become orphans. Failed eviction shutdown keeps its
  reservation occupied, restores the provider handle for cleanup retry, and
  is surfaced in error state and metrics rather than admitting replacement
  work optimistically. The same retention rule applies when a newly activated
  adapter is rejected or the pool begins shutdown before admission completes;
  a retained adapter with a cleanup error is never reused for queries.
- `ProviderState::Dormant` distinguishes a configured lazy provider from an
  absent one. Provider-pool capacity, lifecycle, saturation, timeout, and
  cancellation counters are exposed through provider metrics. This additive
  response contract advances the envelope schema to version 8.
- CLI provider discovery is also deferred to the first activation and cached
  independently of process lifetime. Merely starting syntax/MCP service does
  not run npm discovery for an unused provider.

## Consequences

- Rust, TypeScript/JavaScript, Python, Java, and C# precise providers can
  coexist in one materialized polyglot workspace without cross-language
  routing.
- A first precise query may pay command discovery and provider startup cost;
  syntax results remain available while the provider is dormant, catching
  up, saturated, or degraded.
- Memory figures are reservations used for deterministic admission, not a
  claim about measured provider RSS. Observed RSS enforcement may be added
  only with a separately designed, portable measurement contract.
- The orchestration crate is intentionally synchronous because the provider
  contract and adapter workers are synchronous; every admission and lifecycle
  wait is bounded or cancellation-aware.
- ADR-0049 extends these limits across multiple registered worktrees while
  keeping every active provider instance and document stream worktree-bound.

## Validation

- Hermetic pool tests activate three providers concurrently within process
  and reservation budgets.
- Deterministic tests cover priority/FIFO admission, full-queue and queue-timeout
  fallback, queued cancellation, activation backoff, LRU resource eviction,
  idle shutdown, restart, and final shutdown.
- Engine tests cover disjoint-language installation and rejection of
  ambiguous or empty routing.
