# Chakra

Chakra is a local Code Intelligence Layer for AI coding agents. It exposes
compact, current, structured, provenance-aware facts about a Git worktree over
MCP — repository structure, symbols, references, callers, source context, and
Git diff state — so agents can navigate a codebase with fewer blind searches.

Status: **v0.1 in progress**. The current slice provides Git-aware Rust
discovery, a Tree-sitter syntax index, bounded live filesystem reconciliation,
deterministic fresh-query barriers, atomic in-memory revisions, and MCP tools
for `status`, `repo_map`, literal/regex `search`, and `symbol_search`. Precise
rust-analyzer enrichment and Git diff context land in later slices.

## Build and run

Requires [rustup](https://rustup.rs/). The pinned toolchain (`1.97.1`, Edition
2024, resolver 3) is selected automatically via `rust-toolchain.toml`.

```sh
cargo build
cargo run -- --help        # or: ./target/debug/chakra --help
cargo run -- serve         # index and serve the current Git worktree
```

### Connect an agent (Codex example)

```toml
# ~/.codex/config.toml
[mcp_servers.chakra]
command = "/absolute/path/to/chakra"
args = ["serve", "--repo", "/absolute/path/to/repository"]
```

Logging goes to stderr (`RUST_LOG=debug` for more); stdout carries only the
MCP protocol stream.

## Validation gates

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check           # requires cargo-deny
```

## Repository layout

- `crates/chakra-cli` — user-facing `chakra` binary.
- `crates/chakra-domain` — core types and MCP-independent query contracts.
- `crates/chakra-engine` — in-memory symbol graph, atomic revision
  publication, `QueryService` implementation.
- `crates/chakra-language-rust` — Git-aware Tree-sitter Rust discovery,
  parsing, incremental syntax extraction, live watching, and reconciliation
  adapter.
- `crates/chakra-mcp` — MCP stdio adapter (`rmcp`) exposing the current
  typed query tools over domain contracts.
- `fixtures/rust/controller-service-provider` — Controller → Service →
  Provider fixture crate (test oracle; excluded from the workspace).
- `docs/SPEC.md` — architectural source of truth.
- `docs/roadmap/v0.1.md` — v0.1 scope authority.
- `docs/adr/` — architectural decision records.
- `.agents/skills/` — mandatory agent-driven review/validation/commit workflows.

`AGENTS.md` defines the mandatory operating rules, including the pre-commit
skill workflow.

## License

Not yet chosen; all crates are currently `publish = false`.
