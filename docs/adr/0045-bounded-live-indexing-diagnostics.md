# ADR-0045: Bounded live indexing diagnostics

Status: accepted
Date: 2026-08-30

## Context

Revision-scoped `IndexingStatus` explains the graph currently attached to a
query, but it does not explain cross-revision live behavior: why the watcher
forced a full reread, which paths were invalidated, whether freshness work was
coalesced, or whether a cache participated. Logs are not a machine-readable
contract for coding agents.

The diagnostics must not expose source contents, grow without bound, invent
unsupported cache activity, or create a second live owner whose lifecycle can
become detached from freshness reconciliation.

## Decision

- `chakra-domain` owns `IndexingDiagnostics` and its typed reason/counter
  values. MCP serializes these domain types without introducing protocol
  types into the engine or language layers.
- `status` exposes diagnostics as an optional additive section. It is absent
  when the engine has no live freshness owner. Revision-scoped timings,
  coverage, resource samples, degradation, and publication facts remain in
  `IndexingStatus` and are not duplicated.
- The single installed `FreshnessBarrier` may provide diagnostics. Freshness
  and diagnostics therefore share one owner and one installation step; a
  failed second installation cannot leave a stopped diagnostics source in the
  engine. The live owner retains its worker, bounded queue, shutdown path, and
  diagnostics state.
- Per-file invalidations contain only repository-relative path and a typed
  reason. The newest 32 records are retained; a cumulative record count makes
  eviction observable. Cold startup does not flood the window with every
  discovered file.
- Project-scope invalidations reuse the bounded `ProjectModelImpact` contract
  from ADR-0044. Diagnostics retain cumulative impact counters and the latest
  non-empty detailed impact: changed unit ids with typed reasons, affected
  dependents, and manifest-issue transitions. This connects diagnostics to
  the same reconciliation result that was atomically published rather than
  constructing a second project diff.
- Full-reread causes are cumulative and retain the complete typed cause set
  for the most recent full reconciliation. Simultaneous watcher causes are not
  collapsed by priority, and stable-scan retry escalation is reported as
  `scan_instability`.
- Queue diagnostics include the named per-class scheduled-work counters and
  latency from ADR-0046 in addition to watcher and freshness-barrier state.
- Engine-observed cold builds count only successful publications that replace
  the graph with non-incremental rebuilt-file evidence. Freshness-only
  publications inheriting the same indexing status do not increment it.
- The per-file syntax fact cache is disabled because issue #39 failed its
  acceptance gates. Diagnostics expose a typed disabled state and reason;
  version, hit/miss/rebuild, and corruption-fallback counters do not exist
  until a cache actually ships.

## Alternatives considered

- **Rely on logs.** Rejected because agents cannot discover or safely parse
  process-specific logging configuration and history.
- **Attach all counters to every query envelope.** Rejected because the
  cross-revision operational picture is status-only and would waste response
  bytes.
- **Use an independent diagnostics owner.** Rejected because two `OnceLock`
  installations cannot roll back atomically; a barrier conflict could leave
  diagnostics pointing at an already stopped worker.
- **Retain unbounded path history.** Rejected because long-lived MCP service
  memory would grow with edit activity.
- **Report zero cache counters.** Rejected because zero would imply a real,
  compatible cache with no activity rather than an intentionally absent
  capability.

## Consequences

- Operators and agents can distinguish no-op, targeted, and full freshness
  work; inspect bounded file and project-scope reasons; observe queue/watcher
  health; and see why a full reread occurred without reading logs or source
  text.
- Operational counters are sampled concurrently and are not intrinsic graph
  facts. The envelope revision still identifies the atomically published
  graph; diagnostics describe the live owner's monotonic operational state at
  status-query time.
- Adding a real cache later requires extending `CacheHealth` with measured
  states and updating the issue #39 acceptance record; the disabled contract
  must not be silently reinterpreted.

## Validation / follow-up

- Domain tests pin serialization and verify no source-content marker appears.
- Engine tests prove cold-build accounting and freshness-only non-duplication.
- Hermetic live tests cover cold start, warm no-op, one-file edit, metadata
  rewrite, manifest-only project-scope invalidation, create/delete reasons,
  bounded record eviction, and status/MCP exposure.
- Full-reconciliation policy tests cover each cause and simultaneous causes.
