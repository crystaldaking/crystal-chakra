# PHP language support

Status: first-class (see `docs/support/languages/php.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0027.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.php` files.
- Composer-aware project scopes and language-neutral source roles
  (production/test, ADR-0019).
- Tree-sitter syntax intelligence (`tree-sitter-php 0.24.2`): declarations,
  namespace/class containers, `use` imports and aliases, byte-accurate
  ranges, PHPUnit-style test hints, actionable syntax diagnostics
  (ADR-0022), and bounded lazy syntax call candidates (ADR-0010).
- Receiver-aware call resolution (ADR-0015): receiver types inferred only
  from syntactically explicit evidence — typed parameters, typed and
  constructor-promoted properties, local `new`, `app(Foo::class)` /
  `resolve(Foo::class)`, explicit scoped types, and fluent reassignments
  that preserve such evidence — resolved against a parsed
  class/interface/trait catalog including inheritance.
- Precise-equivalent relations (ADR-0030): receiver-resolved `CALLS`/`TESTS`
  relations whose evidence is explicit, single-candidate, and
  inheritance-unambiguous are published as `precise` with `chakra_resolver`
  provenance. All other resolved calls stay `heuristic`; precision is never
  upgraded silently (PROV-01).
- Deterministic Laravel framework enrichment (ADR-0017) when Composer
  metadata signals Laravel: container bindings, routes, events, jobs,
  policies, and related framework relations, all heuristic.
- All seven Chakra queries (`status`, `repo_map`, `search`, `symbol_search`,
  `context`, `callers`, `diff_context`) and their MCP exposure, with atomic
  revisions, `require_fresh`, provenance/precision, ambiguity reporting,
  budgets, truncation, and cancellation.

## Install and runtime requirements

None. The grammar is compiled into Chakra and indexing runs fully offline:
no PHP binary, no Composer install, and no language server is required for
any PHP capability, including the precise-equivalent tier. No precise PHP
provider is integrated (deferral: ADR-0018; the shipped equivalent is
ADR-0030), so `status` reports no provider entry for a PHP-only workspace.

## Precision tiers

- **Syntax** (`tree_sitter`): declarations, containers, imports, ranges,
  diagnostics, call-site records.
- **Precise** (`chakra_resolver`): only strict-tier receiver-resolved
  `CALLS`/`TESTS` relations (ADR-0030).
- **Heuristic** (`tree_sitter`/`heuristic`): all other resolved call and
  test relations, and every Laravel framework relation.
- **Textual** (`text_search`): plain text search hits.

## Measured limitations

From the pinned public corpus evaluation (`docs/support/corpus/RESULTS.md`,
macOS/aarch64, 2026-08-18, release build):

- `laravel/framework` (3 039 source files): cold index ≈ 1.8 s, peak RSS
  ≈ 549 MiB, 56 067 symbols / 118 717 edges, warm no-op ≈ 59 ms.
- `symfony/symfony` (11 114 parsed of 11 116 discovered source files;
  2 unreadable non-UTF-8 files are skipped with the index intact): cold
  index ≈ 5.2 s, peak RSS ≈ 1.2 GiB, 121 822 symbols / 245 502 edges, warm
  no-op ≈ 212 ms.

Known false-negative classes (ADRs 0015, 0018, 0030) — these stay unresolved
or heuristic rather than being guessed:

- Dynamic dispatch: untyped receivers, variable class names, string
  callables, `method_exists`/`is_callable` dispatch, and `static::` late
  static binding.
- Magic methods (`__call`, `__callStatic`) and dynamic properties.
- PHPDoc-only types, generics, and arbitrary factory/container return flow.
- Framework container bindings beyond the deterministic Laravel enrichment.

Known limitation of the corpus harness: cancellation is asserted with
pre-cancelled tokens; mid-flight cancellation coverage is a recorded
follow-up (ADR-0029).

## Evidence

- Conformance: `docs/support/conformance/php.json` (14/14 scenarios).
- Corpus: `docs/support/corpus/results/php-laravel__framework.json`,
  `docs/support/corpus/results/php-symfony__symfony.json`.
- Strict-tier promotion: `crates/chakra-language-php/src/indexer/catalog.rs`
  with focused tests in `crates/chakra-language-php/src/indexer/tests.rs`
  (`strict_tier_receiver_calls_promote_to_chakra_resolver_precise`,
  `non_strict_receiver_calls_stay_heuristic`,
  `ambiguous_inherited_candidates_stay_heuristic`) and
  `crates/chakra-language-php/tests/fixture_index.rs`.
- Provider evaluation: `docs/evaluation/php-provider-v0.1.1.md`.
