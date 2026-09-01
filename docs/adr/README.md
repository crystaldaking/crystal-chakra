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
- ADR-034: CommonJS facts in the JavaScript syntax model
- ADR-035: bounded multi-provider orchestration
- ADR-036: jdtls workspace data directory and readiness bound
- ADR-037: csharp-ls workspace-only enrichment
- ADR-038: bash-language-server reference enrichment
- ADR-039: clangd workspace enrichment
- ADR-040: terraform-ls reference enrichment
- ADR-041: gopls workspace enrichment
- ADR-042: revision-bound provider input deltas
- ADR-043: pre-1.0 compatibility policy
- ADR-044: atomically published project model and dependency-scoped invalidation
- ADR-045: bounded live indexing diagnostics
- ADR-046: bounded priority scheduling for interactive freshness
- ADR-047: workspace-bound lazy file facts
- ADR-048: bounded multi-worktree registry and query routing
- ADR-049: worktree-bound instances in a global provider pool

Do not pre-create empty ADRs merely to satisfy a list.
