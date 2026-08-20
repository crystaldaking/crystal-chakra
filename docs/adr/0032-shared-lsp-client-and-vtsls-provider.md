# ADR-0032: shared LSP client crate and vtsls precise provider

Status: accepted
Date: 2026-08-20

## Context

Issue #27 requires TypeScript to pass the full parity contract, including
precise definitions/references/callers (PRECISE-02) through an eligible local
provider. ADR-0027 selected vtsls (MIT, npm-pinnable, Node.js runtime) after
rejecting tsgo (preview-grade LSP) and keeping typescript-language-server as
fallback. ADR-0027 also anticipated that "a shared transport helper may
emerge": with up to eight provider adapters planned, duplicating JSON-RPC
stdio transport, readiness bounding, and lifecycle code per provider crate
would be the most error-prone option.

## Decision

- **`crates/chakra-lsp`** — a minimal, generic LSP stdio client: transport
  framing, request/response routing with `$/cancelRequest`, bounded
  initialize/initialized handshake, didOpen/didChange/didClose text sync,
  shutdown/exit with kill fallback and no orphan processes. Only provider
  adapter crates depend on it; no LSP types leak into domain/query layers
  (invariants 5, 6, 10).
- **`crates/chakra-provider-vtsls`** — `VtslsProvider` implements the
  engine's `PreciseProvider` over chakra-lsp: owned child process, bounded
  readiness (mirroring ADR-0013), revision-scoped document synchronization,
  precise definitions/references/callers converted with the new additive
  `Provenance::Vtsls` (`vtsls`, serde-compatible), cancellation, restart on
  crash, and shutdown without orphan node processes.
- **Discovery and degradation**: an explicit configured command wins;
  otherwise vtsls is resolved via node/npm on `PATH`. An absent or failing
  server degrades to syntax intelligence with explicit provenance — startup
  never fails hard (ADR-0006 semantics). The default test suite requires
  neither Node.js nor vtsls: lifecycle contract tests run against a fake
  stdio server, mirroring the rust-analyzer provider's test pattern.
- **Real-server verification is opt-in**: vtsls requires
  `initializationOptions.typescript.tsdk` (or an auto-detected workspace
  TypeScript); with a bare initialize it exits silently (verified 2026-08-20
  against `@vtsls/language-server` 0.3.0 + typescript scratch-installed under
  `target/`). With the tsdk option it advertises definition, references,
  callHierarchy, and workspaceSymbol — matching the ADR-0027 capability
  claims.

## Alternatives considered

- **Refactor chakra-provider-rust-analyzer onto chakra-lsp now** — rejected
  for churn: the rust-analyzer worker carries provider-specific readiness and
  revision-delta logic that is battle-tested; migration remains a future
  option once chakra-lsp proves itself across two or more providers.
- **Duplicate transport per provider crate** — rejected: eight copies of
  framing/lifecycle code is the worst maintenance outcome.
- **typescript-language-server instead of vtsls** — kept as fallback;
  vtsls wraps the official VS Code TypeScript extension and covers
  callHierarchy with less adaptation.
- **tsgo / @typescript/native-preview** — rejected (LSP "in progress");
  re-evaluate at TypeScript 7 GA per the contract §7 policy.

## Consequences

- TypeScript flips to `advertised: true` / first-class: PRECISE-02..05 now
  pass with the provider crate's contract tests as evidence.
- New provider adapters (#28 pyright, #30 jdtls, #31 csharp-ls, #33
  bash-language-server, #34 clangd, #35 terraform-ls, #36 gopls) build on
  chakra-lsp and the VtslsProvider template.
- Corpus evidence for TypeScript remains syntax-tier (providers are off by
  default in the corpus runner); provider-backed corpus scenarios are a
  possible follow-up.
- Each provider needs its runtime's initialization specifics discovered and
  recorded (as done here for the tsdk requirement).

## Validation / follow-up

- 25 contract tests across chakra-lsp and chakra-provider-vtsls pass with a
  fake server (handshake, readiness bound, sync, conversion provenance,
  cancellation, restart, orphan-free shutdown).
- Real vtsls 0.3.0 capability probe executed locally (see above); a
  `#[ignore]`d in-repo real-server test remains a follow-up.
- Follow-ups: optional rust-analyzer migration onto chakra-lsp; real-server
  fixture test; provider-backed corpus scenarios.
