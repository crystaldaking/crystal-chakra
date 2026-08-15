# Chakra

Chakra is a local Code Intelligence Layer for AI coding agents. It exposes
compact, current, structured, provenance-aware facts about a Git worktree over
MCP — repository structure, symbols, references, callers, source context, and
Git diff state — so agents can navigate a codebase with fewer blind searches.

Status: **v0.1 in progress**. Domain contracts, the in-memory engine with
atomic revision publication, and an MCP stdio skeleton (`status` tool)
exist; syntax indexing and the remaining tools land in later phases.

## Build and run

Requires [rustup](https://rustup.rs/). The pinned toolchain (`1.97.1`, Edition
2024, resolver 3) is selected automatically via `rust-toolchain.toml`.

```sh
cargo build
cargo run -- --help        # or: ./target/debug/chakra --help
cargo run -- serve         # serve MCP over stdio for the current directory
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
- `crates/chakra-mcp` — MCP stdio adapter (`rmcp`), currently the `status`
  tool over the domain contracts.
- `fixtures/rust/controller-service-provider` — Controller → Service →
  Provider fixture crate (test oracle; excluded from the workspace).
- Further v0.1 crates (`chakra-language-rust`) are added when they carry
  real code, per `docs/roadmap/v0.1.md` §14.
- `docs/SPEC.md` — architectural source of truth.
- `docs/roadmap/v0.1.md` — v0.1 scope authority.
- `docs/adr/` — architectural decision records.
- `.agents/skills/` — mandatory agent-driven review/validation/commit workflows.
- `prompts/` — implementation phase prompts.

`AGENTS.md` defines the mandatory operating rules, including the pre-commit
skill workflow.

## License

Not yet chosen; all crates are currently `publish = false`.
