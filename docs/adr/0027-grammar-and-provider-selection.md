# ADR-0027: Syntax grammar and precise provider selection for target languages

Status: accepted
Date: 2026-08-18

## Context

Milestone v0.1.2 adds first-class support for TypeScript, JavaScript, Python,
Java, C#, Shell, C++, HCL, and Go, and keeps Rust and PHP at the same contract
(issues #23, #27–#37). The parity contract (ADR-0026) requires every language
to name a maintained Tree-sitter grammar and an eligible local precise
provider — or record a gap that becomes explicit implementation work.

Selection criteria per the contract §5: maintenance, capabilities (definition,
references, call hierarchy, workspace symbols, synchronization, cancellation),
license, resource behavior, lifecycle ownership, reproducible installation.
Evidence was collected 2026-08-18 from upstream repositories, crates.io, and
capability probes (`tools/probe_language_server.py`).

## Decision

### Grammar selection (all compatible with tree-sitter 0.26, ABI 13–15)

| Language | Crate | Version | License | Caveat |
|----------|-------|---------|---------|--------|
| Rust | tree-sitter-rust | 0.24 (current) | MIT | bump to 0.24.2 is trivial |
| PHP | tree-sitter-php | 0.24.2 (current) | MIT | status quo |
| TypeScript | tree-sitter-typescript | 0.23.2 | MIT | split grammar: `LANGUAGE_TYPESCRIPT` + `LANGUAGE_TSX`; links tree-sitter-javascript as base; crate lags repo |
| JavaScript | tree-sitter-javascript | 0.25.0 | MIT | base grammar for TS/TSX |
| Python | tree-sitter-python | 0.25.0 | MIT | — |
| Java | tree-sitter-java | 0.23.5 | MIT | crate lags repo |
| C# | tree-sitter-c-sharp | 0.23.5 | MIT | — |
| Shell | tree-sitter-bash | 0.25.1 | MIT | — |
| C++ | tree-sitter-cpp | 0.23.4 | MIT | inherits C grammar; crate lags repo |
| HCL | tree-sitter-hcl | 1.1.0 | Apache-2.0 | community `tree-sitter-grammars` org, not core org |
| Go | tree-sitter-go | 0.25.0 | MIT | — |

Grammar crates lagging their upstream repos by 6–18 months (TypeScript, Java,
C++) are accepted: the grammars are stable, and the lag is recorded here as a
supply-chain observation to revisit during corpus evaluation.

### Precise provider selection

| Language | Provider (pinned) | License | Install / pin | Gaps |
|----------|-------------------|---------|---------------|------|
| Rust | rust-analyzer (status quo, re-verified) | Apache-2.0/MIT | rustup component | none |
| TypeScript/JavaScript | vtsls (`@vtsls/language-server` 0.3.x) | MIT | npm, exact-version pinnable; needs Node.js | none |
| Python | pyright (1.1.4xx) | MIT | npm or pip, pinnable; needs Node.js | none |
| Java | jdtls (1.60.x milestone) | EPL-2.0 | milestone tarball from download.eclipse.org; needs JDK 21+ | slow startup, JVM heap 1–2 GB; budgets must reflect this |
| C# | csharp-ls (0.26.x) | MIT | `dotnet tool install --global csharp-ls --version …`; needs .NET 10 SDK+ | decompiled-source definition needs opt-in metadata-uris; Chakra keeps these disabled (ADR-0037) |
| Shell | bash-language-server (5.6.x) | MIT | npm, pinnable; needs Node.js | **no callHierarchy anywhere** → Chakra-owned equivalent (Tree-sitter-derived function-call edges); explainshell hover needs network — keep disabled offline |
| C++ | clangd (LLVM 21+/22.x) | Apache-2.0 w/ LLVM exception | OS package / brew / llvm.org installer | needs `compile_commands.json`; degrade gracefully without it; large-tree indexing uses GBs RAM |
| HCL | terraform-ls (0.39.x) | MPL-2.0 | GitHub releases / brew | **no callHierarchy/typeHierarchy** → Chakra-owned reference graph; HCL "callers" are resource references, derivable from Tree-sitter facts |
| Go | gopls (0.23.x) | BSD-3-Clause | `go install golang.org/x/tools/gopls@vX.Y.Z` | none |
| PHP | none (deferral reaffirmed) | — | — | see ADR-0018; #32 requires a trustworthy precise layer or Chakra-owned equivalent |

### Rejected alternatives

- **typescript-language-server** — acceptable fallback for vtsls; thinner
  wrapper over tsserver with fewer features.
- **tsgo / @typescript/native-preview** — rejected for now: the LSP mode is
  explicitly "in progress" (pull-diagnostics only). Re-evaluate at TypeScript
  7 GA.
- **pylsp** — no callHierarchy. **basedpyright** — unnecessary fork-divergence
  risk over pyright. **ty (Astral)** — pre-1.0 with incomplete type-check
  coverage; best distribution story (single Rust binary) — re-evaluate at 1.0.
- **OmniSharp** — maintenance mode, no callHierarchy. **Roslyn LSP** — capable
  but distribution is tied to VS Code extension builds; worse standalone
  pinning than csharp-ls.

### Probe and test policy

- `tools/probe_language_server.py` (stdlib-only, opt-in) verifies a server's
  advertised capabilities and exits non-zero when a required capability is
  missing. Verified in this worktree against rust-analyzer 1.97.1 and clangd
  (Apple clangd 21.0.0), both advertising definition, references,
  callHierarchy, and workspaceSymbol.
- The default test suite must not require globally installed servers:
  provider contract tests use recorded or stub transports; real-server probes
  stay opt-in (the `real_provider` gating pattern already used by
  chakra-provider-rust-analyzer).
- Missing provider capabilities become explicit implementation work tracked in
  the language issues: Chakra-owned call edges for Shell (#33) and HCL (#35),
  graceful degradation without `compile_commands.json` for C++ (#34), a
  precise layer or equivalent for PHP (#32).

### Adapter locality

Each provider integration is a separate `chakra-provider-<name>` adapter crate
implementing the engine's `PreciseProvider` trait. LSP protocol types must not
leak into domain/query crates (invariants 5, 6, 10). External runtimes
(Node.js, JDK, .NET SDK, LLVM, Go toolchain) are per-language install
requirements documented per language issue, never bundled into the core.

## Alternatives considered

- **One generic LSP client crate for all languages** — rejected: provider
  lifecycle, readiness, and degradation differ materially (jdtls import cost
  vs. gopls); a shared *transport* helper may emerge during #27/#28
  implementation, but each provider keeps its own adapter crate.
- **Dropping precise providers for Shell/HCL entirely** — rejected: the
  contract forbids lowering the tier; bash-language-server + a Chakra-owned
  call graph meets it.
- **Deferring grammar selection to each language issue** — rejected: the
  issue asks for one evidence-backed decision record; per-language issues
  execute it and may reopen individual rows with new evidence.

## Consequences

- Nine new Tree-sitter grammar dependencies and up to eight provider adapter
  crates enter the workspace over #27–#37; each addition re-runs the
  dependency review per AGENTS.md.
- The parity manifests (`docs/support/languages/*.json`) reference this ADR
  for `grammar.adr` / `precise_provider.adr`.
- tsgo and ty are scheduled re-evaluation candidates at their GA/1.0
  milestones (contract §7 review policy).

## Validation / follow-up

- Probe tool executed successfully against rust-analyzer, clangd, and
  terraform-ls 0.39.0 in this worktree. terraform-ls advertised definition,
  references, document/workspace symbols, and text synchronization, but no
  call hierarchy or type hierarchy, matching ADR-0040.
- Language issues #27–#37 implement the selections; #24's conformance harness
  re-verifies provider capabilities as contract tests.
