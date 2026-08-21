# ADR-0028: Cross-language conformance harness and scenario manifest

Status: accepted
Date: 2026-08-18

## Context

Issue #24 requires proving language parity with one reusable test oracle
instead of unrelated per-language happy-path fixtures. The parity contract
(ADR-0026) makes `CONFORM-01` mandatory for first-class status and the support
matrix checker consumes per-language conformance result files.

Before this change, Rust and PHP each had their own fixture tests with
asymmetric coverage; nothing asserted the same scenarios against both, nothing
emitted machine-readable per-language results, and provenance was not asserted
explicitly per scenario.

## Decision

Add `crates/chakra-conformance`, a tooling crate (not a runtime dependency of
shipped crates) with three parts:

1. **Scenario catalog** — a fixed set of scenarios implemented once in Rust
   over the public `QueryService` surface: declarations/containers,
   imports/aliases, source roles, ambiguity, syntax callers, test hints,
   text search, bounded responses, syntax-error recovery, file lifecycle
   (create/modify/atomic-save/rename/delete), diff-context scopes,
   provider-absent degradation, provider crash/recovery (via an in-process
   `PreciseProvider` double — real language servers are never required), and
   high-degree callers.
2. **Per-language manifest** — `fixtures/conformance/<language>/manifest.json`
   declares the same scenario ids plus the language-specific expectations
   (qualified names, paths, counts). Adding a language means adding a fixture
   tree and a manifest; the harness discovers it without code changes and
   fails if a manifest omits or invents scenario ids.
3. **Deterministic result emission** — `chakra-conformance emit <dir>` writes
   `docs/support/conformance/<language>.json` with per-scenario pass/fail,
   contract capability ids, and the provenance assertions actually performed.
   Emission is byte-identical across runs (fixed ordering, no timestamps), so
   CI can diff a fresh emission against the committed files.

Synchronization uses the existing live freshness barrier and `RequireFresh`
queries — never sleeps. Provenance/precision is asserted explicitly in every
scenario (PROV-01 evidence).

CI gains a `conformance` job: `cargo test -p chakra-conformance` (regression
gate) plus a diff of a fresh emission against committed results.

## Alternatives considered

- **Per-language test files without a shared manifest** (status quo) —
  rejected: that is exactly the "unrelated happy-path fixtures" the issue
  forbids; expectations drift per language.
- **Harness inside chakra-engine tests** — rejected: the harness is product
  tooling spanning several crates; a dedicated crate keeps engine tests
  focused and keeps fixture/git wiring out of the engine's test surface.
- **Timestamps and environment data in result files** — rejected: breaks
  byte-identical re-emission; measurement data belongs to the corpus
  evaluation (#25), not the functional conformance gate.

## Consequences

- `CONFORM-01` in the Rust and PHP support manifests is now `pass` with
  `docs/support/conformance/<language>.json` as evidence.
- Every new language issue (#27–#36) must add a conformance fixture and
  manifest; the CI gate then covers it automatically.
- The provider double borrows `Provenance::RustAnalyzer` for precise facts
  because the domain enum has no generic precise-provider variant yet; new
  provider adapters (#27+) are expected to generalize the enum.
- Large-project degradation budgets are deliberately out of scope here and
  belong to the corpus evaluation (#25).

## Validation / follow-up

- `cargo test -p chakra-conformance` passes; rust and php each pass 14/14
  scenarios.
- `chakra-conformance emit` is byte-identical on re-run (verified locally and
  enforced in CI).
- #25 reuses this crate's fixtures/runner patterns for corpus evaluation.
