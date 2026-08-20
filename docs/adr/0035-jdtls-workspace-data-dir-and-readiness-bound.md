# ADR-0035: jdtls workspace data directory and readiness bound

Status: accepted
Date: 2026-08-20

## Context

ADR-0027 selected jdtls as the Java precise provider and recorded its two
material costs: it needs a JDK 21+ runtime with a 1–2 GB JVM heap, and its
first project import on a cold workspace can take minutes. Two lifecycle
specifics remained open for the `chakra-provider-jdtls` adapter (#30):

1. jdtls requires a **per-workspace data directory** (`-data <dir>`) where it
   writes its project model and indexes. The directory is cache, not source:
   it must never land inside the repository (it would pollute the Git
   worktree Chakra treats as canonical), and two workspaces must never share
   one (jdtls keys its model by workspace).
2. The shared ADR-0032 readiness pattern proves readiness with a
   post-synchronization `prepareCallHierarchy` barrier bounded by the
   per-request timeout. That bound is sized for servers that answer in
   milliseconds; jdtls legitimately cannot answer call hierarchy until the
   project import completes, so a request-sized bound would flap the
   provider between `CatchingUp` and spurious timeouts during the first
   minutes of a cold start.

## Decision

- The data directory is `std::env::temp_dir()/chakra-jdtls-<hash>` where
  `<hash>` is FNV-1a (64-bit) over the workspace root path bytes — a
  deterministic, dependency-free key. The directory lives under the OS
  temporary directory, never inside the repository; the worker creates it
  (bounded, best-effort) before spawning and reports a transport error into
  the degradation path if it cannot.
- `JdtlsConfig::default` carries a workspace-bound command rather than a
  relative cache path; `JdtlsProvider::start` resolves it from the pinned
  workspace root. Explicit commands remain possible, but their `-data` path
  must be the only `-data` occurrence, must have a value, and must be absolute
  and outside the repository (including after resolving an existing symlink
  ancestor), otherwise startup returns a typed error before spawning a thread
  or process.
- Readiness keeps the ADR-0032 post-synchronization request barrier, but the
  prepare round-trips are bounded by a dedicated, configurable
  `readiness_timeout` (default 180 s) instead of the per-request
  `request_timeout` (default 10 s). Every query stays bounded end-to-end:
  the per-query wait budget (`query_wait_timeout`, default 1 s) still
  returns `CatchingUp` promptly while the worker keeps importing in the
  background, so callers never block on the JVM.
- Command discovery follows the provider pattern: an explicit configured
  path first, then `jdtls` on `PATH`, then `jdt-language-server` on `PATH`.
  Both launchers accept `-data`; no other flag is required. A missing JDK or
  server degrades the provider without failing Chakra startup
  (ADR-0006/0013).

## Consequences

- Repeated jdtls sessions for one workspace reuse the same data directory,
  so only the first-ever session pays the full import cost; deleting the
  tempdir entry is a safe cache reset.
- The resource profile recorded in ADR-0027 (JDK 21+, 1–2 GB heap, slow
  first import) is documented for operators in `docs/languages/java.md`;
  no heap flags are set by Chakra — operators tune the JVM through their own
  jdtls installation (`JDTLS_JAVA_OPTS` or launcher edits), keeping the
  adapter's command surface minimal.
- `Provenance::Jdtls` (`jdtls`, serde-compatible) labels precise Java facts,
  additive like `Vtsls`/`Pyright`; syntax-tier behavior is unchanged whether
  or not a JDK is present.
