---
name: chakra-self-review
description: Review staged/working changes before every commit. Checks architecture direction, v0.1 scope, correctness risks, and honesty of claims beyond formatting. Produces blocking findings that must be resolved before commit.
---

# Chakra Self-Review

Run this before **every** commit, after staging the intended files.

## Inputs

- `git diff --cached` (the exact staged patch) — if nothing is staged yet, review `git diff` plus untracked files intended for the commit.
- `git status --short` — confirm the staged set contains only files that belong to this change.

## Checklist

Review the patch against every applicable item. This is not a formatting pass.

1. **Architecture and dependency direction** — no domain/query layer depending on MCP, LSP, storage, or other adapter types; no new crate dependency that points the wrong way.
2. **v0.1 scope** — nothing beyond `docs/roadmap/v0.1.md`; no premature future crates, persistence, multi-worktree, or semantic search.
3. **Stale or partially published state** — updates are built privately and published atomically; no query can observe hybrid state.
4. **Accidental full-reindex paths** — a normal file change must not trigger whole-repository reindexing.
5. **Provenance/precision correctness** — textual/heuristic results never labeled `precise`; precision and provenance preserved end to end.
6. **Read-your-writes** — after an edit, fresh queries reflect the latest reconciled state.
7. **Races and unbounded work** — no unowned background tasks, no unbounded channels in long-running pipelines, cancellation and shutdown paths exist.
8. **Leaked adapter types** — no LSP/MCP/storage types in public domain or query APIs.
9. **Hot-path waste** — no unnecessary allocations or clones where measurements or structure suggest a hot path.
10. **Tests** — negative/error cases covered where relevant; claims about passing tests were actually executed in this worktree.
11. **Processes and tasks** — no orphaned child processes or tasks.
12. **Git/path safety** — no hardcoded `.git` administrative layout assumptions; paths resolved through Git-aware mechanisms; no shell string concatenation from repository-controlled input.
13. **Honest docs** — comments, README, and commit message describe what the code actually does; no unverified claims.

## Output

- **Blocking findings**: must be fixed and re-reviewed before commit.
- **Non-blocking notes**: may be deferred; say so explicitly.

If there are no blocking findings, state that the change is clear to proceed to validation.
