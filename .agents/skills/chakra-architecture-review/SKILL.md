---
name: chakra-architecture-review
description: Architecture conformance review, mandatory when a change touches architecture, crate boundaries, graph semantics, identity, consistency, persistence, language-provider contracts, MCP contracts, the Git model, or the concurrency model. Validates against SPEC, the v0.1 roadmap, and ADRs.
---

# Chakra Architecture Review

Run this before commit when the change touches any of:

- architecture or crate boundaries;
- graph semantics, identity, consistency, or persistence;
- language-provider contracts;
- MCP contracts;
- the Git model;
- the concurrency model.

## References

- `docs/SPEC.md` — architectural source of truth.
- `docs/roadmap/v0.1.md` — scope authority; wins over SPEC on "implement now" questions.
- `docs/adr/` — existing decisions; a conflicting change needs a new or amended ADR.

## Checks

1. **Layering** — domain and query layers free of MCP/LSP/storage types; MCP and language tooling remain adapters.
2. **Canonical state** — Git objects plus the materialized worktree remain the source of truth; derived state is validated against it, never trusted blindly.
3. **Revision model** — the change preserves atomic publication of workspace revisions.
4. **Snapshot semantics** — commit snapshots are not treated as precise LSP indexes; `CommitSnapshot + WorktreeOverlay + WorkspaceEnrichment` separation is preserved.
5. **Identity** — symbol/file/repository identity rules are not silently changed.
6. **Concurrency** — background work has owners, cancellation, bounded queues; the Tokio executor is not blocked by CPU-heavy work.
7. **Scope** — the change stays within the Rust/PHP v0.1 slice and does not pull deferred items (multi-worktree, persistence, cross-repo, embeddings, web UI) into v0.1.
8. **Surface** — public APIs stay small, typed, explicit; no arbitrary command execution added to the MCP surface.
9. **ADR need** — if this is a durable architectural decision or reverses one, an ADR in `docs/adr/` must accompany it.

## Output

- **Blocking findings** with the SPEC/roadmap/ADR section each violates.
- **Notes** for acceptable trade-offs that should be recorded.

No blocking findings means the change is architecturally clear to proceed.
