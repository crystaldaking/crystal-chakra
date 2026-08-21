# JavaScript language support

Status: first-class (see `docs/support/languages/javascript.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0027; shared LSP
client record: ADR-0032; CommonJS fact model: ADR-0034.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.js`, `.jsx`,
  `.mjs`, and `.cjs` files.
- package.json-aware project scopes and language-neutral source roles
  (ADR-0019): every `package.json` is a package root, a `jsconfig.json`
  without a sibling `package.json` is a project boundary named after its
  directory, and `__tests__/` / `*.test.*` / `*.spec.*` / `tests/` sources
  classify as tests.
- Tree-sitter syntax intelligence (`tree-sitter-javascript 0.25.0`): one
  grammar parses `.js`/`.mjs`/`.cjs` sources and JSX natively for `.jsx`
  sources (no separate TSX-style grammar or configuration). Extraction
  covers declarations (functions, generators, async functions, classes,
  methods, class fields, arrow/function-expression bindings), nested
  function/class containers, ES module imports/exports including aliases,
  CommonJS `require()` bindings and `module.exports`/`exports` assignment
  facts (ADR-0034), byte-accurate ranges, jest/vitest/mocha test hints
  (`describe`/`it`/`test` and their `.only`/`.skip`/`.todo` variants),
  actionable syntax diagnostics (ADR-0022), and bounded lazy syntax call
  candidates (ADR-0010).
- Import-aware syntax resolution: named-import aliases and namespace
  imports — from ES imports *and* `require()` bindings — resolve calls and
  `extends` relations against the target module; `new ClassName()` records
  a constructor call; `this.` calls qualify against the enclosing class;
  `Namespace.member()` calls qualify through namespace aliases. Only
  relative specifiers resolve; package specifiers (npm packages) record the
  import fact without an alias.
- Precise enrichment through vtsls (optional, on demand; ADR-0027/0032):
  definitions, references, and callers with revision-scoped
  synchronization. One vtsls session serves TypeScript and JavaScript
  natively; JavaScript precise facts carry the same `Provenance::Vtsls`
  provenance and `.js`/`.jsx`/`.mjs`/`.cjs` documents synchronize with the
  `javascript`/`javascriptreact` language ids.
- All seven Chakra queries (`status`, `repo_map`, `search`, `symbol_search`,
  `context`, `callers`, `diff_context`) and their MCP exposure, with atomic
  revisions, `require_fresh`, provenance/precision, ambiguity reporting,
  budgets, truncation, and cancellation.

## Install and runtime requirements

- **Syntax intelligence** (always available): none. The grammar is compiled
  into Chakra and indexing runs fully offline: no Node.js, no package
  install, and no language server is required.
- **Precise enrichment** (optional): the vtsls language server —
  `npm install -g @vtsls/language-server` (or a `vtsls` shim on `PATH`); a
  global npm package without a `PATH` shim is resolved through
  `npm root -g` and launched with `node`. Chakra owns the process
  lifecycle: bounded readiness, restart, cancellation, and shutdown without
  orphan processes. When vtsls is absent, crashed, or not ready, queries
  degrade to syntax intelligence with explicit provenance and `status`
  reports the configured provider as `dormant` before first use and
  `degraded` after a failed activation. The provider binary is shared with
  the TypeScript support; no JavaScript-specific server is needed.

## Precision tiers

- **Precise** (`vtsls`): definitions, references, and callers confirmed by
  the language server, when configured.
- **Syntax** (`tree_sitter`): declarations, containers, imports (ES and
  CommonJS), ranges, diagnostics, call-site records.
- **Heuristic** (`tree_sitter`): resolved call and `extends` relations.
- **Textual** (`text_search`): plain text search hits.

Corpus evidence (`docs/support/corpus/results/`) is syntax-tier: providers
are off by default in the corpus runner.

## Measured limitations

From the pinned public corpus evaluation (`docs/support/corpus/RESULTS.md`,
macOS/aarch64, 2026-08-20, release build):

- `react/react` (4 557 sources: 3 896 JavaScript plus 541 TypeScript and
  120 Rust, 24 MiB): cold index ≈ 2.1 s, peak RSS ≈ 522 MiB, 82 629
  symbols, 66 023 edges, warm no-op ≈ 0.11 s, no degradations. 939 parsed
  files carry syntax diagnostics, dominated by Flow-annotated `.js`
  sources: 1 161 react files carry a `@flow` pragma and Flow type
  annotations are not JavaScript grammar. Tree-sitter error recovery keeps
  their intact declarations queryable; Flow-specific type syntax is not
  extracted (tree-sitter-javascript is the ADR-0027 grammar; a Flow grammar
  is out of scope for v0.1).
- Syntax call coverage is honestly shallow: 105 337 of 141 546 call sites
  stay unresolved and 18 241 are ambiguous — JavaScript's dynamic dispatch
  is largely unresolvable from syntax alone and Chakra reports that instead
  of guessing.

Known false-negative classes — these stay unresolved or ambiguous rather
than being guessed:

- Duck-typed member calls (`obj.method()` without a nameable receiver
  type): resolved only when the method name is unique; otherwise reported
  as ambiguous candidates, never guessed.
- Dynamic dispatch: computed property access, `Reflect`, proxies, monkey
  patching, and `Object.assign`-based exports.
- CommonJS shapes outside the ADR-0034 model: dynamic or non-literal
  `require(expr)`, `require` calls nested in arbitrary expressions, and
  export-name enumeration from `module.exports = { a, b }` object literals.
- Class-expression bindings (`const X = class {...}`) and calls inside
  class field initializers are not indexed.

## Evidence

- Conformance: `docs/support/conformance/javascript.json` (14/14
  scenarios), including a CommonJS `require()` alias hard case.
- Corpus: `docs/support/corpus/results/javascript-react__react.json`
  (12/12).
- Adapter tests: `crates/chakra-language-javascript/tests/fixture_index.rs`
  (declarations, containers, ES/CommonJS imports and aliases, ranges, test
  hints, JSX, diagnostics, call candidates, ambiguity, reconcile) and
  `crates/chakra-language-javascript/src/indexer.rs` unit tests (parallel
  determinism, cancellation, bounded lazy call fan-out).
- Provider contract tests: `crates/chakra-provider-vtsls/tests/lifecycle.rs`
  (fake-server lifecycle, delta sync, JavaScript document language-id
  contract, cancellation, crash restart, orphan-free shutdown).
- Discovery/classification: `crates/chakra-git/src/discovery.rs` and
  `crates/chakra-git/src/source_metadata.rs` tests.
