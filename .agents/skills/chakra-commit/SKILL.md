---
name: chakra-commit
description: Final commit gate. Runs only after self-review, architecture review (when triggered), and validation have passed. Verifies the staged patch is one coherent change and creates the commit with a Conventional Commit-style message.
---

# Chakra Commit

Run this last, only after:

1. `$chakra-self-review` — no blocking findings;
2. `$chakra-architecture-review` — when triggered, no blocking findings;
3. `$chakra-validate` — all applicable gates pass.

## Steps

1. `git status --short` — confirm the staged set contains exactly the files of this change. Stage explicit paths only; never `git add .` or `git add -A`. Preserve unrelated user changes.
2. `git diff --check` — no whitespace errors.
3. `git diff --cached` — read the exact staged patch end to end. Confirm it is one coherent change with no unrelated edits, no leftover debug code, and no secrets.
4. Write the message: concise Conventional Commit-style subject (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`), imperative, no `wip`/`changes`-style placeholders. Add a short body only when the why is not obvious from the diff.
5. `git commit` with that message.
6. Report the created commit hash and subject.

## Never

- rewrite published history (`rebase`, `reset --hard`, `--amend` on pushed commits) without explicit instruction;
- commit automatically just because a file changed — commit only at a coherent, validated boundary;
- commit with failing review or validation results outstanding.

## Chakra dogfooding

When committing Chakra itself and a current Chakra MCP/binary is available:

1. Use `status` to record revision and coverage and `diff_context` to compare
   the staged intent with Chakra's view. Git's exact staged patch remains the
   commit authority.
2. If Chakra fails, is stale, omits expected facts, mislabels precision, or
   gives unusable output, capture the version/revision, exact input, expected
   result, actual result, and fallback evidence.
3. Search existing GitHub issues. When GitHub writes are authorized, create or
   update a reproducible issue; otherwise include an issue-ready report in the
   commit closeout. Continue with Git so dogfooding does not block a validated
   commit.
4. Close a reported issue only after its fix and current-worktree validation
   pass.
