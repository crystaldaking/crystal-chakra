# Shell language support

Status: first-class (see `docs/support/languages/shell.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0027; shared LSP
client record: ADR-0032; reference-enrichment record: ADR-0038.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.sh`, `.bash`,
  `.zsh`, and `.ksh` files.
- Project scopes use the nearest `.shellcheckrc` or `shellcheckrc` boundary,
  with a repository-relative path fallback. Test, vendor, and generated roles
  follow explicit directory/file conventions without executing scripts.
- Tree-sitter syntax intelligence (`tree-sitter-bash 0.25.1`): script modules,
  nested and top-level functions, `source`/`.` imports, aliases, byte-accurate
  ranges, actionable diagnostics, test hints, and bounded static command-call
  candidates. Unique local function names produce Chakra-owned syntax call
  edges; ambiguous names remain explicit.
- A tested `bash-language-server` adapter confirms incoming references and
  maps them to provider-reported enclosing functions. Outgoing calls remain
  syntax-derived because the server has no call-hierarchy capability
  (ADR-0038).
- All seven Chakra queries and MCP exposure, including atomic revisions,
  `require_fresh`, provenance/precision, ambiguity reporting, budgets,
  truncation, cancellation, and graceful provider degradation.

## Install and runtime requirements

- Syntax intelligence is fully offline: no shell interpreter, ShellCheck,
  Node.js, npm, or language server is required. Chakra never executes indexed
  scripts.
- Precise incoming-reference enrichment optionally uses
  `bash-language-server` 5.6.x with Node.js. A reproducible installation is:

  ```sh
  npm install --global bash-language-server@5.6.0
  ```

  Put `bash-language-server` on `PATH` or pass
  `--bash-language-server-path`; use `--no-bash-language-server` for
  deterministic syntax-only operation. The route is dormant until the first
  precise Shell query. Chakra does not invoke explainshell or any other
  network-backed hover service.

## Precision tiers and limitations

- Precise: repository-local incoming references confirmed by
  `bash-language-server` and attributed to its document symbols.
- Syntax: declarations, containers, sources/aliases, ranges, diagnostics,
  test hints, and static local function call sites in both directions.
- Heuristic: uniquely resolved local function relations.
- Textual: plain text search hits.

Shell is runtime-dynamic. Chakra does not execute expansion, command
substitution, `eval`, traps, aliases, PATH lookup, or sourced scripts. Calls
through variables and computed names stay unresolved. Static external command
names can appear as bounded call candidates but do not resolve to fabricated
repository symbols. POSIX/Bash syntax is the supported baseline; zsh/ksh
extensions accepted by the Bash grammar are indexed conservatively and may
produce diagnostics for dialect-only constructs.

Top-level provider references have no enclosing callable and are omitted from
precise caller results. Provider locations outside the captured worktree are
also omitted. Absence, timeout, crash, or missing capabilities keeps the
syntax graph available and reports provider degradation.

## Evidence

- Conformance: `docs/support/conformance/shell.json` (14/14 scenarios).
- Adapter tests: `crates/chakra-language-shell/tests/fixture_index.rs` and
  parser/indexer unit tests.
- Provider tests:
  `crates/chakra-provider-bash-language-server/tests/lifecycle.rs`.
- Live and MCP tests: `crates/chakra-language/tests/live_updates.rs` and
  `crates/chakra-mcp/tests/contract.rs`.
- Corpus: `docs/support/corpus/results/shell-ohmyzsh__ohmyzsh.json` and
  `docs/support/corpus/results/shell-nvm-sh__nvm.json`.
