# ADR-0051: Compatible complete commit snapshot reuse

Status: accepted
Date: 2026-09-02

## Context

ADR-0050 retains one immutable syntax graph per commit, but every registered
worktree still rebuilt identical Tree-sitter facts. The v0.2.0 per-file cache
experiment cannot be reused: its measured restore rebuilt the graph and was
only 1.10–1.33× faster than a cold build because materialization and the
consistency audit consumed 83–86% of restore time. Issue #49 therefore needs
to reuse the complete materialized commit state while preserving the private
incremental facts required by normal one-file reconciliation.

Commit SHA is insufficient compatibility evidence. The same object can
produce different bounded graphs under another repository identity, snapshot
format, graph model, parser/extractor version, Chakra version, or indexing
configuration. Worktree overlays and precise providers additionally depend on
materialization and must never enter the reusable commit layer.

## Decision

- `chakra-workspace` owns one bounded `CommitSnapshotCache` per repository
  registry. An exact process-local hit clones the persistent graph/index
  structures and substitutes only the destination repository root. Duplicate
  concurrent requests for one key coalesce behind one builder; waiters poll
  cooperative cancellation and no background task is introduced.
- The key contains the Git-aware local repository identity, exact commit (or
  unborn state), commit-snapshot format, graph-model version, the
  `chakra-language` version, every registered adapter's explicit snapshot
  version, and all indexing budgets that affect retained facts. The key is
  canonical JSON fingerprinted with BLAKE3; commit identity makes a separate
  worktree-content hash unnecessary.
- `chakra-language` owns the payload codec because only that layer can restore
  the object-safe adapter registry. Each adapter serializes its complete
  materialization-independent state: parsed facts, relationship
  contributions, already materialized per-language graph, graph limits, and
  framework facts. Restore merges the already built disjoint graphs and runs
  the complete graph consistency audit; it does not invoke Tree-sitter or
  rematerialize graph entities.
- The versioned payload uses named MessagePack structures inside a bounded
  envelope with exact length and BLAKE3-128 checksum. Named fields tolerate
  Serde's optional/omitted domain fields and keep migration failures explicit.
  Every adapter carries a manually bumped `language:sN` state-format version.
- Optional disk storage publishes `snapshot.bin`, `manifest.json`, and an
  access marker through a same-directory temporary directory plus rename.
  Readers accept only a complete manifest/payload pair. Missing, incompatible,
  corrupt, oversized, or unreadable data falls back to the exact deterministic
  Git-object build; corrupt artifacts are replaced by the successful rebuild.
- Memory entries are LRU-bounded (four by default). Configured disk storage is
  bounded by artifact count, individual payload bytes, and aggregate bytes;
  eviction visits only resolved cache-entry directories and skips temporary or
  symlink entries. Disk I/O checks cancellation between 64 KiB chunks. The
  disk path is opt-in until issue #50 measures complete restore/verification;
  process-local sharing is enabled for every workspace registry.
- `CommitSnapshotLayer.reuse` reports `cold_build`, `memory_reuse`, or
  `disk_restore`, reused file/source-byte counts, elapsed time, artifact size,
  and a typed rejection reason. The additive query envelope advances from
  schema 17 to 18. Live `HEAD` replacement uses the same cache provider and
  preserves the prior reuse record on ordinary file reconciliation.
- The payload never includes worktree-overlay ownership, provider inputs, a
  provider process/cache, or precise enrichment. Overlay composition and
  provider readiness remain independently verified and atomically published
  for each materialized worktree.

## Alternatives considered

- **Persist the v0.2 per-file fact projection and rebuild the graph.** Rejected
  by the existing measured no-go: it preserves nearly all startup cost.
- **Cache only `SymbolGraph`.** Rejected because the live owner would have to
  reparse the repository before it could reconcile one file, eliminating the
  startup benefit or violating the no-whole-reindex invariant.
- **Use commit SHA as the key.** Rejected because repository identity,
  extraction/model versions, and budgets change graph meaning and coverage.
- **Put storage in `chakra-engine` or domain types.** Rejected because the
  engine must not depend on persistence policy and only the language adapter
  layer can restore its private incremental state.
- **Enable disk restore in the CLI immediately.** Rejected pending issue #50's
  explicit full-restore benchmark and trust/transport decision.

## Consequences

- Identical commits avoid all parsing and graph construction within a running
  multi-worktree registry. Optional restart reuse restores the same complete
  state and remains a fallible optimization.
- Serialization is intentionally supported for internal graph and adapter
  types. These are not stable public data contracts; format/model/adapter
  versions invalidate artifacts across incompatible changes.
- New production dependencies are `blake3` (integrity and collision-resistant
  keying; already evaluated by the rejected #39 prototype) and `rmp-serde`
  (compact named MessagePack over existing Serde domain types). `serde` gains
  `rc` and `rpds` gains `serde` features so shared immutable structures encode
  without a parallel shadow model.
- Disk artifacts can be larger than retained source because they contain both
  query-ready graph indexes and live incremental facts. Issue #50 must measure
  size, wall/CPU/RSS, and verification cost before choosing a default or a CI
  transport.

## Validation / follow-up

- Linked-worktree tests cover cold/memory reuse, overlay/provider isolation,
  `HEAD` transition reuse, restart restore, corruption repair, format
  migration, budget mismatch, cancellation, LRU disk eviction, process-local
  coalescing, and concurrent atomic disk publication.
- Query contracts pin schema 18 and serialize the typed reuse/fallback record.
- Issue #50 will compare deterministic rebuild, local disk restore, and a
  trusted prebuilt artifact on the representative corpus. It may enable the
  disk configuration only after architecture-review approval of the measured
  result.
