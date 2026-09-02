# ADR-052: No default shared or prebuilt indexes after complete-snapshot evaluation

Status: accepted
Date: 2026-09-02

## Context

ADR-0051 introduced complete compatible commit snapshots so identical commits
can share immutable syntax state inside one process. It deliberately left disk
restore opt-in until issue #50 compared deterministic rebuild, same-machine
restore, and a CI-produced artifact simulation using the real codec and full
graph verification.

The v0.2 persistence decision established fixed gates above 1,000 indexed
files: restore must be at least 5× faster than rebuild, artifact bytes must be
at most 2× retained source, and graph results must match exactly. Adding a
prebuilt transport also requires an explicit trust and provenance boundary;
artifact integrity alone does not authenticate its producer.

The v0.3 evaluation measured the production snapshot path on the pinned
Rust/PHP corpus. Laravel restored at only 0.41–0.44× cold-build speed and its
artifact was 16.49× source bytes. Symfony exceeded the bounded 512 MiB writer
before an artifact could be completed. Successful local and copied-artifact
restores were graph-exact, but correctness does not justify an optimization
that is slower and substantially larger than rebuilding.

## Decision

- Keep process-local compatible commit sharing enabled. It avoids parsing,
  encoding, disk I/O, and decoding, and is independent of the rejected disk
  economics.
- Keep the complete-snapshot disk directory opt-in. Do not configure it by
  default in the CLI or workspace registry.
- Do not add a prebuilt/CI snapshot import, fetch, publication, or discovery
  surface in v0.3.0. Chakra core continues to require no network service.
- Retain the release-only `chakra-conformance shared-indexes` harness as the
  executable acceptance gate. A future representation or restore design must
  demonstrate at least 5× complete restore, at most 2× source bytes, and exact
  graph equivalence on repositories above 1,000 files before product wiring.
- A future prebuilt proposal, after passing performance gates, must use an
  authenticated artifact producer/channel and explicit user opt-in. Its
  provenance must bind producer policy, Git-aware repository identity, exact
  commit, the complete syntax compatibility key, fact scope, digest, and byte
  length. Import must stage bounded verification before atomic local
  publication. BLAKE3 supplies integrity, not authenticity.
- Imported complete snapshots may contain only materialization-independent
  commit syntax and the adapter state needed to reconcile it. Worktree
  overlays, provider state/inputs, precise enrichment, and environment-derived
  facts remain owned by the materialized worktree and are never transported
  under this format.
- Missing, incompatible, corrupt, oversized, or unauthenticated data always
  falls back to the deterministic Git-object build; an artifact is never a
  source of truth.

## Alternatives considered

- **Enable local disk restore because exactness passed.** Rejected: Laravel
  restore was 2.3–2.4× slower than a cold build and used 16.49× source bytes;
  Symfony could not encode within the safety bound.
- **Ship prebuilt import only because transport was cheap.** Rejected:
  transport took 22–360 ms, but decoding and auditing the copied payload had
  the same multi-second cost as local restore. Distribution does not remove
  the bottleneck and adds a trust surface.
- **Lower the v0.2 gates after seeing the data.** Rejected: even a neutral 1×
  speed threshold and a relaxed 5× size threshold would fail on qualifying
  targets. Changing budgets post hoc would not establish agent value.
- **Persist provider enrichment with the snapshot.** Rejected: it would mix
  materialization-dependent evidence into commit truth and require a richer
  environment/lifecycle fingerprint. It cannot repair the syntax payload's
  measured cost.

## Consequences

- v0.3.0 adds no default disk-cache latency, disk consumption, network
  dependency, CI trust configuration, or new artifact attack surface.
- The opt-in store remains useful for testing compatibility and future codec
  work but is not advertised as a startup optimization.
- Benchmark tooling directly depends on the existing `chakra-workspace` and
  workspace-managed `blake3` crates to exercise and verify the production
  store. No new production dependency is introduced by this decision.
- The complete payload remains an internal, versioned cache format rather
  than a portable release artifact contract.
- Issue #162 corrects the discovered loss of the typed `oversized` reason
  when the MessagePack writer reaches its bound; it does not change the
  measured no-go.

## Validation / follow-up

- Two release runs on each pinned Rust/PHP corpus target and hermetic fixture
  tests are recorded in `docs/evaluation/v0.3.0-shared-indexes.md`.
- The harness verifies compatibility lookup, bounded production decode,
  BLAKE3 transport integrity, and exact cold/local/prebuilt graph summaries.
- Reconsider only with a materially different representation or restore
  architecture and rerun the fixed gates before adding product import code.
