# ADR-0037: csharp-ls workspace-only enrichment

Status: accepted
Date: 2026-08-20
Last reviewed: 2026-09-04

## Context

ADR-0027 selected `csharp-ls` as the optional precise C# provider. The
adapter still needs an explicit runtime and result-boundary contract:

- upstream `csharp-ls` 0.26.0 requires the .NET 10 SDK;
- its normal executable speaks LSP over stdio without a mode flag;
- opt-in metadata URIs can expose decompiled framework/dependency source via
  `csharp:/...` locations that are not part of the pinned Git worktree;
- Chakra precise facts must remain revision-scoped and provenance-aware, and
  an optional runtime must never be required for syntax intelligence.

## Decision

- Chakra invokes `csharp-ls` directly, with no default arguments. An explicit
  `--csharp-ls-path` overrides side-effect-free `PATH` discovery.
- The CLI registers the provider as a dormant C# route in the bounded
  provider pool. It reserves 1 GiB when activated and otherwise consumes no
  provider process or reservation.
- Chakra does not enable `csharp-ls` metadata URIs. Provider relations are
  accepted only when both declaration and call-site locations map back into
  a captured repository document. External/decompiled locations are dropped
  before response limits are applied.
- `csharp-ls` exposes method names as display signatures such as
  `MethodA(string)`. The shared call-hierarchy driver first restricts prepare
  results to the captured file and syntax declaration range, prefers one exact
  symbol name, and otherwise accepts one `name(...)` display signature.
  Multiple compatible overload candidates are never guessed: that query
  keeps syntax facts and reports fallback while the synchronized provider
  session remains healthy for subsequent queries.
- The adapter uses the shared `chakra-lsp` client, owns one process group,
  synchronizes only C# documents by revision, and requires the server to
  advertise call hierarchy before reporting readiness.
- Missing executable, missing capability, crash, timeout, and cancellation
  degrade to the syntax tier. None may fail initial Chakra indexing.
- The documented optional runtime is `csharp-ls` 0.26.x with .NET 10 SDK or
  later. Core `.cs` discovery and Tree-sitter indexing require neither.

## Consequences

- Precise results remain inside the same atomically published workspace
  envelope as syntax facts; Chakra does not present arbitrary SDK/dependency
  source as if it belonged to the repository.
- Users who need decompiled metadata navigation should use their editor or a
  future explicitly modeled external-source contract. Enabling metadata URIs
  silently would violate current Git/worktree identity semantics.
- Hermetic fake-server tests cover signature-bearing method names, ambiguous
  overload fallback with subsequent recovery, lifecycle, delta
  synchronization, cancellation, restart, capability gating, and orphan-free
  shutdown. A real server remains an opt-in operator smoke test.
- Source-document and C# project-input changes produce revision-bound
  watched-file events. `.csproj`, `.sln`, `.slnx`, `Directory.Build.*`, and
  `global.json` identities are published atomically with the syntax graph and
  synchronized without fabricating text-document changes (ADR-0042).

## Alternatives considered

- **Enable metadata URIs by default.** Rejected because `csharp:/` locations
  have no repository-relative identity or captured source in the current
  graph model.
- **Bundle or install the .NET tool automatically.** Rejected: provider
  installation is an operator action; discovery must be side-effect free.
- **Use OmniSharp.** Rejected by ADR-0027 because it is in maintenance mode
  and does not supply the required call-hierarchy contract.

## Validation / follow-up

- `crates/chakra-provider-csharp-ls/tests/lifecycle.rs` uses a hermetic peer
  that emits `target()`/`caller()` display names and an ambiguous
  `target()`/`target(int)` pair at the same declaration. The ambiguous query
  falls back locally; the next query through the same process is precise and
  the shared provider state remains `ready`.
- `crates/chakra-provider-csharp-ls/tests/real_provider.rs` passed on
  2026-09-04 against csharp-ls 0.26.0 (`b714e27`) on the official .NET SDK
  10.0.400 ARM64 macOS runtime: `MethodA(string)` resolved one precise
  `MethodB(string)` incoming caller in 1.88 seconds.
