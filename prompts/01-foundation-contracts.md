# Prompt 01 — v0.1 Foundation & Contracts

Read:

- `AGENTS.md`
- all of `docs/roadmap/v0.1.md`
- `docs/SPEC.md` sections 1–11, 23–30, 34–36, 40–46

## Goal

Establish the smallest architecture and typed contracts needed for Chakra v0.1 without implementing future scope.

## Required work

1. Define core types for workspace identity, revision, source location/range, provenance, precision, provider state, and structured query envelopes.
2. Define query/application interfaces independent of MCP.
3. Define the minimal symbol/code-entity model needed by v0.1.
4. Choose and document the in-memory graph/index representation after comparing plausible options. Do not introduce persistence.
5. Define atomic published-revision ownership semantics and a testable update interface.
6. Create fixture infrastructure for a small Rust Controller → Service → Provider scenario with tests.
7. Write unit tests for types/invariants and a regression test proving queries cannot observe a partially published revision using the chosen design.
8. Add the smallest MCP transport skeleton necessary for typed tool exposure, but do not overbuild tools yet.
9. Create/update ADRs only for decisions actually made.
10. Run self-review, architecture review, validation, then create cohesive commits.

## Design constraints

- No MCP types in core query/domain code.
- No LSP types in core code.
- No global `Mutex<Everything>`.
- No speculative snapshot/persistence system.
- No PHP or multi-worktree.

## Done when

The project has a clean, tested foundation on which syntax indexing can be added without changing the fundamental query/revision model.
