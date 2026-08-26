# ADR-0043: Pre-1.0 compatibility policy

Status: accepted
Date: 2026-08-22

## Context

Chakra is pre-1.0 and its public surface is still converging: the MCP tool
names and response envelopes, the `status` data shape, the support-matrix
schema, and the CLI flags all change as design errors are found (for example,
issue #61 corrects workspace-global provider-pool metrics being repeated per
provider in `status`). The primary consumers of these contracts are AI coding
agents, which have no compiler to catch a breaking change: an agent silently
receives a different response shape and may draw wrong conclusions from it.

The project needs an explicit rule for when breaking changes are acceptable
before 1.0.0, and what must accompany them, so that "pre-1.0" does not become
an excuse for undocumented drift.

## Decision

- Until the `v1.0.0` tag, backward-incompatible changes to any public
  contract (MCP tool names and envelope/schema shapes, CLI flags, support
  matrix and corpus result schemas, documented response semantics) are allowed
  when they correct a design error or remove misleading structure. They are
  not a vehicle for gratuitous churn.
- Every breaking change must be:
  1. recorded in `CHANGELOG.md` under an explicit `Breaking` heading for the
     release that carries it;
  2. covered by updated contract/conformance tests that pin the new shape;
  3. reflected in schema/version documentation in the same change.
- Patch releases (`0.x.Y`) prefer non-breaking fixes; a breaking change in a
  patch release requires an explicit note in the issue and changelog entry
  explaining why it cannot wait for the next minor boundary.
- Semantic invariants are never "broken" under this policy, at any version:
  provenance/precision honesty, atomic revision publication, no
  whole-repository reindex on an ordinary edit, adapter dependency direction,
  and bounded degradation. Renaming a field is a legal break; changing what
  `precise` means is not.
- Response envelopes remain self-describing where feasible (typed kind
  fields, explicit truncation/precision markers) so agents can detect a
  changed shape at runtime rather than trust a stale assumption.

## Alternatives considered

- **Anything-goes SemVer-0.** Legal but invites silent drift; agent consumers
  cannot compensate for undocumented changes.
- **Full stability before 1.0.** Rejected: it would freeze known design
  errors (e.g. #61) and force premature abstraction to work around them.
- **Deprecation windows with dual shapes pre-1.0.** Rejected as
  disproportionate for a pre-1.0 single-worktree tool with no external
  stability promise; the changelog plus contract tests carry the migration
  story instead.

## Consequences

- Issue #61 may change the `status` response shape within the v0.1.3 line,
  provided the changelog, contract tests, and schema documentation move
  together. v0.1.3 publishes that new shape as response schema 14 rather than
  reusing v0.1.2's schema 13.
- Reviewers and agents gain a checklist for any contract change: changelog
  `Breaking` entry, contract tests, schema docs — or the change does not
  merge.
- At `v1.0.0` this policy is revisited; the expectation is ordinary SemVer
  from that tag onward.

## Validation / follow-up

- First applied to the v0.1.3 milestone (issues #61, #82, #108).
- Release review checklist in `CONTRIBUTING.md` already requires comparing
  schema-version claims with the final implementation; this ADR adds the
  `Breaking` changelog heading to that comparison.
