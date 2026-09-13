# ADR-0053: Shared project configuration with typed precedence and trust boundaries

Status: accepted
Date: 2026-09-11

## Context

Issue #206 (milestone v0.4.0): every agent client currently receives Chakra's
provider and indexing arguments as a separate copy inside its own MCP
registration. Users maintaining four clients keep four drifting copies of the
same flags. The release requires one typed, versioned, portable project
configuration shared by every client, with deterministic precedence and an
explicit trust boundary for execution-sensitive settings.

The configuration surface must not grow Chakra's feature set: it re-exposes
only the existing `serve` CLI options for provider enablement, indexing limits,
provider-pool limits, and startup budgets. Domain, query, engine, and provider
crates must not depend on TOML or agent-client types; parsing and merging live
at the CLI boundary, which passes typed options inward (AGENTS.md invariants 5
and 10).

## Decision

### Files and ownership

- `chakra.toml` — the **shared** project configuration, committed to the
  repository. It must be portable across contributors, operating systems,
  agent clients, and linked worktrees of the same project. It may contain only
  portable settings: indexing limits, provider enablement, provider-pool and
  readiness budgets, and startup limits.
- `chakra.local.toml` — the **private** project override, placed next to
  `chakra.toml` and covered by `.gitignore`. It may additionally contain
  machine-specific provider executable paths. It is the only file-based layer
  that may select executables.
- Explicit CLI options remain the final override and the only way to pass a
  one-off value without a file.

A per-user global configuration is deliberately out of scope for v0.4.0;
`chakra.local.toml` plus CLI options cover machine-specific needs. The schema
is versioned so a user layer can be added without breaking changes.

### Schema and compatibility

- The document carries an explicit `schema_version = 1`. Unknown schema
  versions, unknown keys, invalid types or ranges, malformed TOML, and
  unreadable files are hard errors naming the file and key. A partially parsed
  configuration is never applied.
- Compatibility follows ADR-0043: additive optional keys may ship in patch
  releases; removing or retyping a key requires a schema-version bump and
  release-notes migration guidance.
- Configuration is applied once at process startup. Changing configuration
  requires restarting the server; v0.4.0 has no hot reload and no live graph
  reconfiguration.

### Discovery and the project boundary

- Without `--config`, the shared file is discovered at the root of the Git
  worktree that contains the resolved primary `--repo` path, using
  `chakra-git` worktree-root resolution (never a hand-rolled `.git` walk, per
  AGENTS.md invariant 7). The lookup does not climb above the worktree root,
  so a nested invocation cannot accidentally inherit an unrelated parent
  project's configuration. Linked worktrees each read their own checked-out
  `chakra.toml`; the private override is resolved as `chakra.local.toml` in
  the same worktree root.
- `--config PATH` selects the shared file explicitly and disables discovery;
  the private override is then the sibling `chakra.local.toml` of `PATH`.
- Relative paths inside configuration are resolved against the directory of
  the file that declares them, never against the process working directory.
- A missing `chakra.toml` is not an error: behavior is byte-identical to the
  pre-configuration CLI defaults.

### Precedence

From lowest to highest, per key:

1. built-in defaults (the existing clap default values),
2. shared `chakra.toml`,
3. private `chakra.local.toml`,
4. explicit CLI options.

To make layer 4 exact, `serve` arguments become `Option`s at the clap
boundary: an absent CLI flag contributes nothing, so parser defaults can never
override a configured value. The merged result is exposed by
`chakra config show` with the source layer of every key.

### Trust boundaries

- The shared file is executable-blind: a `path` key for any provider in
  `chakra.toml` is a validation error directing the user to
  `chakra.local.toml` or the CLI. A repository clone therefore cannot select
  an executable through committed configuration.
- Configuration discovery, parsing, merging, and `config show` never execute
  or shell out to project files. The schema has no command, hook, or script
  keys, and none may be added without a new ADR.
- Future network-affecting settings (for example the #204 update check) may
  live only in the private layer or CLI, so committed project defaults cannot
  re-enable a check a user opted out of.

### Agent-client integration

Issue #205's explicit setup generates a minimal commented `chakra.toml` when
none exists and registers every supported client against the same resolved
settings. Setup never overwrites an existing valid configuration, and repeated
setup is idempotent. Effective settings are intentionally identical across
clients because clients no longer carry Chakra flags at all.

## Alternatives considered

- **One file with a `[local]` table.** Rejected: a committed file containing
  machine paths leaks contributor environments and invites accidental commits;
  separate files make the trust boundary visible to `.gitignore`.
- **Clap default-value merging as-is.** Rejected: clap defaults are
  indistinguishable from explicit flags, so configuration could not override a
  default without breaking explicit-CLI wins. The `Option` boundary is the
  deterministic resolution.
- **Parent-directory search above the worktree root.** Rejected: silent
  inheritance from an unrelated enclosing project violates the project
  boundary requirement; the Git worktree root is the documented base.
- **Hot reload via the existing live watcher.** Rejected for v0.4.0: budgets
  and pool limits are constructed at startup, and partial reconfiguration
  would violate atomic publication semantics.

## Consequences

- `chakra-cli` gains a TOML parsing boundary (workspace-managed `toml` and
  existing `serde`) and a direct `chakra-git` dependency for worktree-root
  resolution. No other crate gains a configuration or TOML dependency.
- Every existing CLI invocation without configuration files behaves exactly as
  before; the conformance and provider gates are unaffected.
- `chakra config show` is the single diagnostics surface for effective
  configuration; MCP query responses do not repeat it.
- #205 (client setup) consumes the minimal-template and preservation contract
  defined here instead of inventing per-client flags.

## Validation / follow-up

- Hermetic tests cover precedence order, defaults-versus-explicit-CLI,
  shared/private separation, portable-path resolution, nested-directory and
  linked-worktree discovery, per-worktree isolation, and every rejected input
  class (unknown key, bad schema version, bad type, malformed TOML, unreadable
  file, executable path in the shared file).
- README, CLI help, an example configuration, and release notes are updated
  with the precedence and trust rules.

## Addendum 2026-09-13: release-audit hardening (issues #212–#216)

The v0.4.0 release audit found five defects in the initial implementation;
this addendum records the enforced behavior.

- **Tracked private overrides are rejected (#212).** The trust boundary is
  only real if the private file stays out of version control. When the
  private override lives inside a Git worktree, Chakra asks Git whether the
  file is tracked and fails startup if it is: a committed repository must
  never select provider executables. Outside a Git worktree there is no
  tracking to enforce and the file is the user's own.
- **Workspace-scoped settings are per-worktree (#213).** Index budgets and
  the live startup timeout resolve from each registered worktree's own
  checked-out `chakra.toml` (plus its private sibling and CLI overrides), as
  this ADR's discovery section always required. Process-global settings —
  provider-pool limits, provider enablement and executable paths, and the
  registry workspace limit — come from the primary worktree's configuration;
  per-worktree provider policies remain deferred. An explicit `--config`
  disables discovery and applies to every registered worktree.
- **`--config` is absolutized (#214).** The explicit path is made absolute
  without resolving symlinks, so relative provider paths always anchor at
  the declaring file's real directory, never the process working directory.
- **Only regular files are read, with a size cap (#215).** Configuration
  metadata is checked before `open`; FIFOs, devices, sockets, and
  directories are rejected so they cannot block startup, and reads are
  capped at 1 MiB enforced during the read.
- **Dangling links are hard errors (#216).** File presence is
  symlink-metadata based: a broken `chakra.toml` or `chakra.local.toml`
  symlink fails startup instead of silently restoring built-in defaults.
