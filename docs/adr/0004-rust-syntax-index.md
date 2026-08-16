# ADR-004: Git-aware Rust syntax index

Status: accepted
Date: 2026-08-15
Last reviewed: 2026-08-16

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
  parser. It owns Tree-sitter parsing, Rust-specific syntax extraction, and
  graph construction. Shared Git source discovery later moved to `chakra-git`
  under ADR-008 so Rust and PHP use one inventory policy.
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
- Git stdout is drained while only a fixed 64 MiB prefix can be retained;
  stderr capture is separately bounded. Discovery fails explicitly when the
  inventory exceeds that budget instead of allocating the complete output
  before checking its size. Every Git discovery child has a 30-second process
  deadline, and paths known not to be Rust are rejected from their raw bytes
  before UTF-8 conversion so an unrelated non-UTF-8 filename cannot disable
  indexing.
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
- Read source files through a bounded reader. One Rust source is capped at
  8 MiB and the repository's captured Rust text at 256 MiB; exceeding either
  budget aborts the private build before any fresh revision is published.
- Nested function declarations own their own contained symbols and call
  candidates. The outer function records the nested declaration but does not
  absorb calls from the nested body.

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
- Source bodies consume bounded memory, and `Arc<str>` prevents graph snapshot
  clones from duplicating them. Measure before introducing persistence or a
  second text index.
- Physical module qualification covers conventional Rust file layouts and
  inline modules. Cross-file remapping introduced by a custom `#[path]`
  attribute is a documented v0.1 limitation rather than a guessed relation.

## Validation / follow-up

- Discovery tests cover tracked, untracked, ignored, `target`, tracked files
  matching ignore rules, an unrelated non-UTF-8 filename, and a real linked
  worktree whose `.git` is a file.
- Parser tests cover declarations, fields, impl containers, calls, test
  attributes, Unicode-aware source positions, bounded import signatures, and
  partial extraction from an error tree.
- An indexer regression rejects a source one byte above the file budget.
- Fixture integration tests cover ambiguous `refund` symbols, imports,
  containment, impls, rejection of false qualified impl links,
  call-candidate quality, exact/regex text search, bounded source output,
  context snippets, and indexing/query measurements.
- The MCP contract test queries the real fixture through an in-process MCP
  client.
- Current local fixture measurements and their exact test entry points are
  recorded in `docs/evaluation/v0.1-readiness.md`.
