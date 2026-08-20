# Architecture Decision Records

Use ADRs only for durable decisions whose alternatives and consequences matter to future contributors/agents.

Suggested format:

```markdown
# ADR-NNN: Title

Status: proposed | accepted | superseded
Date: YYYY-MM-DD

## Context

## Decision

## Alternatives considered

## Consequences

## Validation / follow-up
```

Accepted decisions:

- ADR-001: workspace revision publication model
- ADR-002: in-memory graph representation
- ADR-003: MCP transport and SDK
- ADR-004: Git-aware Rust syntax index
- ADR-005: live syntax reconciliation and freshness barriers
- ADR-006: optional rust-analyzer precise enrichment
- ADR-007: agent queries and Git diff scope
- ADR-008: PHP syntax support and multi-language composition
- ADR-009: Git-aware repository and worktree identity
- ADR-010: lazy syntax call-site resolution
- ADR-011: bounded indexing and deterministic degradation
- ADR-012: cooperative query deadlines and cancellation
- ADR-013: bounded rust-analyzer readiness and revision-delta synchronization
- ADR-014: bounded resource-aware syntax parsing
- ADR-015: receiver-aware PHP syntax call resolution
- ADR-016: deduplicated test relationships
- ADR-017: deterministic Laravel framework enrichment
- ADR-018: precise PHP provider evaluation and deferral
- ADR-019: Cargo-aware, language-neutral source roles
- ADR-020: bounded deterministic symbol search ranking
- ADR-021: revision-scoped repository-map pagination and structural overview
- ADR-022: revision-scoped actionable syntax diagnostics
- ADR-023: query truncation contract
- ADR-024: caller aggregation and byte-first response budgets
- ADR-025: query execution work budgets
- ADR-026: first-class language parity contract and generated support matrix
- ADR-027: syntax grammar and precise provider selection for target languages
- ADR-028: cross-language conformance harness and scenario manifest
- ADR-029: pinned public evaluation corpus and budgeted runner
- ADR-030: PHP precise-equivalent resolver through strict-tier promotion
- ADR-031: object-safe syntax language adapter trait and registry
- ADR-032: shared LSP client crate and vtsls precise provider
- ADR-033: slot-keyed revision-local entity-id partitions

Do not pre-create empty ADRs merely to satisfy a list.
