# ADR-0026: First-class language parity contract and generated support matrix

Status: accepted
Date: 2026-08-18

## Context

SPEC §15 defines "first-class language support" as prose. Milestone v0.1.2
(issue #22) expands Chakra from Rust+PHP to eleven languages and requires one
testable product contract: no vague or tiered meaning of support, no language
marked first-class through documentation alone, and CI enforcement that an
advertised language actually passes every mandatory capability.

Existing parity between Rust and PHP is a structural convention (mirrored
adapter APIs, ADR-0008), not an enforced gate. Without a machine-readable
contract, each new language would renegotiate what "supported" means.

## Decision

Adopt `docs/language-parity-contract.md` as the single pass/fail contract for
first-class language support. It defines a catalog of capability identifiers
(discovery, syntax intelligence, precise provider, query/MCP contract,
consistency invariants, evidence) with mandatory and conditional levels.

Support status is machine-readable:

- per-language manifests in `docs/support/languages/<language>.json` conforming
  to `docs/support/matrix.schema.json`;
- `tools/check_support_matrix.py` (stdlib-only) validates manifests, requires
  existing evidence paths for every claimed `pass`/`equivalent`, refuses
  advertised status without conformance and corpus evidence, and regenerates
  `docs/support/matrix.json` and `docs/support/SUPPORT_MATRIX.md`;
- CI runs the checker in `--check` mode so stale or hand-edited matrix
  artifacts fail the build.

Provider eligibility (maintenance, capabilities, license, resource behavior,
lifecycle ownership, reproducible installation) is defined by the contract.
Where an eligible provider lacks a mandatory capability, we implement a
Chakra-owned equivalent tested to the same standard or leave the language
unadvertised.

The target language list is reconsidered at every minor release planning; the
last review date is recorded in the generated matrix.

## Alternatives considered

- **Prose support tiers in README** — rejected: untestable, drifts from
  reality, and the issue explicitly forbids first-class-by-documentation.
- **Rust-native checker crate** — rejected for now: the checker is a
  repository-maintenance tool, not product code; a stdlib Python script matches
  the existing `tools/evaluate_php_lsp.py` precedent and adds no compile cost.
- **Fold the matrix into the conformance harness (#24)** — rejected: the
  contract and its gate must exist before the harness so that harness output
  has a defined consumer and schema.

## Consequences

- `advertised: true` becomes impossible without `CONFORM-01` and `CORPUS-01`
  evidence, so no language can be claimed first-class until #24 and #25 land.
  Rust and PHP currently report `tier: in-progress`.
- Adding a capability to the contract requires updating every language
  manifest in the same change (the checker fails on unknown or missing ids).
- The checker hardcodes the capability id list; it must stay in sync with the
  contract document (noted in both files).

## Validation / follow-up

- `python3 tools/check_support_matrix.py --check` passes and is enforced in CI.
- #24 (conformance harness) must emit the per-language result files the
  checker consumes; #25 (public corpus) the corpus records.
- Language issues #27–#37 flip their manifest to `advertised: true` only with
  full evidence.
