# Chakra

Chakra is a local Code Intelligence Layer for AI coding agents. It exposes
compact, current, structured, provenance-aware facts about a Git worktree over
MCP — repository structure, symbols, references, callers, source context, and
Git diff state — so agents can navigate a codebase with fewer blind searches.

Status: **v0.1 bootstrap**. The workspace, toolchain pin, and `chakra` CLI
skeleton exist; indexing and MCP serving are implemented in later phases.

## Build and run

Requires [rustup](https://rustup.rs/). The pinned toolchain (`1.97.1`, Edition
2024, resolver 3) is selected automatically via `rust-toolchain.toml`.

```sh
cargo build
cargo run -- --help        # or: ./target/debug/chakra --help
```

## Validation gates

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check           # requires cargo-deny
```

## Repository layout

- `crates/chakra-cli` — user-facing `chakra` binary (only crate so far; further
  v0.1 crates such as `chakra-domain`, `chakra-engine`, `chakra-language-rust`,
  and `chakra-mcp` are added when they carry real code, per
  `docs/roadmap/v0.1.md` §14).
- `docs/SPEC.md` — architectural source of truth.
- `docs/roadmap/v0.1.md` — v0.1 scope authority.
- `docs/adr/` — architectural decision records.
- `.agents/skills/` — mandatory agent-driven review/validation/commit workflows.
- `prompts/` — implementation phase prompts.

`AGENTS.md` defines the mandatory operating rules, including the pre-commit
skill workflow.

## License

Not yet chosen; all crates are currently `publish = false`.
