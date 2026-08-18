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

Do not pre-create empty ADRs merely to satisfy a list.
