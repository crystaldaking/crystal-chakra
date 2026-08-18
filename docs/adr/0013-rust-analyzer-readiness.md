# ADR-013: Bounded rust-analyzer readiness and revision-delta synchronization

Status: accepted
Date: 2026-08-18

## Context

The pinned Zed evaluation has 1,929 Rust sources and 55.3 MB of retained text.
Syntax became useful independently, but the optional provider could remain
`catching_up` while loading Cargo metadata and indexing. The original first
precise query also rebuilt a complete document map and sent `didOpen` plus full
text for every file. Later calls compared the complete source catalog again,
and the precise cache was bounded only by entry count.

Upstream rust-analyzer exposes standard LSP work-done `$/progress` values and
the experimental `experimental/serverStatus` notification. Server status
directly reports only health, quiescence, and an optional message; it is not a
document-version acknowledgement. Work-done titles/messages such as Loading,
Roots Scanned, Fetching, and Indexing are display-oriented provider facts, not
stable completion barriers:

- <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#progress>
- <https://rust-analyzer.github.io/book/contributing/lsp-extensions.html#server-status>

## Decision

- Keep syntax readiness independent. Optional enrichment gets a one-second
  per-query wait budget inside the MCP end-to-end deadline. Expiry returns the
  already useful syntax facts with `catching_up`; it does not make a query wait
  for arbitrary workspace loading.
- Expose provider progress as a stage, bounded message, optional percentage,
  and explicit source. `provider` means a direct LSP progress/status fact;
  `chakra` means an inference from process startup, initialization, document
  synchronization, barrier, degradation, or shutdown. Cargo metadata,
  workspace loading, and indexing are classified from direct work-done
  title/token/message text and are not claimed as stable rust-analyzer API
  phases.
- Make `ProviderWorkspace` an immutable handle to the pinned graph rather than
  a newly allocated vector of every source. Target lookup is direct. A changed
  revision produces sorted created/changed/deleted Rust documents. Shared
  `Arc<str>` allocations are the fast exact unchanged fingerprint; a replaced
  allocation gets one value comparison, so conservative full checkpoints can
  avoid false changes without a hash collision hiding a real edit.
- Start rust-analyzer against the materialized fresh worktree and leave
  unchanged non-target documents disk-backed. Open only the selected target
  with its pinned source. For later deltas, send full-text `didChange` only for
  documents already opened by Chakra; send `workspace/didChangeWatchedFiles`
  for other creates/changes/deletes and `didClose` for removed open documents.
- Accept a precise result only when all of these hold: the syntax snapshot was
  fresh, provider health is OK and quiescent, a request after the current sync
  notifications completed the generation barrier, the result revision matches
  the pinned revision, a post-provider live freshness barrier reconciles the
  materialized worktree, and the engine still publishes that same fresh
  revision afterward. This second proof is required because an unopened caller
  remains disk-backed and could otherwise expose an edit before the watcher
  publishes a new syntax revision. Otherwise retain syntax evidence and report
  `catching_up`. An `allow_stale` request skips precise enrichment rather than
  silently turning its low-latency syntax read into a fresh barrier.
- Measure each synchronization: workspace documents/bytes, open documents,
  created/changed/deleted counts, text messages/bytes, watched-file events,
  catalog entries examined, and source bodies compared. Retain cumulative text
  and watched-event totals.
- Bound the precise cache by both 128 entries and 8 MiB. Approximate retained
  dynamic bytes include keys, names, source paths/ranges, and relation payloads.
  Evict least-recently-used entries until both limits hold. Revision changes
  invalidate other revisions; provider restart clears entries while retaining
  hit/miss/eviction counters. Expose entries, bytes, capacities, hits, misses,
  and evictions in `status`.
- On Unix, start rust-analyzer as a separate process-group leader. Cooperative
  LSP shutdown remains first; bounded fallback signals the whole group so
  rust-analyzer-owned Cargo/build-script descendants cannot remain orphaned.
  `nix` 0.31 is used only with its `signal` feature under `cfg(unix)`. Other
  platforms retain bounded direct-child termination.
- Do not invent crate-scoped Call Hierarchy initialization. Current LSP
  capabilities do not provide a guarantee that a workspace-wide incoming-call
  result is complete after loading only one Cargo package. Targeted enrichment
  remains query-time, but rust-analyzer owns its supported workspace model.

## Alternatives considered

- Continue opening all documents: rejected because first-query traffic and
  serialization scale with the full corpus even when the provider will miss
  the query wait budget.
- Trust `serverStatus.quiescent` alone: rejected because it does not acknowledge
  Chakra's newer document generation.
- Use only a content hash for deltas: rejected because a collision must never
  hide a change that would make precise facts stale. Allocation identity plus
  exact comparison is collision-free.
- Keep the last precise result across revisions: rejected because older facts
  must not be attributed to a newer workspace.
- Force lazy provider process startup: rejected for now. Opportunistic parallel
  startup does not block syntax availability and gives the provider useful
  loading time before the first precise query.

## Consequences

- A changed revision still performs linear metadata traversal to construct its
  exact Rust document delta. It does not copy or compare every unchanged source
  body, and an identical workspace/cache hit bypasses synchronization.
- Disk-backed non-target facts depend on Chakra's initial fresh-worktree proof,
  provider barrier, post-provider fresh-worktree proof, and final revision
  check. If any proof is lost, Chakra returns syntax rather than precision.
- Status and high-level Rust queries explain provider fallback without leaking
  LSP types into domain/query contracts.
- One Unix-only production dependency is added: `nix` 0.31.3 (MIT) with only
  the `signal` feature. Its wrapper avoids `unsafe` in Chakra.

## Validation / follow-up

- A hermetic 1,929-document/55,316,267-byte contract opens one 19-byte target,
  then changes one unopened document and observes one body comparison, one
  watched-file event, and no new full-text message. Repeating the query hits
  the cache without additional document traffic.
- Hermetic tests prove a 75 ms provider wait returns `catching_up` before a
  two-second request timeout, direct Cargo metadata progress remains labeled as
  provider-reported, and cache bytes never exceed their configured budget.
- Query contracts advance the engine revision during enrichment and from the
  post-provider freshness barrier, proving both same-number precise results are
  discarded. A separate regression proves `allow_stale` remains provider-free.
- A Unix lifecycle peer leaves a descendant running; provider shutdown proves
  the entire owned process group is reaped.
- The default suite remains independent of a global rust-analyzer. The ignored
  real-provider smoke remains the opt-in compatibility check.
