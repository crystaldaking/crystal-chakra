# ADR-022: Revision-scoped actionable syntax diagnostics

Status: accepted
Date: 2026-08-18

## Context

The Rust and PHP syntax indexes accepted Tree-sitter error trees and exposed
only an aggregate count of files with syntax errors. Real evaluations could
therefore report an error without identifying its file, range or recovery
node. That was insufficient both for correcting a temporarily broken edit and
for recognizing a grammar coverage problem.

The `psp-app` evaluation supplied a concrete distinction. PHP itself accepts
`public const string DEFAULT = 'default';`, but the maintained
`tree-sitter-php` 0.24.2 grammar produces one `ERROR` for that declaration.
The same grammar accepts a typed constant with a non-keyword name and an
untyped constant named `DEFAULT`. Treating this valid PHP 8.3 construct as an
ordinary unexplained source failure would be misleading.

A parse-only evaluation of 1,931 tracked Rust files at
`zed-industries/zed@eb5483528be607b497afdaad0a237bebeb191249` reproduced 15
diagnostic files and 28 recovery nodes. Every node came from one of two valid
constructs already listed in the upstream grammar's open rustc-compatibility
tracker: lifetime-first trait objects such as `dyn 'static + Fn()` and
attributes on struct-pattern fields.

The maintained releases were rechecked before making this decision. Chakra
already resolves `tree-sitter` 0.26.12, `tree-sitter-rust` 0.24.2 and
`tree-sitter-php` 0.24.2, so there is no newer compatible published grammar to
adopt:

- <https://crates.io/crates/tree-sitter>
- <https://crates.io/crates/tree-sitter-rust>
- <https://crates.io/crates/tree-sitter-php>
- <https://github.com/tree-sitter/tree-sitter-php/issues/197>
- <https://github.com/tree-sitter/tree-sitter-rust/issues/229>

## Decision

- Each language adapter traverses the exact Tree-sitter tree used for syntax
  extraction and captures `ERROR` and `MISSING` nodes. A diagnostic carries
  language, repository-relative range, recovery kind, grammar node kind and a
  typed cause, together with explicit `tree_sitter` provenance and `syntax`
  precision.
- `parse_recovery` means only that Tree-sitter recovered. It deliberately does
  not claim the source is invalid, because previously unseen valid syntax can
  also produce a recovery node. Confirmed limitations use
  `known_grammar_gap` plus a closed typed identifier. The initial identifiers
  cover PHP's typed class constant named `DEFAULT`, Rust lifetime-first trait
  objects, and Rust attributes on struct-pattern fields. Matchers are narrow
  to the evaluated recovery shapes and surrounding syntax context.
- Diagnostics live with the parsed file facts and are copied into the same
  private graph that is atomically published as a workspace revision. Queries
  never reparse a file or read current source to explain an older revision.
- Retain at most 64 diagnostics per file while separately recording the true
  count. `status` returns the deterministic first 100 diagnostics ordered by
  path, range, language, recovery kind, cause and node kind. It reports total
  and omitted counts plus distinct `per_file_limit` and `status_limit` causes.
- Continue publishing internally consistent error trees. A temporary syntax
  error remains queryable, and correction removes its diagnostics only in the
  later complete revision produced by the normal freshness barrier.

## Alternatives considered

- Treat every recovery node as an invalid-source error: rejected because it
  would misrepresent confirmed grammar gaps and other unsupported valid
  syntax.
- Suppress a diagnostic for a known grammar gap: rejected because the grammar
  still recovered, may have omitted syntax facts, and operators need that
  coverage limitation to remain observable.
- Patch or vendor the PHP grammar in Chakra: rejected for this narrow gap.
  Chakra should track the maintained upstream grammar rather than silently
  fork parser semantics; a future upstream release can remove the typed gap
  and its regression classification together.
- Return source text around every error: rejected because ranges are enough to
  retrieve context and an aggregate status response must remain bounded.

## Consequences

- The status/query schema gains structured diagnostic and truncation fields.
  MCP remains a serialization adapter and no Tree-sitter protocol type crosses
  into the domain contract.
- A nonzero syntax-error count now has at least one path/range diagnostic
  unless the explicit response budget omits it; omitted work is quantified.
- Adding a new known gap requires evidence that the source is valid, a narrow
  adapter-side matcher and a regression fixture. Unknown recovery stays honest
  rather than being guessed into a grammar or source category.
- No production dependency or grammar version changes are introduced.

## Validation / follow-up

- Rust and PHP parser regressions cover temporary broken source, current
  language constructs and language-attributed actionable ranges.
- PHP and Rust regressions preserve each evaluated construct as a named
  grammar gap, while representative current constructs supported by the
  grammars parse without errors.
- The Zed parse-only audit reports 15 diagnostic files, 28 bounded recovery
  nodes and zero unexplained nodes after classification; no complete graph or
  source copy is retained by that manual audit.
- Engine coverage proves deterministic status ordering and simultaneous
  per-file/status omission causes.
- Live reconciliation coverage proves a broken file and its diagnostics are
  published atomically and disappear after deterministic fresh recovery.
- MCP contract coverage proves the structured diagnostic summary is exposed
  without requiring a globally installed language provider.
