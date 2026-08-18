# ADR-014: Bounded resource-aware syntax parsing

Status: accepted
Date: 2026-08-18

## Context

After ADR-010 removed eager call-edge fan-out and ADR-011 bounded retained
facts, parsing dominated the pinned Zed cold index: about 4.56 of 6.94 seconds
with one CPU effectively utilized. Rust and PHP files are independent until the
declaration catalog is complete, but graph ids, lexical budget allocation,
degradation reasons, and query order must remain deterministic.

Unbounded work stealing or parallel Rust/PHP builds could trade wall time for
uncontrolled retained results and memory. Ordinary one-file live reconciliation
also must not pay a thread-pool startup cost.

## Decision

- Add `max_workers` to `IndexBudgets`, defaulting to eight with a hard ceiling
  of 64. The effective worker limit is the minimum of the configured cap,
  `std::thread::available_parallelism()`, and a memory policy. The policy first
  reserves the configured workspace-source budget without consuming the final
  64 MiB, then assigns each parser worker a conservative 64 MiB reserve. A
  result of one is explicit low-resource mode; both reserve values are exposed
  in revision status.
- Parallelize only initial parse/extraction when one language has at least 32
  admitted files. Each scoped worker constructs and exclusively owns one
  Tree-sitter parser. No parser crosses a thread boundary.
- Distribute immutable, lexically ordered inputs through an atomic cursor. No
  retained task queue exists, so observed queue depth is truthfully zero.
  Per-worker result vectors collectively contain at most the already admitted
  file/source budgets. The owner joins every scoped worker, reduces through
  fixed original-input-index result slots, and only then constructs the
  `BTreeMap` used by later phases.
- Keep Rust and PHP adapter builds sequential. They share the same effective
  cap and therefore never double worker or memory use. The public Zed corpus
  contains only Rust; concurrent language adapters have no demonstrated benefit
  sufficient to justify the extra peak-memory policy.
- Keep relationship/call-site contribution construction sequential for v0.1.1.
  The global edge budget is allocated in lexical path order. Computing later
  files eagerly would either retain up to `workers × remaining_limit` edges or
  require a second full pass. Measurements show this phase is about 14 ms on
  Zed, so parallelism would add complexity to a non-dominant phase.
- Keep graph materialization, validation, composition, and revision publication
  serialized. Parallel workers only produce private parsed facts; queries still
  observe the previous complete revision or the next complete revision.
- Initial and live phase records expose wall time, process CPU time, CPU/wall
  utilization (1,000 = one logical CPU), active workers, scheduler queue depth,
  end-of-phase RSS where useful, and process high-water RSS. Scheduling status
  exposes configured/available/memory/effective limits and parallel/sequential
  file counts.
- Revision publication is timed by the outer startup/live owner rather than
  embedded in the revision being published: a revision cannot truthfully
  contain the duration of its own final compare-and-swap without a second
  publication. The acceptance harness records that sidecar measurement and
  reuses the immediately surrounding steady/high-water RSS observations.
- Advance the query envelope contract to schema v4 because scheduling and
  per-phase resource fields are now revision metadata on every response.
- Use the existing `nix` 0.31 dependency with its `resource` feature on Unix for
  a safe `getrusage` wrapper. This adds no new package to the lockfile. Other
  platforms report unavailable CPU/high-water observations as `None`.
- Keep ordinary live reconciliation sequential. Cancellation is checked before
  every claimed file; scoped ownership joins all parser workers before an error
  returns, so cancellation cannot publish partial facts or leave tasks behind.

## Alternatives considered

- Rayon or a persistent pool: rejected for this slice because scoped standard
  threads provide the measured benefit without a global runtime, hidden queue,
  or new scheduling dependency.
- Parallel relationship construction: rejected because deterministic global
  edge allocation would require excessive bounded intermediates or duplicate
  computation for a phase that is not currently material.
- Parallel Rust and PHP adapters: deferred until a mixed public workload shows
  wall-time benefit within the shared memory budget.
- Use wall time or RSS to stop workers: rejected because nondeterministic
  observations must never change graph contents. They cap concurrency or report
  warnings; count/byte budgets alone control retained facts.

## Consequences

- Worker completion order cannot affect ids, ordering, truncation, or
  degradation. Exact Zed fingerprints match for 1, 2, and 8 workers.
- Parser threads are created only for a qualifying initial language phase and
  joined before catalog construction. There is no background pool lifecycle.
- Process CPU metrics may include unrelated work in the same process if callers
  deliberately run other CPU tasks concurrently; the default startup owner
  performs indexing in isolation.
- The source and 64 MiB worker reserves are scheduling heuristics, not
  retained-graph budgets.
  ADR-011's file/source/symbol/edge/call-site ceilings remain authoritative.

## Validation / follow-up

- Unit tests run Rust and PHP with 1 and 4 workers and compare complete stable
  graph snapshots, facts, build reports, worker counts, and queue depth.
- A deterministic barrier test cancels four already-started Rust parser workers
  and proves the owner joins them before returning `Cancelled`.
- A mixed 80-file test proves the shared configured/effective cap is never
  exceeded and small fixtures remain sequential.
- On Zed commit `bc538def4545534201bbfcac4e95ac34ea6501b6`, release indexing
  improved from 6.941 s with one worker to 4.916 s with two and 3.416 s with
  eight. Eight-worker parse utilization reached 7.613 logical CPUs; external
  peak RSS was 937,459,712 bytes versus 906,264,576 with one worker.
- Issue #15 owns the long-lived generated/public regression gates and final
  v0.1.1 performance matrix.
