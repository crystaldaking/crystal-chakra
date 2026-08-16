# ADR-008: PHP syntax support and multi-language composition

Status: accepted
Date: 2026-08-16

## Context

The intended v0.1 product scope includes useful current intelligence for both
Rust and PHP, while the imported governance and roadmap mistakenly declared
PHP deferred. The implemented Rust index owned both language extraction and
the workspace watcher, which could not honestly publish a second language in
the same atomic revision. Query symbol views also omitted their language, and
the optional rust-analyzer adapter could be asked to enrich any symbol.

The official maintained `tree-sitter-php` project publishes a Rust grammar
binding compatible with Chakra's Tree-sitter runtime:

- <https://github.com/tree-sitter/tree-sitter-php>
- <https://crates.io/crates/tree-sitter-php/0.24.2>

## Decision

- Promote PHP syntax intelligence into the v0.1 roadmap. PHP support covers
  Git-aware `.php` discovery, namespaces, functions/methods, classes,
  interfaces, traits, enums, properties, constants, imports, inheritance
  candidates, call candidates, test hints, ranges, snippets, live updates,
  Git diff context, and all shared query/MCP surfaces.
- Add `chakra-language-php` as an independent Tree-sitter adapter using
  `tree-sitter-php` 0.24.2. It emits only Chakra domain/graph types with
  `tree_sitter` provenance and syntax or heuristic precision. It does not
  implement PHP runtime type resolution.
- Add `chakra-language` as the workspace syntax owner. It composes private
  Rust and PHP graphs, owns the single watcher/freshness barrier, prepares all
  changed language state privately, and publishes one combined graph revision.
  `chakra-language-rust` and `chakra-language-php` remain independently
  testable parsing adapters.
- Move supported-source discovery into `chakra-git`, so initial indexing,
  reconciliation, and `diff_context` share the same `.rs`/`.php`, ignore,
  regular-file, symlink, and Git-worktree policy.
- Add `Language::Php` and expose `language` in every `SymbolView`. A
  same-named Rust/PHP symbol remains ambiguous by name; callers resolve it by
  the revision-scoped entity id returned from `symbol_search`.
- Make precise-provider capability explicit. rust-analyzer advertises Rust
  only, receives only Rust documents, and is never invoked for PHP symbols.
  The CLI does not start it for a PHP-only workspace. PHP queries remain
  current and useful at syntax/heuristic precision.

## Alternatives considered

- Put PHP parsing inside `chakra-language-rust`: rejected because the crate
  boundary and ownership name would become false and future language-specific
  changes would be coupled.
- Run one live publisher per language: rejected because two independent
  freshness owners could publish incomplete/hybrid workspace graphs and race
  over revision freshness.
- Require a PHP LSP before declaring support: rejected for v0.1. First-class
  query support does not require identical semantic precision, and an
  unselected PHP language server would add lifecycle and licensing choices
  unrelated to the deterministic syntax baseline.
- Merge Rust and PHP call candidates by name: rejected because cross-language
  name equality is not a call relationship. Each adapter resolves relations
  only inside its language graph before workspace composition.

## Consequences

- Production dependency added: official `tree-sitter-php` 0.24.2 (MIT). No
  PHP runtime, Composer dependency, external service, or PHP language server
  is required.
- Complete private graph materialization combines both language graphs, but
  unchanged files are not reparsed and unaffected relationship contributions
  remain cached. Instrumentation proves a one-file PHP edit reparses one file.
- PHP call, inheritance, and test relations are intentionally conservative and
  carry heuristic precision where Tree-sitter cannot resolve runtime types,
  aliases, traits, dynamic dispatch, magic methods, or framework metadata.

## Validation / follow-up

- Unit and fixture tests cover PHP syntax extraction, ambiguous duplicate
  names, temporary syntax errors, call/test candidates, and unchanged-content
  avoidance.
- Live integration tests prove an immediate fresh PHP edit appears in one
  atomic revision and reparses exactly one file without reparsing Rust.
- MCP end-to-end coverage exercises PHP `symbol_search`, `context`, callers,
  and `diff_context`, including explicit `language: php` and honest heuristic
  relations.
- A future precise PHP provider requires its own adapter/ADR and capability;
  it must not change the syntax graph's canonical ownership.
