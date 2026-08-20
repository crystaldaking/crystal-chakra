# Python language support

Status: first-class (see `docs/support/languages/python.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0027; shared LSP
client record: ADR-0032.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.py` and `.pyi`
  files.
- pyproject-aware project scopes and language-neutral source roles
  (ADR-0019): every `pyproject.toml` is a package root (PEP 621
  `[project].name` when parseable), a `setup.py` or `setup.cfg` without a
  sibling `pyproject.toml` is a project boundary named after its directory,
  and `test_*.py` / `*_test.py` / `tests/` sources classify as tests.
- Tree-sitter syntax intelligence (`tree-sitter-python 0.25.0`):
  declarations (functions, async functions, classes, methods, class-level
  properties, module-level constants, decorated definitions with decorators
  recorded in the signature), nested containers (module > class > function),
  imports (`import x`, `import x as y`, `from m import a as b`, relative
  `from . import` / `from ..m import`), byte-accurate ranges, pytest/unittest
  test hints (`test_*` functions and methods), actionable syntax diagnostics
  (ADR-0022), and bounded lazy syntax call candidates (ADR-0010).
- Import-aware syntax resolution: named-import aliases and namespace imports
  resolve calls and base-class relations against the target module;
  `ClassName()` records an `__init__` constructor call; `self.`/`cls.` calls
  qualify against the enclosing class; `super().method()` qualifies against
  the sole written base class when there is exactly one. Non-repository
  (stdlib/third-party) module paths are not resolved syntactically.
- Precise enrichment through pyright (optional, on demand; ADR-0027/0032):
  definitions, references, and callers with revision-scoped synchronization
  over the shared `chakra-lsp` client.
- All seven Chakra queries (`status`, `repo_map`, `search`, `symbol_search`,
  `context`, `callers`, `diff_context`) and their MCP exposure, with atomic
  revisions, `require_fresh`, provenance/precision, ambiguity reporting,
  budgets, truncation, and cancellation.

## Install and runtime requirements

- **Syntax intelligence** (always available): none. The grammar is compiled
  into Chakra and indexing runs fully offline: no Python interpreter, no
  package install, and no language server is required.
- **Precise enrichment** (optional): the pyright language server —
  `npm install -g pyright` (needs Node.js) or `pip install pyright`, both
  providing `pyright-langserver`; a global npm package without a `PATH` shim
  is resolved through `npm root -g` and launched with `node`. Chakra owns the
  process lifecycle: bounded readiness, restart, cancellation, and shutdown
  without orphan processes. When pyright is absent, crashed, or not ready,
  queries degrade to syntax intelligence with explicit provenance and
  `status` reports the configured provider as `dormant` before first use and
  `degraded` after a failed activation. Real pyright 1.1.413 was probed
  locally: it advertises definition, references, and callHierarchy on a bare
  `initialize` (no `initializationOptions` required).

## Precision tiers

- **Precise** (`pyright`): definitions, references, and callers confirmed by
  the language server, when configured.
- **Syntax** (`tree_sitter`): declarations, containers, imports, ranges,
  diagnostics, call-site records.
- **Heuristic** (`tree_sitter`): resolved call and base-class relations.
- **Textual** (`text_search`): plain text search hits.

Corpus evidence (`docs/support/corpus/results/`) is syntax-tier: providers
are off by default in the corpus runner.

## Measured limitations

From the pinned public corpus evaluation (`docs/support/corpus/RESULTS.md`,
macOS/aarch64, 2026-08-20, release build):

- `django/django` (2 928 Python sources, 18.5 MiB): cold index ≈ 2.1 s,
  peak RSS ≈ 651 MiB, 117 181 symbols, 139 240 edges, warm no-op ≈ 0.13 s,
  no degradations. One parsed file carries syntax diagnostics (Tree-sitter
  error recovery keeps its intact declarations queryable).
- `apache/airflow` (8 806 sources: 7 743 Python plus 1 063 TypeScript,
  66 MiB): cold index ≈ 5.1 s, peak RSS ≈ 1.6 GiB, 278 038 symbols,
  248 193 edges, warm no-op ≈ 0.24 s, no degradations. Three parsed files
  carry syntax diagnostics.
- Syntax call coverage is honestly shallow on both: 139 134 of 171 261
  (django) and 249 303 of 304 470 (airflow) call sites stay unresolved, and
  2 769 / 8 850 are ambiguous — Python's dynamic dispatch is largely
  unresolvable from syntax alone and Chakra reports that instead of
  guessing.

Known false-negative classes — these stay unresolved or ambiguous rather
than being guessed:

- Duck-typed member calls (`obj.method()` without a nameable receiver
  type): resolved only when the method name is unique; otherwise reported
  as ambiguous candidates, never guessed.
- Dynamic dispatch: `getattr`, monkey-patching, `__getattr__`/`__init_subclass__`
  effects, decorators' runtime effects, and metaclass transformations.
- Constructors of classes without an explicit `__init__` (no callable
  syntax target exists, so `Foo()` stays unresolved for them).
- Conditional/star imports and names bound at runtime (`__all__`,
  `importlib`); lambda bodies are not call-indexed.

## Evidence

- Conformance: `docs/support/conformance/python.json` (14/14 scenarios).
- Corpus: `docs/support/corpus/results/python-django__django.json` and
  `docs/support/corpus/results/python-apache__airflow.json` (11/11 each).
- Adapter tests: `crates/chakra-language-python/tests/fixture_index.rs`
  (declarations, containers, imports/aliases, ranges, test hints,
  diagnostics, call candidates, ambiguity, reconcile) and
  `crates/chakra-language-python/src/indexer.rs` unit tests (parallel
  determinism, cancellation, bounded lazy call fan-out).
- Provider contract tests: `crates/chakra-provider-pyright/tests/lifecycle.rs`
  (fake-server lifecycle, delta sync, cancellation, crash restart,
  orphan-free shutdown).
- Discovery/classification: `crates/chakra-git/src/discovery.rs` and
  `crates/chakra-git/src/source_metadata.rs` tests.
