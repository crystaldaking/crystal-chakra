# Chakra

[![CI](https://github.com/crystaldaking/crystal-chakra/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/crystaldaking/crystal-chakra/actions/workflows/ci.yml)
[![GitHub release](https://img.shields.io/github/v/release/crystaldaking/crystal-chakra)](https://github.com/crystaldaking/crystal-chakra/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Local, current code intelligence for AI coding agents.**

Chakra turns one materialized Git worktree into a compact, structured graph
that agents can query over MCP. It answers questions about repository shape,
symbols, callers, source context, tests, and current changes without uploading
code or requiring an AI, embedding, analytics, or database service.

Chakra is designed for the gap between text search and a full IDE:

- Git and the worktree remain the source of truth.
- Fresh queries see edits immediately, without an arbitrary client sleep.
- Every result identifies its workspace revision, freshness, provenance, and
  precision (`precise`, `syntax`, `heuristic`, or `textual`).
- Tree-sitter provides an offline baseline; optional language servers add
  revision-bound precise evidence and degrade safely when unavailable.
- Responses, queues, indexing, subprocesses, and background work are bounded.

## Language support

All supported languages have Git-aware discovery, Tree-sitter syntax facts,
live reconciliation, the shared query contract, conformance fixtures, and
pinned public-corpus evidence.

| Language | Offline syntax | Optional precise enrichment |
|---|---|---|
| Rust | Tree-sitter | rust-analyzer |
| PHP | Tree-sitter + deterministic Laravel facts | Chakra resolver |
| TypeScript / TSX | Tree-sitter | vtsls |
| JavaScript / JSX | Tree-sitter | vtsls |
| Python | Tree-sitter | pyright |
| Java | Tree-sitter | jdtls |
| C# | Tree-sitter | csharp-ls |
| Shell | Tree-sitter | bash-language-server references |
| C / C++ | Tree-sitter | clangd |
| HCL / Terraform | Tree-sitter | terraform-ls references |
| Go | Tree-sitter | gopls |

The generated [support matrix](docs/support/SUPPORT_MATRIX.md) records the
capability-level evidence. Language-specific behavior and honest limitations
live under [docs/languages](docs/languages/).

## Install

### Installer (recommended)

One command installs the latest stable release and puts `chakra` on your
PATH, after verifying the archive against the release `SHA256SUMS`
(issue #203):

```sh
curl -fsSL https://raw.githubusercontent.com/crystaldaking/crystal-chakra/main/tools/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/crystaldaking/crystal-chakra/main/tools/install.ps1 | iex
```

The installer covers Linux x86-64, macOS Apple silicon and Intel, and
Windows x86-64, and rejects other platforms before changing anything. It
installs into a user-owned directory (`~/.local/bin`,
`%LOCALAPPDATA%\Programs\chakra`) — no administrator rights needed.
Options: `--version vX.Y.Z` (explicit version; required for downgrades),
`--dir PATH`, `--no-path-modify`. PATH changes are idempotent (a managed
shell block on Unix, the user PATH on Windows) and take effect in a new
terminal; the installer never claims to modify the parent shell. Re-running
upgrades through the same verified download flow, and a failed download or
checksum check leaves any previous installation usable.

Removal: delete the installed binary (`chakra`/`chakra.exe`), remove the
`# >>> chakra path >>>` block from your shell startup file or the install
directory from the Windows user PATH, and re-run `chakra init --agent
<client> --remove` per project if you set up agent integration. Already
registered MCP clients pick up a replaced binary automatically because the
registration points at the install path, not a versioned file.

### Prebuilt release archives

[Chakra v0.4.0](https://github.com/crystaldaking/crystal-chakra/releases/tag/v0.4.0)
ships native archives for:

| Platform | Target | Archive |
|---|---|---|
| Linux x86-64 (glibc) | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| macOS Apple silicon | `aarch64-apple-darwin` | `.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | `.tar.gz` |
| Windows x86-64 | `x86_64-pc-windows-msvc` | `.zip` |

Archive names have the form `chakra-v0.4.0-<target>.<format>`. Each contains
the `chakra` executable, this README, and the MIT license in a versioned
directory. Download `SHA256SUMS` from the same release and verify the archive
before installing it. For example, on Linux:

```sh
version=v0.4.0
target=x86_64-unknown-linux-gnu
archive="chakra-${version}-${target}.tar.gz"
base="https://github.com/crystaldaking/crystal-chakra/releases/download/${version}"
curl -fLO "${base}/${archive}"
curl -fLO "${base}/SHA256SUMS"
grep " ${archive}$" SHA256SUMS | sha256sum --check -
tar -xzf "${archive}"
sudo install -m 0755 "chakra-${version}-${target}/chakra" /usr/local/bin/chakra
chakra --version
```

On macOS, choose `aarch64-apple-darwin` for Apple silicon or
`x86_64-apple-darwin` for Intel and verify with
`grep " ${archive}$" SHA256SUMS | shasum -a 256 --check -`. On Windows, compare
`(Get-FileHash <archive> -Algorithm SHA256).Hash` with the matching
`SHA256SUMS` entry, expand the zip, and put the directory containing
`chakra.exe` on `PATH`.

GitHub also publishes signed build-provenance attestations for every archive:

```sh
gh attestation verify "${archive}" --repo crystaldaking/crystal-chakra
```

The binaries are not yet platform code-signed or Apple-notarized. Use the
attestation to verify their GitHub Actions provenance, or build from source if
your local policy requires a signed executable.

### Build from source

Requirements:

- Git on `PATH`;
- [rustup](https://rustup.rs/) to build the pinned Rust 1.97.1 toolchain.

```sh
git clone https://github.com/crystaldaking/crystal-chakra.git
cd crystal-chakra
git checkout v0.4.0
cargo install --locked --path crates/chakra-cli
chakra --version
```

The Cargo package is `chakra-cli`; the executable is `chakra`. Optional
language servers are discovered only when their language route is activated.
They are not required for indexing or syntax queries, and Chakra never installs
them automatically.

For a deterministic syntax-only service, disable any provider with its
`--no-*` flag. Run `chakra serve --help` for executable paths, provider-pool
budgets, index budgets, and watcher startup controls.

## Quick start with an MCP client

Chakra is a stdio MCP server. The client normally owns its process:

```sh
chakra serve --repo /absolute/path/to/a/git-worktree
```

Stdout is reserved for MCP. Logs go to stderr and can be adjusted with
`RUST_LOG`.

With [Codex CLI](https://developers.openai.com/codex/mcp/):

```sh
codex mcp add chakra -- chakra serve --repo /absolute/path/to/repository
codex mcp list
```

Equivalent `~/.codex/config.toml` configuration:

```toml
[mcp_servers.chakra]
command = "/absolute/path/to/chakra"
args = ["serve", "--repo", "/absolute/path/to/repository"]
startup_timeout_sec = 60
tool_timeout_sec = 60
```

Start an agent session with `status`, then use `repo_map` to understand the
workspace before narrowing through symbols and relationships.

## Project configuration

A project can keep one shared, committed `chakra.toml` at its Git worktree
root instead of repeating Chakra flags in every client's MCP registration
(ADR-0053). Every supported `serve` option for provider enablement, indexing
limits, provider-pool limits, and startup budgets is configurable; see the
commented example at `docs/examples/chakra.toml`.

Configuration is loaded once at startup (restart to change it) and merges in
this order, per key:

1. built-in defaults,
2. the shared `chakra.toml` at the worktree root,
3. a private, non-committed `chakra.local.toml` next to it (add it to your
   project's `.gitignore`),
4. explicit `chakra serve` CLI options.

The shared file is portable: it is discovered at the Git worktree root (never
above it, so nested directories cannot inherit an unrelated project's
settings), and it cannot select provider executables — a `path` key there is
rejected. Machine-specific executable overrides belong to
`chakra.local.toml` or CLI flags; relative paths resolve against the
directory of the file that declares them. `--config PATH` selects the shared
file explicitly and takes its private sibling.

When several worktrees are registered, each one reads its own checked-out
`chakra.toml` for workspace-scoped settings (index budgets and the live
startup timeout); process-global settings (provider-pool limits, provider
enablement and paths, the workspace limit) come from the primary `--repo`
worktree, and an explicit `--config` applies to every registered worktree.

The private file must stay out of version control: if Git tracks
`chakra.local.toml`, startup fails, because a committed repository must never
select provider executables. Configuration files must be regular files (no
FIFOs or device links), at most 1 MiB, and a dangling configuration symlink
is an error — never a silent fallback to defaults.

Inspect the merged result and the source layer of every key without starting
a server:

```sh
chakra config show --repo /path/to/worktree
```

Invalid configuration — malformed TOML, unknown keys, an unsupported
`schema_version`, zero limits — fails startup with an actionable error naming
the file and key; a partially parsed configuration is never applied.

## Agent setup

One-time project setup registers Chakra as a local stdio MCP server and
installs a concise instruction block for a supported coding-agent client
(ADR-0055):

```sh
chakra init --agent claude          # also: codex, cursor, opencode
chakra init --agent codex --agent cursor   # name each client explicitly
chakra init --agent claude --dry-run       # preview every planned write
chakra init --agent claude --remove        # revert Chakra-owned setup
```

Setup is idempotent and preserves user work: it owns only the `chakra` MCP
entry, a delimited `<!-- chakra:begin/end -->` instruction block, and a
minimal `chakra.toml` created only when none exists. Conflicting
registrations and malformed blocks stop setup with an actionable message
instead of overwriting anything. See `docs/support/agent-clients.md` for
the pinned per-client formats and current evidence status, and run
`chakra doctor --agent <client>` to diagnose registration, instructions,
and project configuration.

`chakra doctor` also explains analysis quality (issue #207): per provider
it reports intentional syntax-only disablement, missing or misconfigured
executables with install guidance per `docs/languages/*.md`, missing
project metadata at the worktree root, and index-budget pressure from the
tracked source inventory — always labeled as an *isolated inspection*,
never as the agent's live session (dormant/catching-up/ready/degraded
states and live counters belong to the session's `status` tool). The
default run spawns no processes; `--probe` adds bounded `--version`
executions with hard deadlines and pinned-version compatibility findings,
and `--json` emits the versioned machine-readable document. Exit status
`1` marks broken setup; an intentionally disabled provider is healthy.

For reproducible issue reports, `chakra doctor --report <path>` writes a
bounded, sanitized JSON document (issue #208): findings, platform,
non-sensitive effective limits, provider readiness, and the HEAD revision,
built from an explicit allowlist — no source text, environment values,
credentials, remote URLs, or absolute machine paths — with unavailable
values marked as such and truncation recorded explicitly. The file is
written owner-only and is never uploaded; review it and attach it to a
GitHub issue manually. It describes observed health, not model behavior.

## Update checks

`chakra update --check` queries GitHub for the latest stable release and
prints the installed version, the newest version with its release notes, and
the supported upgrade path (ADR-0054). Exit status is part of the contract:
`0` up to date, `1` check unavailable, `2` update available with a matching
platform asset.

While `chakra serve` runs, one gated background check runs at most once per
24 hours per state directory (with failure backoff) and reports an available
update to stderr only — never to MCP clients, never blocking startup or
queries. Disable automatic checks completely with
`CHAKRA_UPDATE_CHECK=0` or `[update] automatic = false` in
`chakra.local.toml` (private-only, so a committed repository cannot
re-enable them). Checking never downloads or replaces the binary; upgrades
happen only through the explicit installer flow.

## MCP tools

| Tool | What it returns |
|---|---|
| `status` | Revision, freshness, coverage, diagnostics, budgets, provider state, and operational metrics |
| `repo_map` | A bounded structural overview and paginated file inventory |
| `search` | Bounded textual matches in captured source |
| `symbol_search` | Ranked, filterable declarations with stable revision-local identities; `match_mode: "exact"` reads only the exact-name index |
| `context` | One symbol, source excerpt, callers, callees, implementations, tests, and typed related facts |
| `callers` | Aggregated incoming relations plus unresolved syntax evidence |
| `diff_context` | Git/worktree changes joined with current symbols, callers, tests, and call candidates |

Names are never guessed away. If a name is ambiguous, use the `id` and
`revision` returned by `symbol_search`. All collections and source excerpts are
bounded; truncated responses say which section and budget caused the cut.
Successful tool calls return the same bounded JSON envelope in both typed
`structuredContent` and a backwards-compatible JSON `TextContent` block, so
clients consuming either MCP representation observe equivalent results.

For branch review, `diff_context` supports direct base and merge-base scopes:

```json
{"scope":{"kind":"merge_base","reference":"origin/develop"},"limit":50}
```

## Freshness and trust model

Filesystem notifications are hints, not truth. A fresh query reconciles a
Git-aware inventory and strong filesystem identities before publishing or
using a revision. An edit, atomic save, rename, deletion, or project-metadata
change is therefore visible without asking the client to wait and retry.

Syntax state is published atomically: a query sees the old complete revision
or the new complete revision, never a partially updated graph. Normal file
changes reparse affected files and relationship owners rather than rebuilding
the repository.

Optional providers are lazy, capacity-bounded adapters. Chakra synchronizes
source and project-input deltas, waits only within explicit budgets, and
accepts precise facts only when the provider result and materialized worktree
still match the pinned revision. Missing executables, crashes, timeouts,
cancellation, or saturation preserve current syntax results with an explicit
fallback reason.

## Architecture

```mermaid
flowchart LR
    GW[Git objects + worktree] --> D[Git-aware discovery]
    W[Watcher hints] --> R[Freshness reconciler]
    R --> D
    D --> TS[Language adapters / Tree-sitter]
    TS --> S[Atomic immutable workspace revision]
    S --> Q[Bounded query engine]
    P[Optional language providers] --> E[Revision-bound enrichment]
    E --> Q
    Q --> M[stdio MCP adapter]
    M --> A[AI coding agent]
```

MCP and language servers are adapters; domain and query layers do not depend
on their protocol types. The graph is in memory and rebuilt deterministically
at startup. See the [SPEC](docs/SPEC.md), [v0.1 roadmap](docs/roadmap/v0.1.md),
and [ADRs](docs/adr/) for the full contract and trade-offs.

## Evidence and validation

The shared conformance harness runs the same behavior catalog for every
language. A separate opt-in evaluation runs against 20 pinned public
repositories, including Kubernetes, VS Code, Kafka, Spring Boot, Django,
Symfony, Tokio, and the .NET runtime. Results and machine-readable artifacts
are in [docs/support/corpus](docs/support/corpus/).

Repository validation:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo deny check
```

CI also builds the release workspace, runs the generated multi-language scale
gate, verifies support artifacts, and exercises the native macOS watcher path.

## Project status and limits

Chakra v0.4.0 is the current stable release in the v0.x line. One process
serves one repository and a bounded set of isolated materialized worktrees.
Compatible commit snapshots are shared in process and may be restored from an
opt-in bounded local store; live provider enrichment remains tied to the
materialized worktree. Chakra deliberately does not provide historical commit
materialization, cross-repository graphs, semantic/vector search, prebuilt or
network snapshot import, an eager complete precise call graph, arbitrary
command execution, or a web UI.

Syntax intelligence is intentionally conservative. Dynamic dispatch, macros,
generated code, build-configuration selection, framework magic, and incomplete
provider project models can leave candidates unresolved or ambiguous; Chakra
reports that uncertainty instead of upgrading it silently.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) for the Gitflow, review, validation,
and release process. Architectural changes should start from the relevant ADR;
bugs and dogfooding findings belong in
[GitHub Issues](https://github.com/crystaldaking/crystal-chakra/issues) with the
query/input, workspace revision, expected result, actual result, and fallback
evidence needed to reproduce them.

Chakra is available under the [MIT License](LICENSE).
