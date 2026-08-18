# Chakra

Chakra is a local Code Intelligence Layer for AI coding agents. It exposes
compact, current, structured, provenance-aware facts about one materialized
Git worktree over MCP: repository structure, Rust and PHP symbols, syntax call
candidates, source context, related tests, and current Git diff state.

Status: **v0.1 evaluation candidate**. The implemented slice supports Rust and
PHP syntax intelligence and provides Git-aware discovery, Tree-sitter indexing, bounded live
filesystem reconciliation, deterministic fresh-query barriers, atomic
in-memory revisions, optional Rust-only rust-analyzer call-hierarchy enrichment, and all
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

A PHP runtime or Composer installation is not required for indexing. PHP v0.1
facts are syntax-derived through the official Tree-sitter PHP grammar.

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
for the MCP protocol stream. Use `--no-rust-analyzer` for deterministic
syntax-only operation, or `--rust-analyzer-path /absolute/path/to/rust-analyzer`
when the provider is not on `PATH`. Chakra does not start rust-analyzer for a
workspace with no indexed Rust sources.

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
state. Its provider list also reports provider languages and capabilities, so
the Rust-only precise enrichment boundary is machine-visible. Useful starting
calls are:

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

with `diff_context` summarizes changed Rust/PHP files, current declarations in
those files, and bounded related callers/tests/call candidates.

`context`, `callers`, and `diff_context` keep uniquely resolved syntax calls in
their ordinary relationship collections. Ambiguous or unresolved Tree-sitter
evidence is returned separately as `syntax_call_candidates`,
`syntax_candidates`, or `related_call_candidates`; Chakra does not connect an
unknown receiver to every same-name method. `status` reports total, ambiguous,
and unresolved call-site counts for the published revision.

All seven MCP tools advertise read-only, non-destructive, idempotent,
closed-world annotations. Current Codex clients can therefore use Chakra's
queries under their normal read-tool policy without treating them as writes.

## Freshness, bounds, and cancellation

Syntax queries require fresh state by default. A fresh call requests an
authoritative Git/filesystem reconciliation and waits on a deterministic
barrier; callers do not need to sleep after editing. One immutable workspace
revision is pinned for the complete response. rust-analyzer is queried only
for Rust symbols and its data is accepted as precise only for that same
revision; otherwise current syntax facts are
returned with `catching_up` or `degraded` provider metadata.
Optional precision has its own one-second wait budget inside the 30-second MCP
deadline. `context.data.provider` and `callers.data.provider` explain whether a
syntax fallback was used, the current provider stage, whether that stage came
directly from rust-analyzer or was inferred by Chakra, and the wait budget.
`status.data.providers[].metrics` reports document-delta traffic and the
entry/byte-bounded precise cache.

Provider startup no longer opens every Rust file. A precise query opens the
selected target from its pinned snapshot; unchanged callers remain disk-backed.
Later revisions send full text only for documents Chakra already opened and
use watched-file events for other exact create/change/delete deltas. Provider
quiescence, a post-sync request barrier, and a final workspace revision check
must all hold before precise facts are accepted. A 1,929-file/55.3 MB contract
test sends 19 bytes of target text on first use rather than the complete corpus.

Collection limits default to 20 and are capped at 500. Search patterns are
capped at 1,024 characters, returned match lines at 512 characters, and source
snippets at 20 lines / 4,096 characters. A complete serialized MCP query
response is capped at 1 MiB. Every semantic collection cut sets `truncated`;
an over-budget serialized response is rejected with a request to lower the
limit without emitting it or constructing a full serialized buffer.
Potentially expensive MCP queries share two execution slots. Queueing is
bounded to five seconds and execution to a 30-second end-to-end deadline.
Cancellation before dispatch removes queued work; cancellation after dispatch
cooperatively interrupts freshness, graph traversal, Git, and optional provider
work while the request retains its slot through cleanup. Timed-out or cancelled
rust-analyzer requests send `$/cancelRequest`. `status.data.query_execution`
reports queued/running work, outcomes, and permit hold time without itself
entering the expensive-query pool.

Indexing defaults to 100,000 Git-discovered Rust/PHP files, 8 MiB per source,
128 MiB total source, 500,000 symbols, 1,000,000 relationships, and 1,000,000
compact call sites. Cold-start and phase-sampled resident-memory targets default
to 120 seconds and 2 GiB. All limits are configurable with `chakra serve
--help`, validated against hard safety ceilings, and reused by live updates.

Count/byte limits are enforced before graph allocation. When one is reached,
Chakra publishes an internally consistent fresh-but-`degraded` revision where
possible: files, text search, and retained declarations remain queryable. Every
query envelope reports exact indexing budgets, corpus coverage, capability
completeness, affected capabilities, omission cause, phase measurements, and
best-effort memory samples. Schema v3 also reports graph-publication facts
reused, rebuilt, or copied in the current revision. Ordinary edits use
persistent file-owned graph deltas and shallow Rust/PHP composition, so old
snapshot readers remain immutable without a second complete combined-graph
copy. Calls are never resolved against a truncated symbol catalog. Time/RSS
targets are observable warnings rather than nondeterministic inputs to graph
contents.

Git discovery/diff subprocesses retain bounded output and have a 30-second
local deadline that can only be shortened by the query deadline. Cancellation
kills and reaps the owned child and joins its bounded pipe readers. Initial and
live indexing also support cooperative cancellation between file/phase units;
private cancelled candidates are never published.

## Git diff scope

`diff_context` compares `HEAD` with the final materialized worktree for indexed
regular Rust and PHP files:

- staged and unstaged tracked edits are combined; final worktree content wins;
- untracked, non-ignored Rust/PHP files are included;
- deleted tracked Rust/PHP files are reported by their former path;
- Git-detected staged renames carry `previous_path` and heuristic precision;
- an unstaged move remains delete plus add when Git cannot prove a rename;
- ignored files, `target/`, unsupported-language files, and skipped symlinks are excluded.

Changed-symbol mapping in v0.1 is deliberately file-level: current
declarations in a changed file are marked `declared_in_changed_file` with
heuristic precision. Chakra does not claim that each declaration overlaps a
changed hunk, and deleted historical declarations are not reconstructed.

## Validation and measurements

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo test --locked --manifest-path fixtures/rust/controller-service-provider/Cargo.toml
RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --no-deps
cargo deny check

# Optional real-provider smoke when rust-analyzer is installed:
cargo test -p chakra-provider-rust-analyzer --test real_provider -- --ignored --nocapture
```

The reproducible v0.1 measurement entry points and the latest recorded local
run are in [docs/evaluation/v0.1-readiness.md](docs/evaluation/v0.1-readiness.md).
The Zed/`psp-app` bounded-indexing results are in
[docs/evaluation/v0.1.1-indexing-budgets.md](docs/evaluation/v0.1.1-indexing-budgets.md).
The warmed deterministic freshness measurements are in
[docs/evaluation/v0.1.1-freshness-reconciliation.md](docs/evaluation/v0.1.1-freshness-reconciliation.md).
The provider-readiness contract and Zed-scale transport measurements are in
[docs/evaluation/v0.1.1-rust-analyzer-readiness.md](docs/evaluation/v0.1.1-rust-analyzer-readiness.md).
Use [docs/evaluation/v0.1-template.md](docs/evaluation/v0.1-template.md) for
real agent comparisons before expanding scope.

## Contributing and release flow

See [CONTRIBUTING.md](CONTRIBUTING.md). The annotated `v0.1.0` tag is the
mandatory Gitflow boundary: released code lives on `main`, next-release
integration lives on `develop`, and all later work goes through topic,
release, or hotfix branches and pull requests. Direct post-v0.1.0 commits to
`main` or `develop` are prohibited by project policy.

## Repository layout

- `crates/chakra-cli` — user-facing `chakra` binary.
- `crates/chakra-domain` — core types and MCP-independent query contracts.
- `crates/chakra-engine` — in-memory graph, atomic revisions, query layer.
- `crates/chakra-git` — Git-aware source discovery and typed current-worktree diff adapter.
- `crates/chakra-language` — Rust/PHP graph composition, watching, and reconciliation.
- `crates/chakra-language-rust` — Tree-sitter Rust syntax adapter.
- `crates/chakra-language-php` — Tree-sitter PHP syntax adapter.
- `crates/chakra-mcp` — thin stdio MCP adapter.
- `crates/chakra-provider-rust-analyzer` — optional precise provider adapter.
- `fixtures/rust/controller-service-provider` — integration fixture/test oracle.
- `fixtures/php/controller-service-provider` — PHP integration fixture/test oracle.
- `docs/SPEC.md` — architectural source of truth.
- `docs/roadmap/v0.1.md` — v0.1 scope authority.
- `docs/adr/` — accepted architectural decisions.

Production startup derives repository identity from Git root objects rather
than the checkout path or remote URL. Linked worktrees share a repository id
and receive distinct workspace ids; an unborn repository uses the filesystem
identity of Git's reported common administrative directory.

## Known v0.1 limits

v0.1 supports one repository, one active materialized worktree, Rust and PHP,
and an in-memory index rebuilt at startup. It intentionally has no historical
commit materialization, persistent graph snapshots, provider pool, eager
precise call graph, semantic/vector search, precise PHP provider, or web UI.
Rust module qualification follows conventional `src/foo.rs`, `foo/mod.rs`,
and inline-module layouts; custom external module remapping through `#[path]`
is not modeled in v0.1.
PHP namespace aliases and runtime type resolution are not modeled: PHP calls,
inheritance, and test relations remain explicitly syntax/heuristic facts.
Provider activation is decided at startup; after adding the first Rust file to
an already running PHP-only workspace, restart Chakra to enable precise Rust
enrichment. Live Rust syntax intelligence does not require that restart.

## License

Chakra is licensed under the [MIT License](LICENSE).
