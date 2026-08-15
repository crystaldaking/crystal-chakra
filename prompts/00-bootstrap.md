# Prompt 00 — Bootstrap `crystal-chakra`

You are starting Chakra in an empty or documentation-only directory.

Read:

- `AGENTS.md`
- `docs/SPEC.md` sections 1–7, 30, 38–48
- `docs/roadmap/v0.1.md`
- `docs/IMPLEMENTATION_WORKFLOW.md`

## Goal

Create the real Git repository and minimal Rust project foundation without prematurely implementing the product.

## Required work

1. Ensure the directory/repository is named conceptually `crystal-chakra` and initialize Git if it is not already a Git repository.
2. Preserve the supplied documentation, prompts, and `.agents/skills` in version control.
3. Verify the current latest stable Rust release from official Rust sources. Use the repository baseline from SPEC unless a newer stable release exists at implementation time; if changing it, update the relevant docs consistently.
4. Create a pinned `rust-toolchain.toml`, Edition 2024 Cargo workspace with resolver 3, and the minimal crate structure needed for v0.1. Do not create empty future crates.
5. Ensure the user-facing binary is exactly `chakra`.
6. Establish workspace lint/dependency policy, formatting configuration, `cargo-deny`, `.gitignore`, license/readme placeholders as appropriate.
7. Add a minimal CLI where `chakra --help` works. Do not implement indexing yet.
8. Create CI configuration only if it is useful and straightforward; do not substitute CI for the model-driven pre-commit skills.
9. Run the project validation gates.
10. Before committing, follow the full mandatory skill workflow from `AGENTS.md`.
11. Create a focused initial Git commit (or a small coherent sequence if documentation import and Rust bootstrap are cleaner separately).

## Non-goals

- No Tree-sitter index yet.
- No rust-analyzer integration.
- No MCP business tools yet.
- No SQLite/persistent graph.
- No multi-worktree.

## Done when

- Git is initialized and clean after commit.
- Repository governance files and skills are committed.
- `chakra --help` works.
- Mandatory validation gates pass.
- Commit history is meaningful.
