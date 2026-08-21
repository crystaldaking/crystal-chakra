# ADR-031: object-safe syntax language adapter trait and registry

Status: accepted
Date: 2026-08-19

## Context

The workspace owner (`crates/chakra-language`) hardcoded exactly two
languages: `WorkspaceSources { rust, php }`, `WorkspaceSyntaxIndex { rust,
php, rust_limits, php_limits }`, a two-way `split_graph_limits`, per-field
scan routing in the source-read loop, and hand-written pairwise merges of
fact counts, reconcile metrics, graph reports, and degradation records.
ADR-008 composed Rust and PHP this way when there were exactly two adapters;
the roadmap now has nine more language crates queued, and every one would
have required editing the same half-dozen pairwise sites in the owner.

## Decision

- Add an object-safe `SyntaxLanguageAdapter` trait in
  `crates/chakra-language/src/adapter.rs` covering only what the workspace
  owner calls: `language()` identity, `clone_box()`, bounded scheduled
  `cold_build` from classified `LanguageSources`, incremental `reconcile`,
  `paths()`, `graph()`, `graph_report()`, and `fact_counts()`.
- Share plain data types across adapters instead of generic parameters so
  the trait stays object-safe: `LanguageSources` (files plus shared
  `SourceMetadata`), `AdapterFactCounts`, `AdapterBuildMetrics`,
  `AdapterReconcileMetrics`, and `AdapterFrameworkMetrics` (zero for
  adapters without framework enrichment). `From` conversions map the
  existing per-crate types without changing the adapter crates' public APIs.
- Implement the trait for `RustSyntaxIndex` and `PhpSyntaxIndex` as thin
  delegating impls inside `chakra-language` (orphan rule; the dependency
  direction owner → adapter crates is unchanged). PHP's Laravel detection
  moves into its `cold_build` implementation behind the shared
  `repository_root` parameter.
- Replace the hardcoded owner fields with an ordered registry:
  `default_adapters()` returns the boxed adapters in composition order and
  `registered_languages()` derives the language list from it.
  `WorkspaceSources` becomes an ordered `Vec<WorkspaceLanguageSources>`,
  `WorkspaceSyntaxIndex` holds `Vec<WorkspaceAdapterState>`, and scan
  routing, budget splitting, graph merge, phase/degradation aggregation all
  iterate the registry in order.
- Generalize `split_graph_limits` to N languages with sequential
  proportional shares (`remaining * count / remaining_total`), which is
  arithmetically identical to the historical two-way split for the Rust/PHP
  registry.
- The conformance corpus gate (`supported_languages()`) derives from
  `registered_languages()` instead of a hardcoded `["php", "rust"]`.
- The `Language` enum is unchanged; new variants still land with their own
  language issues. `IndexMetrics.rust_files`/`php_files` remain as public
  fields, now filled by registry lookup.

## Alternatives considered

- **Keep hardcoding per language pair**: each new language crate would edit
  the owner's sources struct, index struct, scan routing, budget split,
  metric combines, and degradation emission. Nine upcoming crates makes this
  the most error-prone option, and ordering mistakes would silently change
  merge and degradation order.
- **Enum dispatch** (`enum AnySyntaxIndex { Rust(..), Php(..) }`):
  compiles to the same static behavior and keeps `Clone` cheap, but the
  enum, its constructors, and every match arm still live in the owner and
  grow per language; a trait object registry isolates the registration to
  one function (`default_adapters`) and matches the planned
  adapter-crate-per-language layout. The boxed clone is cold-path only
  (registry snapshots on reconcile), so the dynamic-dispatch cost is
  irrelevant next to Tree-sitter parsing.
- **Generic trait with associated source/metric types**: not object-safe,
  forcing the owner back into generics or an enum; rejected in favor of
  shared plain data types with `From` conversions.

## Consequences

- A new language crate appends one `Box::new(...)` line to
  `default_adapters()` (plus its `Language` variant and scan classification)
  and is picked up by scanning, budgeting, composition, status, and the
  conformance gate without further owner edits.
- Registry order (Rust, then PHP) is now semantically significant: it fixes
  graph-merge order, degradation record order, and phase ordering. It is
  documented at `default_adapters()`.
- No behavior change for Rust/PHP workspaces: query results, provenance,
  freshness, graph contents, indexing status, and conformance output are
  unchanged. The only intentional structural difference is internal
  (vectors instead of named field pairs).
- `WorkspaceSources` lost its public `rust`/`php` fields in favor of
  `get`/`file_count` accessors; the only in-workspace consumer was one test
  assertion.

## Validation / follow-up

- `cargo fmt --all -- --check`, `cargo clippy --locked --workspace
  --all-targets -- -D warnings`: clean.
- `cargo test --locked --workspace`: all suites pass.
- `cargo run --locked -p chakra-conformance -- emit docs/support/conformance`:
  byte-identical output (rust 14/14, php 14/14).
- `cargo run --locked -p chakra-conformance -- corpus --verify`: 0 problems.
- `python3 tools/check_support_matrix.py --check`, `cargo deny check`: clean.
- Follow-up: when the third language lands, `IndexMetrics` should grow a
  registry-keyed per-language file count instead of named
  `rust_files`/`php_files` fields.
