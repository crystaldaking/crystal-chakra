# ADR-0047: Workspace-bound lazy file facts

Status: accepted
Date: 2026-08-30

## Context

Some derived per-file facts are useful only for a small fraction of files and
would add publication latency and retained payload if built eagerly for every
revision. Issue #42 requires an on-demand mechanism without introducing an
untyped cache, weakening atomic revisions, or allowing duplicate concurrent
work and cache state to grow without bounds.

A fact may depend on more than source bytes. File path, the immutable graph,
workspace/config state, producer semantics, and the exact published revision
can all change the answer. A content-only key would therefore be incorrect for
identical files in different paths, and a revision number alone is not unique
across workspaces.

## Decision

- `chakra-engine` owns a typed `LazyFactProducer`/`LazyFactStore<P>` contract.
  Each producer declares one concrete fact type, a stable id and format
  version, a wall-time and retained-byte budget, provenance, precision, and a
  typed invalidation key. There is no type-erased value map or arbitrary cache
  namespace.
- Public `FileFactInput` values are constructed from one immutable
  `WorkspaceSnapshot`. The constructor obtains the source and graph from that
  snapshot, so callers cannot combine a revision label with facts from a
  different publication. A store is bound to one `WorkspaceId` and rejects
  input from another workspace.
- The default exact-file invalidation key contains repository-relative path,
  content identity, and revision. Producer identity and workspace identity are
  fixed by the store namespace. Path remains part of the key because graph-
  and path-derived facts for byte-identical files need not be equal.
- Ready values use count- and estimated-payload-byte-bounded LRU retention.
  The count bound includes in-flight unique computations. Admission may evict
  a ready LRU value; if every bounded slot is in flight, a new unique request
  receives typed `StoreSaturated` backpressure instead of growing the map.
  An oversized result may be returned to its owner and waiters but is not
  retained.
- Duplicate requests for one key share an owned `InFlightSlot`. The first
  caller computes; waiters use their own cancellation/deadline and a bounded
  condition-variable polling interval. Owner failure or cancellation is
  published once to existing waiters and removed from the map, so a later
  request recomputes. No task is detached.
- The producer must poll the bounded operation while computing. The store also
  checks the effective deadline after `compute` returns and refuses to publish
  a late result.
- `FileOutlineDigestProducer` is the first real producer. It derives a bounded
  per-file outline from the published syntax graph and labels it
  `TreeSitter`/`Syntax`. It is measured by the hermetic and opt-in real-worktree
  conformance harness, but is not yet exposed through query or MCP contracts.

## Alternatives considered

- **A general `Any`/JSON cache.** Rejected because keys, values, provenance,
  budgets, and compatibility would become runtime conventions.
- **Content-only keys.** Rejected because path- and graph-dependent producers
  would alias byte-identical files.
- **Revision-only keys.** Rejected because revisions are workspace-local and
  one revision contains many files.
- **Unbounded in-flight coalescing.** Rejected because coalescing duplicates
  does not bound a burst of unique keys.
- **Put lazy values in the published graph.** Rejected because on-demand facts
  are enrichment; mutating an immutable revision or delaying publication would
  violate snapshot semantics.

## Consequences

- Lazy facts cannot contaminate or partially update a published revision; an
  outcome always records its pinned revision, provenance, and precision.
- Cache pressure and producer failures degrade explicitly through typed errors
  and counters. Cancellation cannot leave a poisoned ready entry.
- Count bounds are strict across ready and in-flight entries. The byte bound
  covers producer-reported retained fact payload; fixed map/key/synchronization
  overhead remains bounded indirectly by the entry limit and is not reported
  as payload bytes.
- Wiring a producer into `context` or MCP requires a separate query-contract
  decision and measured user value; ADR-0047 does not silently expand the
  public tool surface.

## Validation / follow-up

- Unit tests cover content/path/revision/workspace isolation, coalescing,
  waiter and owner cancellation, failure retry, strict in-flight saturation,
  LRU count/byte eviction, oversize handling, late-result rejection, and
  provenance honesty.
- Conformance compares eager computation with a sparse lazy workload using
  deterministic computation/cache counters and records wall time as local
  evidence rather than a cross-machine SLA.
