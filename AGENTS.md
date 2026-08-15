# AGENTS.md — Chakra

## Mission

Chakra is a local Code Intelligence Layer for AI coding agents. It must provide current, structured, provenance-aware facts about a Git worktree without becoming an autonomous coding agent, IDE, RAG wrapper, or general shell executor.

Repository name: `crystal-chakra`.
Product name: `Chakra`.
User-facing binary: `chakra`.
Internal Rust crates use the `chakra-*` prefix.

## Source of truth

Before substantial work, read the minimum relevant documentation:

1. `docs/SPEC.md` — architectural source of truth and long-term direction.
2. `docs/roadmap/v0.1.md` — scope authority for v0.1. If SPEC and v0.1 differ on whether something must be implemented now, v0.1 wins on scope while SPEC wins on architecture.
3. Relevant ADRs in `docs/adr/` when touching an architectural decision.
4. The closest nested `AGENTS.md` if one exists.

Do not read the entire documentation tree by default. Load only what the task needs.

## Core architectural invariants

1. Git objects plus the materialized filesystem/worktree are canonical source of truth.
2. Queries must only observe atomically published workspace revisions.
3. A normal file change must never trigger a whole-repository reindex.
4. Precise, syntax-derived, heuristic, and textual results must be distinguishable.
5. MCP is an adapter. Domain and query layers must not depend on MCP protocol types.
6. Language tooling is an adapter. Core architecture must not depend on rust-analyzer or any single provider.
7. Do not hardcode `.git` administrative layout. Resolve Git paths through Git-aware mechanisms.
8. No external AI, embedding, or analytics service is required for core code intelligence.
9. Do not add arbitrary command execution to the MCP surface.
10. Keep public APIs small, typed, and explicit.
11. Do not introduce cache complexity before benchmarks justify it.
12. v0.1 must not silently grow into the full SPEC.

## Snapshot semantics

Do not treat a commit snapshot as a precise LSP index.

The long-term model is:

`EffectiveWorkspaceGraph = CommitSnapshot + WorktreeOverlay + WorkspaceEnrichment`

Where:

- `CommitSnapshot` is materialization-independent and contains offline-indexable facts such as file inventory, hashes, Tree-sitter-derived symbols/relations, and Git metadata.
- `WorktreeOverlay` contains changes relative to the base commit.
- `WorkspaceEnrichment` is materialization-dependent and may contain precise facts from rust-analyzer or another live language provider.

Precise provider data is not an intrinsic property of an arbitrary Git commit.

## v0.1 scope discipline

For v0.1, prefer the smallest useful implementation that proves agent value:

- Rust only.
- One repository and one active materialized worktree.
- Tree-sitter syntax intelligence.
- rust-analyzer precise enrichment on demand.
- Git diff awareness.
- Filesystem watcher and fresh query semantics.
- In-memory graph.
- MCP access.
- `repo_map`, `search`, `symbol_search`, `context`, `callers`, `diff_context`.

Explicitly deferred unless the task is specifically about a later milestone:

- PHP.
- Multi-worktree orchestration.
- Historical commit materialization.
- Persistent graph snapshots and restoration.
- Cross-repository graph.
- Semantic/vector search.
- Full eager precise call graph.
- Distributed indexing.
- Web UI.

## Rust engineering rules

- Use the pinned stable Rust toolchain and Edition 2024.
- Prefer strong domain types, newtypes, enums, explicit ownership, and typed errors.
- Keep crate and module boundaries meaningful; avoid god crates and god modules.
- Avoid `unwrap`, `expect`, and `panic!` in recoverable production paths.
- `unsafe` is forbidden unless a dedicated ADR and safety justification explicitly permit a narrowly scoped exception.
- Do not block the Tokio executor with CPU-heavy parsing; use controlled blocking/worker execution where needed.
- Every background task must have an owner, cancellation path, shutdown behavior, and observable error handling.
- Prefer bounded queues over unbounded channels in long-running pipelines.
- Do not add a dependency merely to avoid writing a small amount of straightforward code. Do not reimplement mature protocol or Git functionality without a reason.

## Dependency changes

Before adding a production dependency:

1. State what problem it solves.
2. Check maintenance and current API/documentation when the choice is time-sensitive.
3. Consider compile cost, transitive dependency count, security, license, and API stability.
4. Prefer workspace-managed versions.

A dependency change must be mentioned in the task closeout.

## Testing expectations

Changes must be tested at the narrowest useful level, then at repository level.

Prefer real temporary Git repositories for Git/worktree behavior.
Prefer provider contract tests and fixtures over tests that depend on a developer-global language server installation.

Do not claim a test passed unless it was actually executed in the current worktree.

## Git workflow

Git history is part of the project from the first implementation step.

Before editing:

- Inspect `git status --short` and current branch.
- Preserve unrelated user changes.
- Never discard, reset, clean, or overwrite unrelated work without explicit instruction.

During work:

- Keep changes cohesive.
- Do not use `git add .` or `git add -A` blindly.
- Stage only files that belong to the intended commit.
- Prefer small semantic commits at meaningful phase boundaries, not a commit after every tiny edit.
- Do not rewrite published history unless explicitly asked.

## Mandatory pre-commit workflow

Before **every** `git commit`, the model must perform these steps itself. This is mandatory even though there are intentionally no Git hooks enforcing it.

1. Invoke `$chakra-self-review` and resolve all blocking findings.
2. If the change touches architecture, crate boundaries, graph semantics, identity, consistency, persistence, language-provider contracts, MCP contracts, Git model, or concurrency model, invoke `$chakra-architecture-review` and resolve blocking findings.
3. Invoke `$chakra-validate` and run the relevant validation commands.
4. Inspect `git diff --check`.
5. Inspect the exact staged patch with `git diff --cached`.
6. Confirm staged files contain one coherent change and no unrelated edits.
7. Invoke `$chakra-commit` to create the commit.

A failed review or validation means: do not commit yet. Fix the issue, rerun the relevant skill, then continue.

Do not replace this process with pre-commit hooks. The workflow is intentionally agent-driven.

## Self-review expectations

Self-review must look for more than formatting. It must check, where relevant:

- architecture and dependency direction;
- scope creep beyond `docs/roadmap/v0.1.md`;
- stale or partially published graph state;
- accidental full reindex paths;
- provenance/precision correctness;
- read-your-writes behavior;
- races and unbounded work;
- leaked LSP/MCP/storage types across boundaries;
- unnecessary allocations or clones in hot paths;
- missing negative/error tests;
- orphaned tasks or child processes;
- unsafe Git/path handling;
- misleading documentation or claims.

## Commit messages

Use concise Conventional Commit-style subjects when practical:

- `feat: ...`
- `fix: ...`
- `refactor: ...`
- `test: ...`
- `docs: ...`
- `chore: ...`

Do not create meaningless commits such as `wip`, `changes`, `fix stuff`, or `update`.

## Completion behavior

Do not stop after scaffolding if the task requests a working phase.

At task completion report:

- what changed;
- architectural decisions made;
- validation actually run and its results;
- commits created;
- known limitations or deferred items.
