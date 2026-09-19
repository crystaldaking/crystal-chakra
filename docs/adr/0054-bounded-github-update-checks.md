# ADR-0054: Bounded GitHub update checks with offline-first operation

Status: accepted
Date: 2026-09-13

## Context

Issue #204 (milestone v0.4.0): users need to discover newer stable Chakra
releases through a manual CLI check and a bounded automatic notification,
while normal offline operation is preserved. Checking must never send
repository data, must not require authentication, and must never delay MCP
readiness or alter query behavior. AGENTS.md invariant 8 keeps core code
intelligence free of network requirements; ADR-0053 reserves
network-affecting settings for the private configuration layer or CLI.

## Decision

### Manual check

- `chakra update --check` queries the GitHub Releases API for
  `crystaldaking/crystal-chakra` and prints the installed version, the
  latest stable version, the release-notes URL, whether an asset exists for
  the current platform, and the supported upgrade path (the #203 installer).
- Exit statuses are part of the contract: `0` already current (including a
  locally newer or development build), `1` check unavailable or failed, `2`
  update available with a matching platform asset.
- The API's `releases/latest` endpoint is authoritative: it excludes drafts
  and prereleases by design. Tags are parsed as `vMAJOR.MINOR.PATCH`;
  anything else is malformed and reported, never guessed. Comparison is
  numeric per component, so `0.4.10` sorts after `0.4.9`.

### Network adapter and bounds

- One blocking HTTPS GET via `ureq` (rustls; no OpenSSL platform coupling),
  executed on Tokio's blocking pool. The adapter lives in `chakra-cli`;
  domain, graph, query, and provider crates gain no network dependency.
- Bounds: 5 s total request deadline, 256 KiB response cap, at most 3
  redirects, no automatic retry for the manual check. A `User-Agent` of
  `chakra/<version>` is the only request identity; no repository paths,
  worktree data, or telemetry is sent.
- Offline operation, timeouts, DNS failures, HTTP errors, rate limits, and
  malformed or oversized responses are reported as a failed check and never
  fail normal Chakra operation.

### Automatic check

- `chakra serve` spawns at most one background update task per process,
  after startup completes. The task has an owner (the serve runtime), a 10 s
  overall deadline, cancellation on shutdown, and writes nothing to MCP
  stdout; an available update is reported through `tracing` on stderr only.
- The task is eligible at most once per 24 hours per state directory,
  measured from the persisted last-attempt timestamp; consecutive failures
  apply exponential backoff capped at 4x. The attempt timestamp is written
  atomically (temporary file + rename) before the request so concurrent
  process starts cannot storm the API; a rare duplicate request inside the
  race window is accepted and bounded by one request per process.
- Automatic checks are disabled completely — zero requests — by the
  environment variable `CHAKRA_UPDATE_CHECK=0|false|off` or by
  `[update] automatic = false` in the private `chakra.local.toml`. Following
  ADR-0053, the `[update]` table is rejected in the shared `chakra.toml`:
  a committed repository must not re-enable a check the user opted out of.
  The manual check always works regardless of these switches.
- The state file lives under `$CHAKRA_STATE_DIR`, else the platform state
  directory (`$XDG_STATE_HOME/chakra` or `~/.local/state/chakra`,
  `~/Library/Application Support/chakra`, `%LOCALAPPDATA%\chakra`). An
  unavailable state directory disables the automatic check silently; the
  manual check never touches it.

## Alternatives considered

- **`reqwest` as the HTTP client.** Rejected: larger compile and transitive
  cost for one bounded GET; `ureq` with rustls covers the need.
- **`semver` crate for ordering.** Rejected: release tags are a strict
  `vX.Y.Z` triplet by project policy; a thirty-line parser with explicit
  malformed-tag handling is simpler than a dependency and the prerelease
  exclusion is already done by the API endpoint.
- **Checking on every CLI invocation.** Rejected: subcommands must stay
  instant and offline-safe; only the long-running `serve` performs the
  background check, and only inside the 24-hour gate.
- **A `dirs`/`directories` crate for the state path.** Rejected: three
  platform branches of straightforward environment lookups do not justify a
  dependency.

## Consequences

- `chakra-cli` gains workspace-managed `ureq` (rustls, no default features
  beyond TLS) as its only network dependency; `cargo deny` licensing is
  validated in CI.
- Configuration schema v1 gains the additive optional private-only
  `[update] automatic` key (ADR-0043 additive change; no version bump).
- Core indexing, queries, and MCP behavior are byte-identical offline and
  online; the automatic check is inert unless `serve` runs with network
  access and the gate open.

## Validation / follow-up

- Hermetic tests cover version ordering (including 0.4.9 vs 0.4.10),
  malformed and prerelease tags, a local HTTP stub for available/current/
  error/oversized/redirect responses, interval and backoff persistence,
  concurrent-start coordination, opt-out with zero stub hits, and unchanged
  MCP stdout.
- README and CLI help document the command, exit statuses, automatic-check
  behavior, opt-out, and the explicit upgrade path through the #203
  installer.
