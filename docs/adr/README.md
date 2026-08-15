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

Likely early ADRs, only when the decision is actually made:

- Cargo workspace boundaries
- workspace revision publication model
- Rust syntax graph representation
- rust-analyzer synchronization/freshness barrier
- MCP transport for v0.1

Do not pre-create empty ADRs merely to satisfy a list.
