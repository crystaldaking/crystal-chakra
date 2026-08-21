---
name: chakra-validate
description: Run the project validation gates before every commit and report what actually executed. Never claim a gate passed without running it in the current worktree.
---

# Chakra Validate

Run the gates below in the repository root **in the current worktree**, and report the real result of each.

## Gates

When a Cargo workspace exists:

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. `cargo deny check` — required once `deny.toml` exists and `cargo-deny` is installed; if the tool is missing, say so explicitly instead of silently skipping.

Always, for every commit (including documentation-only ones):

5. `git diff --check` — no whitespace errors or conflict markers in the staged patch.

## Rules

- Run the gates that apply to the staged change. For a documentation-only commit before any Cargo workspace exists, state that gates 1–4 are not applicable and run gate 5.
- A failed gate blocks the commit: fix the issue, rerun the failed gate, then continue.
- Report each gate as `pass`, `fail`, or `not applicable`, with the exact command run. Never report `pass` for a gate that was not executed.

## Chakra dogfooding

When validating Chakra itself and a current Chakra MCP/binary is available:

1. Use `status` to record revision and coverage, and `diff_context` to inspect
   the intended patch. Use the other Chakra queries when they materially test
   the change. These checks supplement, never replace, the gates above.
2. If Chakra fails, is stale, omits expected facts, mislabels precision, or
   gives unusable output, capture the version/revision, exact input, expected
   result, actual result, and fallback evidence.
3. Search existing GitHub issues. When GitHub writes are authorized, create or
   update a reproducible issue; otherwise include an issue-ready report in the
   validation result. Continue with repository tools so dogfooding does not
   block the mandatory gates.
4. Close a reported issue only after its fix and current-worktree validation
   pass.
