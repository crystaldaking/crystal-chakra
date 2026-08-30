# ADR-0046: Bounded priority scheduling for interactive freshness

Status: accepted
Date: 2026-08-30

## Context

A fresh query after an edit must not wait behind unrelated background work,
but Chakra has more than one queue and lifecycle owner. The live syntax worker
owns filesystem/freshness reconciliation, while the provider pool owns precise
provider admission. A single global scheduler would couple adapters, add a new
background owner, and weaken existing cancellation and shutdown boundaries.

The v0.2.0 reconciliation checkpoint also used to turn whichever freshness
request crossed the configured interval into a full source-body reread. That
made background maintenance indistinguishable from interactive work and could
inflate the latency of a one-file edit.

## Decision

- `chakra-domain` defines a shared `WorkClass` vocabulary ordered from
  `freshness_edit` through `provider_sync`, `reconciliation`, `cache_warmup`,
  and `maintenance`. It also owns typed, self-describing per-class queue
  metrics. The vocabulary does not invent a global queue: each existing owner
  schedules only work it actually owns.
- The live syntax worker stages transported signals in a worker-local
  `PriorityWorkQueue`. Each class has a hard 256-entry bound behind the
  existing bounded transport channel. Each scheduling pass drains at most one
  channel capacity before selecting staged work, so a continuously replenished
  transport cannot bypass aging. Watcher evidence rejected by either bound
  advances the dropped-event counter, forcing the existing conservative
  reconciliation path; freshness-barrier demand remains durable in its
  generation counter.
- The live queue uses elapsed-time aging. Waiting work gains one urgency level
  per second and eventually competes at the top class, with FIFO sequence
  ordering breaking ties. Interactive work is therefore preferred while
  older bounded work cannot starve.
- Periodic full rereads are explicit queued `reconciliation` work. A freshness
  edit can overtake a queued checkpoint, and any full reconcile cancels
  obsolete queued checkpoints. A running full reconcile is not preempted.
  Correctness-driven full rereads (cold start, watcher errors/drops, uncertain
  hints, scan instability) retain their existing conservative behavior and
  typed diagnostic reasons.
- Provider admission keeps its existing bounded queue and three request
  priorities. Admission-based aging promotes every remaining waiter after a
  successful admission; a background request reaches top rank after two
  admissions, then bounded FIFO ordering prevents newer interactive arrivals
  from overtaking it. Cancellation and timeout still remove the exact waiter.
- Live scheduled-work metrics are exposed through
  `IndexingDiagnostics::queue`; provider admission latency remains in the
  workspace-global `ProviderOrchestrationMetrics`. Wire fields use named
  classes/priorities rather than undocumented array positions.
- The syntax worker and provider pool remain the task owners. Shutdown drains
  or cancels staged work, no task is detached, and graph construction still
  publishes only through the existing atomic revision gate.

## Alternatives considered

- **One global scheduler.** Rejected because syntax reconciliation and
  provider admission have different owners, cancellation tokens, and resource
  limits; centralizing them would couple domain behavior to adapters.
- **Strict priority without aging.** Rejected because a sustained stream of
  edits could starve reconciliation or low-priority provider requests.
- **Guaranteed per-class time slices.** Rejected because it would delay short
  interactive bursts and require another clock-driven owner. Aging supplies a
  bounded convergence rule inside the existing worker loop.
- **Keep interval checks inside freshness reconciliation.** Rejected because
  the next edit would accidentally inherit background full-reread work.
- **Preempt an in-progress full scan.** Deferred: private scan construction is
  already cancellation-aware for observed demand, but safe checkpoint
  preemption needs measured value and a distinct retry contract. v0.2.0
  guarantees ordering of queued work, not arbitrary CPU preemption.

## Consequences

- A queued one-file freshness edit is selected before a queued reconciliation
  checkpoint and still publishes one complete revision.
- Queue pressure, cancellation, supersession, rejection, and latency are
  observable by named priority class without exposing protocol-specific
  types.
- Only `freshness_edit` and `reconciliation` are staged in the live queue
  today. Provider sync has its own admission queue; cache warmup and
  maintenance remain explicit zero-activity vocabulary until a real queued
  producer exists.
- Fairness bounds when old work reaches top rank. Actual completion still
  depends on the finite work already ahead of it and on owned task duration.

## Validation / follow-up

- Scheduler unit tests cover ordering, FIFO ties, aging, per-class bounds,
  typed rejection, cancellation, supersession, and named metric
  serialization.
- Live tests cover checkpoint staging/cancellation, typed periodic-checkpoint
  diagnostics, a one-file targeted edit, atomic old/new snapshots, and the
  invariant that every accepted staged item is dequeued or cancelled.
- Provider-pool tests cover promotion to top rank, latency attribution,
  saturation, timeout, cancellation, and shutdown/resource lifecycle.
