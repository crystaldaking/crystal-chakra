# ADR-004: Git-aware Rust syntax index

Status: accepted
Date: 2026-08-15

## Context

Roadmap §§7 and 11 require a deterministic Tree-sitter Rust baseline over
Git-tracked plus untracked non-ignored files. SPEC §§7, 18–21 require
explicit provenance/precision, repository-aware exclusions, syntax call
candidates rather than invented precise calls, and an incremental-friendly
index shape. Language tooling must remain an adapter, and the implementation
must not assume that Git administration is a `.git` directory.

The upstream Tree-sitter project currently publishes the maintained
`tree-sitter` Rust runtime and the official `tree-sitter-rust` grammar:

- <https://github.com/tree-sitter/tree-sitter>
- <https://github.com/tree-sitter/tree-sitter-rust>

## Decision

- Add `chakra-language-rust` as a real adapter crate. It depends inward on
  Chakra domain/engine types and the core crates do not depend on the Rust
  parser. It owns Git discovery, Tree-sitter parsing, Rust-specific syntax
  extraction, and graph construction.
- Use workspace-managed `tree-sitter` 0.26 and `tree-sitter-rust` 0.24. The
  grammar exposes `LANGUAGE` through the current `tree-sitter-language`
  binding and is loaded through `Parser::set_language`.
- Ask Git itself for the repository root and scan set. Discovery runs
  `git rev-parse --show-toplevel` and NUL-delimited
  `git ls-files --cached --others --exclude-standard` with fixed arguments,
  then admits regular `.rs` files only. It explicitly excludes `target`
  and `.git` path components and skips paths whose final filesystem entry is
  a symlink. Repository-relative paths reject lexical traversal, and no
  administrative Git path is constructed by Chakra. Reconciliation against
  concurrent filesystem replacement belongs to the live-update slice.
- Parse files in sorted repository-relative order and build the graph in
  deterministic passes: files/source, declarations, containment and impl
  relations, then call candidates. Declarations and direct AST containment
  use Tree-sitter/syntax quality. Cross-declaration name links, including
  impl targets, trait methods, and call candidates, use Tree-sitter
  provenance with heuristic precision. Impl linking only considers unique,
  unqualified names in the same logical module; qualified/external and
  compound paths are not collapsed to a same-named local declaration.
- Capture source text in the same immutable graph revision as syntax facts.
  Snapshot text search uses the mature Rust `regex` crate in literal or
  regex mode, returns textual precision, and bounds result count, pattern
  length, and returned source lines. Symbol-name search caps its input and
  stops result construction at the requested budget. Context snippets have
  line and character caps. Extracted signatures, including imports, share
  one hard character cap whose truncation marker stays inside the budget.
- Tree-sitter error trees are accepted. Valid declarations elsewhere in a
  temporarily invalid file remain indexable, and the index reports how many
  files contain syntax errors.

## Alternatives considered

- Recursively walk the filesystem and apply `.gitignore` rules ourselves:
  rejected because tracked files may intentionally match ignore rules and
  linked-worktree/submodule semantics already belong to Git.
- Add a full Git implementation crate for this discovery slice: deferred.
  It would add substantial compile/transitive cost while fixed-argument Git
  plumbing provides the exact required tracked/untracked set. The process is
  not exposed through MCP and no repository-controlled string is executed by
  a shell.
- Put Tree-sitter directly in `chakra-engine`: rejected because it would
  reverse the language-adapter dependency boundary.
- Treat name-matched call targets as syntax-precise or provider-precise:
  rejected. Tree-sitter does not type-resolve Rust calls.
- Resolve qualified impl paths by their final identifier: rejected because
  `std::fmt::Display` must not be linked to an unrelated local `Display`.
  v0.1 skips unresolved qualified links rather than building a Rust resolver.
- Read files directly during each text query: rejected because a query could
  mix published syntax revision N with filesystem contents from N+1.

## Consequences

- Runtime dependencies added: `tree-sitter`, `tree-sitter-rust`, and
  `regex`. The Tree-sitter projects and grammar are MIT licensed; `regex` is
  MIT/Apache-2.0. `tempfile` is test-only for real temporary Git repository
  coverage.
- `git` must be available to index a repository, which is consistent with
  Git being Chakra's canonical source of truth.
- Initial indexing is a full deterministic rebuild. Per-file replacement,
  watcher coalescing, and reconciliation reuse this representation in the
  live-update slice; they are not silently simulated here.
- Source bodies consume memory, but `Arc<str>` prevents graph snapshot clones
  from duplicating them. Measure before introducing persistence or a second
  text index.

## Validation / follow-up

- Discovery tests cover tracked, untracked, ignored, `target`, tracked files
  matching ignore rules, and a real linked worktree whose `.git` is a file.
- Parser tests cover declarations, fields, impl containers, calls, test
  attributes, Unicode-aware source positions, bounded import signatures, and
  partial extraction from an error tree.
- Fixture integration tests cover ambiguous `refund` symbols, imports,
  containment, impls, rejection of false qualified impl links,
  call-candidate quality, exact/regex text search, bounded source output,
  context snippets, and indexing/query measurements.
- The MCP contract test queries the real fixture through an in-process MCP
  client.
