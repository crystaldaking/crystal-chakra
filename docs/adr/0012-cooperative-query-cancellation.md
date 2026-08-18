# ADR-012: Cooperative query deadlines and cancellation

Status: accepted
Date: 2026-08-18

## Context

MCP originally bounded expensive queries with two semaphore permits, but
cancellation was effective only before dispatch. Once a synchronous query had
entered `spawn_blocking`, dropping the client request detached the work until
its freshness, Git, provider, or graph traversal finished. Two abandoned calls
could therefore occupy both permits for their full local timeouts. SPEC §§35–36
requires cancellation-aware long operations, explicit child ownership, and no
partially published state.

Cancellation also crosses architectural boundaries. MCP cancellation types
must not enter the domain/query layer; Git process handles and LSP request ids
must remain inside their adapters.

## Decision

- Define `OperationContext` in `chakra-domain`. It contains a shared cooperative
  cancellation token plus an optional absolute deadline. Derived adapter-local
  bounds may shorten but never extend the caller's deadline. The type contains
  no Tokio, MCP, Git, or LSP concepts.
- Keep the existing `QueryService` methods as convenience wrappers with an
  unbounded context for direct/static callers. Context-aware entry points are
  the required implementation contract; `WorkspaceEngine` propagates their
  context through freshness, snapshot materialization, graph traversal, Git
  diff, and optional precise enrichment.
- Apply the same required-context/legacy-wrapper shape to `FreshnessBarrier`,
  `WorkspaceDiffProvider`, and `PreciseProvider`. This prevents a newly added
  adapter from compiling while silently discarding mid-operation cancellation.
- Give the MCP adapter a five-second queue deadline and a 30-second end-to-end
  execution deadline. A drop guard tied to the handler future cancels in-flight
  synchronous work when the client request disappears. The blocking closure
  retains its permit until cooperative unwinding completes, so a permit is
  never detached from still-running work.
- Poll CPU traversal cancellation once per bounded work batch (currently 256
  files, lines, symbols, edges, or call sites), plus before/after monolithic
  adapter calls. Do not add an atomic load to every graph edge.
- Keep `status` outside the expensive-query pool. Its optional
  `query_execution` section exposes current queued/running gauges, cumulative
  started/cancelled/queue-timeout/execution-timeout/completed/failed counters,
  and total/maximum permit hold time.
- Give every freshness waiter its own generation outcome. A later successful
  reconciliation cannot overwrite an earlier waiter's error. Waiters poll their
  operation context with a ten-millisecond bound and unregister on cancellation.
  A barrier-only reconciliation is cooperatively cancelled when no live waiter
  remains; watcher-driven reconciliation remains owned by the live index and
  continues independently. Cancellation is checked before publication, so a
  private candidate is either published completely or discarded.
- Thread the operation context through both Git inventory and worktree-diff
  commands. A child uses the earlier of the 30-second Git bound and the caller
  deadline. Cancellation kills and waits for the child, then joins both bounded
  pipe readers before returning; no child or reader is detached.
- Apply the same context to bounded `cargo metadata` classification. A caller
  cancellation or deadline kills and waits for the owned Cargo child and joins
  its bounded pipe readers; adapter-local metadata failures still degrade to
  deterministic path classification.
- Attach the context to each bounded rust-analyzer command. Queue and response
  waits poll cancellation; an in-flight LSP request sends `$/cancelRequest`.
  The provider process remains owned by the provider lifecycle and is reaped on
  restart/shutdown rather than being detached from the cancelled query.
- Distinguish queue timeout, execution deadline, caller cancellation, and MCP
  response-budget exhaustion with bounded JSON-RPC error `data.kind` values.
  Provider-local timeouts remain honest syntax fallback with `catching_up` and
  an operator-visible provider error; indexing/query collection budgets remain
  revision metadata and `truncated` rather than being mislabeled as timeouts.

## Alternatives considered

- Abort the Tokio `spawn_blocking` handle: rejected because running blocking
  tasks cannot be forcibly stopped safely and owned Git/provider resources
  still require cleanup.
- Put Tokio cancellation tokens in `QueryService`: rejected because transport
  and runtime choices would leak into the application contract.
- Kill rust-analyzer for every cancelled query: rejected because the process is
  workspace-owned and LSP supports request cancellation. Transport failure and
  shutdown retain explicit termination/reaping paths.
- Poll every graph item: rejected because batching gives bounded cancellation
  latency without turning an atomic load into a hot-edge cost.

## Consequences

- Cooperative implementations are part of the required context-aware query
  contract. Polling frequency remains implementation-specific, but adapters do
  not silently fall back to the legacy unbounded entry points.
- Client cancellation normally produces no response because the client has
  abandoned the request, but it remains visible in executor metrics. Queue and
  execution timeouts return distinct typed metadata when the transport is
  present.
- Automatic watcher work can finish after one query is cancelled. It remains a
  single owned worker and may publish only a complete latest revision; this is
  not detached per-query work.
- No production dependency was added.

## Validation / follow-up

- A deterministic MCP regression starts two blocking queries, waits until both
  hold permits, cancels both, and proves a third query starts without waiting
  for the original 30-second deadline. It also verifies executor gauges and
  counters.
- Freshness unit regressions prove generation outcomes cannot overwrite each
  other and that cancelling the last waiter cancels barrier-only work before
  publication.
- A fake owned Git process proves cancellation terminates and reaps the child
  promptly.
- A fake metadata command proves cancellation terminates and reaps the child
  promptly without waiting for the adapter-local 30-second bound.
- A hermetic LSP peer proves caller cancellation interrupts an already-sent
  precise request and records `$/cancelRequest` without requiring a global
  rust-analyzer installation.
- ADR-013 covers provider lifecycle refinements and broader provider-specific
  cancellation cases; the generated large-workspace gate exercises
  cancellation under load.
