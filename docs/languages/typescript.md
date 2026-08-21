# TypeScript language support

Status: first-class (see `docs/support/languages/typescript.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0027; provider
integration record: ADR-0032.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.ts`, `.tsx`,
  `.mts`, and `.cts` files.
- npm-aware project scopes and language-neutral source roles (ADR-0019):
  every `package.json` is a package root (workspace members are covered by
  their own manifests), a `tsconfig.json` without a sibling `package.json`
  is a project boundary, and `*.test.*` / `*.spec.*` / `__tests__/` sources
  classify as tests.
- Tree-sitter syntax intelligence (`tree-sitter-typescript 0.23.2`; the TSX
  grammar parses `.tsx` sources): declarations (functions, classes,
  interfaces, type aliases, enums, methods, variables), nested containers
  (namespaces/modules/classes), ES module imports and re-exports including
  aliases (`import { x as y }`, `import * as ns`, `export ... from`),
  byte-accurate ranges, jest/vitest/mocha test hints (`describe`/`it`/`test`
  blocks), actionable syntax diagnostics (ADR-0022), and bounded lazy syntax
  call candidates (ADR-0010).
- Import-aware syntax resolution: named-import aliases and namespace imports
  with *relative* specifiers resolve calls and `extends`/`implements`
  relations against the target module; `new X()` records a constructor call.
  Package (non-relative) specifiers are not resolved syntactically.
- Precise enrichment through vtsls (optional, on demand; ADR-0032):
  definitions, references, and callers with revision-scoped synchronization
  over the shared `chakra-lsp` client.
- All seven Chakra queries (`status`, `repo_map`, `search`, `symbol_search`,
  `context`, `callers`, `diff_context`) and their MCP exposure, with atomic
  revisions, `require_fresh`, provenance/precision, ambiguity reporting,
  budgets, truncation, and cancellation.

## Install and runtime requirements

- **Syntax intelligence** (always available): none. The grammar is compiled
  into Chakra and indexing runs fully offline: no Node.js, no npm install,
  and no language server is required.
- **Precise enrichment** (optional): Node.js plus the vtsls server
  (`npm install -g @vtsls/language-server`, or a project-local install) and a
  resolvable TypeScript (`tsdk`) — vtsls exits silently without one; see
  ADR-0032. Chakra owns the process lifecycle: bounded readiness, restart,
  cancellation, and shutdown without orphan processes. When vtsls is absent,
  crashed, or not ready, queries degrade to syntax intelligence with explicit
  provenance; `status` reports the configured provider as `dormant` before
  first use and `degraded` after a failed activation.

## Precision tiers

- **Precise** (`vtsls`): definitions, references, and callers confirmed by
  the language server, when configured.
- **Syntax** (`tree_sitter`): declarations, containers, imports, ranges,
  diagnostics, call-site records.
- **Heuristic** (`tree_sitter`): resolved call and heritage relations.
- **Textual** (`text_search`): plain text search hits.

Corpus evidence (`docs/support/corpus/results/`) is syntax-tier: providers
are off by default in the corpus runner.

## Measured limitations

From the pinned public corpus evaluation (`docs/support/corpus/RESULTS.md`,
macOS/aarch64, 2026-08-19, release build):

- `microsoft/vscode` (12 835 discovered TypeScript sources, 148.3 MiB —
  above the default 128 MiB workspace source-byte budget): the index
  **degrades as designed** (ADR-0011) instead of failing — 11 549 files
  parsed at the budget cap with 7 recorded degradations, cold index ≈ 7.7 s,
  peak RSS ≈ 2.6 GiB, warm no-op ≈ 0.53 s. The degradations are: 1 file over
  the 8 MiB single-file limit, 1 285 files omitted by the workspace
  source-byte budget, and the shared 500 000-symbol graph budget splitting
  across the three languages present in the repo (TypeScript plus 88 Rust
  and 3 PHP files), which retains 499 484 of 564 676 extracted symbols and,
  transitively, only 7 619 of 873 165 extracted call sites. Call-graph
  coverage on this repository is therefore thin under default budgets; the
  degradation is recorded, not silent. With the byte budget saturated, a
  one-file edit can legitimately (de)materialize small budget-boundary
  files; the corpus edit scenarios record this as bounded churn, never a
  full reindex.
- 44 of the parsed vscode files carry syntax diagnostics (Tree-sitter
  error recovery keeps their intact declarations queryable). Some reflect
  the recorded grammar-version lag of the 0.23.2 crate against current
  TypeScript syntax (ADR-0027).

Known false-negative classes — these stay unresolved or ambiguous rather
than being guessed:

- Untyped member calls (`obj.method()` without an explicit receiver type):
  resolved only when the method name is unique; otherwise reported as
  ambiguous candidates, never guessed.
- Package imports (`import ... from "react"`): not resolvable to repository
  sources syntactically.
- Computed/dynamic dispatch, higher-order callbacks, and decorators'
  runtime effects.
- CommonJS `require()`/`module.exports` relations (only ES module syntax is
  extracted).

## Evidence

- Conformance: `docs/support/conformance/typescript.json` (14/14 scenarios).
- Corpus: `docs/support/corpus/results/typescript-microsoft__vscode.json`.
- Adapter tests: `crates/chakra-language-typescript/tests/fixture_index.rs`
  (declarations, containers, imports/aliases, ranges, test hints,
  diagnostics, call candidates, ambiguity, reconcile) and
  `crates/chakra-language-typescript/src/indexer.rs` unit tests (parallel
  determinism, cancellation, bounded lazy call fan-out).
- Discovery/classification: `crates/chakra-git/src/discovery.rs` and
  `crates/chakra-git/src/source_metadata.rs` tests.
