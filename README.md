# Chakra

Chakra is a local Code Intelligence Layer for AI coding agents. It exposes
compact, current, structured, provenance-aware facts about one materialized
Git worktree over MCP: repository structure, Rust symbols, syntax call
candidates, source context, related tests, and current Git diff state.

Status: **v0.1 evaluation candidate**. The implemented slice is Rust-only and
provides Git-aware discovery, Tree-sitter syntax indexing, bounded live
filesystem reconciliation, deterministic fresh-query barriers, atomic
in-memory revisions, optional rust-analyzer call-hierarchy enrichment, and all
seven v0.1 MCP tools:

- `status`
- `repo_map`
- `search`
- `symbol_search`
- `context`
- `callers`
- `diff_context`

## Requirements

- Git available on `PATH`.
- [rustup](https://rustup.rs/) for building from source. The repository pins
  Rust `1.97.1` and Edition 2024 in `rust-toolchain.toml`.
- Optional: `rust-analyzer` on `PATH`. If it is unavailable or unhealthy,
  Chakra continues serving current syntax facts and reports the provider as
  degraded instead of inventing precise results.

No API key, external AI service, embedding service, database, or telemetry
service is required.

## Install

From a clone of this repository:

```sh
cargo install --locked --path crates/chakra-cli
chakra --version
```

For development, build the workspace binary in place:

```sh
cargo build --release
./target/release/chakra --help
```

The installed package is named `chakra-cli`; the user-facing executable is
always `chakra`.

## Run

`chakra serve` is a stdio MCP server. Normally the MCP client starts and owns
the process:

```sh
chakra serve --repo /absolute/path/to/a/git-worktree
```

Logging goes to stderr (`RUST_LOG=debug` for more detail). Stdout is reserved
for the MCP protocol stream.

## Connect Codex

The current Codex CLI registers a local stdio server with:

```sh
codex mcp add chakra -- /absolute/path/to/chakra serve --repo /absolute/path/to/repository
codex mcp list
```

Codex CLI and the ChatGPT desktop app share MCP configuration on the same
Codex host. In the desktop app, the equivalent path is **Settings → MCP
servers → Add server**, followed by a restart. See the
[official OpenAI MCP setup](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).

For explicit timeout/configuration control, use `~/.codex/config.toml` or a
trusted project's `.codex/config.toml`:

```toml
[mcp_servers.chakra]
command = "/absolute/path/to/chakra"
args = ["serve", "--repo", "/absolute/path/to/repository"]
startup_timeout_sec = 60
tool_timeout_sec = 60
```

After connecting, `status` should report a published revision and fresh syntax
state. Useful starting calls are:

```json
{"query":"refund","limit":20}
```

for `symbol_search`, then:

```json
{"symbol":{"by_name":"service::payment_service::PaymentService::refund"},"limit":20}
```

for `context` or `callers`. Use the returned `id` together with its `revision`
when a name is ambiguous. For current changes:

```json
{"limit":20}
```

with `diff_context` summarizes changed Rust files, current declarations in
those files, and bounded related callers/tests.

## Freshness, bounds, and cancellation

Syntax queries require fresh state by default. A fresh call requests an
authoritative Git/filesystem reconciliation and waits on a deterministic
barrier; callers do not need to sleep after editing. One immutable workspace
revision is pinned for the complete response. rust-analyzer data is accepted
as precise only for that same revision; otherwise current syntax facts are
returned with `catching_up` or `degraded` provider metadata.

Collection limits default to 20 and are capped at 500. Search patterns are
capped at 1,024 characters, returned match lines at 512 characters, and source
snippets at 20 lines / 4,096 characters. Every cut response sets `truncated`.
Potentially expensive MCP queries share two execution slots. Cancellation
before dispatch removes queued work; an already-dispatched synchronous query
finishes inside that bound. Timed-out rust-analyzer requests send
`$/cancelRequest`, and all provider waits have fixed deadlines.

## Git diff scope

`diff_context` compares `HEAD` with the final materialized worktree for indexed
regular Rust files:

- staged and unstaged tracked edits are combined; final worktree content wins;
- untracked, non-ignored Rust files are included;
- deleted tracked Rust files are reported by their former path;
- Git-detected staged renames carry `previous_path` and heuristic precision;
- an unstaged move remains delete plus add when Git cannot prove a rename;
- ignored files, `target/`, non-Rust files, and skipped symlinks are excluded.

Changed-symbol mapping in v0.1 is deliberately file-level: current
declarations in a changed file are marked `declared_in_changed_file` with
heuristic precision. Chakra does not claim that each declaration overlaps a
changed hunk, and deleted historical declarations are not reconstructed.

## Validation and measurements

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check

# Optional real-provider smoke when rust-analyzer is installed:
cargo test -p chakra-provider-rust-analyzer --test real_provider -- --ignored --nocapture
```

The reproducible v0.1 measurement entry points and the latest recorded local
run are in [docs/evaluation/v0.1-readiness.md](docs/evaluation/v0.1-readiness.md).
Use [docs/evaluation/v0.1-template.md](docs/evaluation/v0.1-template.md) for
real agent comparisons before expanding scope.

## Repository layout

- `crates/chakra-cli` — user-facing `chakra` binary.
- `crates/chakra-domain` — core types and MCP-independent query contracts.
- `crates/chakra-engine` — in-memory graph, atomic revisions, query layer.
- `crates/chakra-git` — typed current-worktree diff adapter.
- `crates/chakra-language-rust` — Git-aware Tree-sitter indexing, watching,
  and reconciliation.
- `crates/chakra-mcp` — thin stdio MCP adapter.
- `crates/chakra-provider-rust-analyzer` — optional precise provider adapter.
- `fixtures/rust/controller-service-provider` — integration fixture/test oracle.
- `docs/SPEC.md` — architectural source of truth.
- `docs/roadmap/v0.1.md` — v0.1 scope authority.
- `docs/adr/` — accepted architectural decisions.

## Known v0.1 limits

v0.1 supports one repository, one active materialized worktree, Rust only,
and an in-memory index rebuilt at startup. It intentionally has no historical
commit materialization, persistent graph snapshots, provider pool, eager
precise call graph, semantic/vector search, PHP support, or web UI.

## License

Not yet chosen; all crates are currently `publish = false`.
