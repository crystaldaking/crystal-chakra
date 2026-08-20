# ADR-0038: bash-language-server reference enrichment

Status: accepted
Date: 2026-08-21

## Context

ADR-0027 selected `bash-language-server` 5.6.x as the optional Shell
provider, but the server does not advertise LSP call hierarchy. Chakra still
needs an honest first-class `callers` contract without presenting syntax
facts as provider-native call hierarchy or requiring a network service.

The server does advertise definitions, references, and document symbols.
Those capabilities can confirm incoming references and identify the Shell
function containing each reference. Chakra's Tree-sitter index already owns a
bounded, deterministic graph of static local function calls for both incoming
and outgoing syntax results.

## Decision

- Shell syntax support remains fully offline and uses `tree-sitter-bash`
  0.25.1 for declarations, sources/aliases, diagnostics, and bounded static
  function-call candidates.
- The CLI registers `bash-language-server` as a dormant Shell route in the
  bounded provider pool. Activation reserves 512 MiB; an inactive route owns
  no process or reservation.
- Chakra invokes `bash-language-server start`. Discovery checks `PATH` only
  and never installs packages. `--bash-language-server-path` selects an
  explicit executable and `--no-bash-language-server` disables the route.
- Readiness requires definition, references, and document-symbol
  capabilities. A post-synchronization references request is the revision
  barrier because the server has no explicit quiescence signal.
- Incoming precise callers are built from `textDocument/references`. Each
  repository-local reference is mapped to the innermost enclosing function
  reported by `textDocument/documentSymbol` and carries
  `Provenance::BashLanguageServer`. References outside captured Shell
  documents and top-level references without a callable owner are dropped.
- Outgoing callers remain Chakra's bounded Tree-sitter function-call edges.
  The adapter returns no fabricated provider call hierarchy, so syntax
  provenance and precision remain visible.
- The shared `chakra-lsp` transport owns the process group, revision-scoped
  Shell document synchronization, cancellation, restart, bounded waits, and
  shutdown. Missing capability, executable, crash, timeout, or cancellation
  degrades to syntax results and never fails Chakra startup.
- Chakra never invokes the optional explainshell hover integration. Core and
  precise Shell intelligence therefore require no network access.

## Alternatives considered

- **Advertise references as native LSP call hierarchy.** Rejected because it
  would misstate the provider capability and lose the distinction between
  provider-confirmed incoming references and syntax-derived outgoing calls.
- **Use syntax facts only.** Rejected because repository-local references and
  document symbols provide a useful, revision-scoped precision improvement
  without weakening the fallback contract.
- **Enable explainshell hover.** Rejected because it requires a network
  service and is unrelated to Chakra's code-relationship queries.

## Consequences

- `callers` can contain provider-confirmed incoming Shell relations while
  outgoing relations remain explicitly syntax-derived.
- Dynamic command construction, `eval`, aliases expanded at runtime, sourced
  files selected through variables, and external commands are not promoted
  to precise call edges.
- Hermetic lifecycle tests cover capabilities, document synchronization,
  reference conversion, cancellation, restart, degradation, and orphan-free
  shutdown. A real globally installed server is optional for operators.

## Validation / follow-up

- Shell conformance passes 14/14 scenarios.
- The pinned ohmyzsh and nvm public corpora each pass 11/11 scenarios within
  committed budgets.
- Provider contract evidence lives in
  `crates/chakra-provider-bash-language-server/tests/lifecycle.rs`.
