# Go language support

Status: first-class (see `docs/support/languages/go.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0027; shared LSP
client record: ADR-0032; gopls integration record: ADR-0041.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.go` files.
  `go.mod`, `go.sum`, `go.work`, and `go.work.sum` participate in freshness
  but are not indexed as source documents.
- Project scopes use the nearest Git-visible `go.mod` module directive, with
  a `go.work` workspace fallback. `_test.go`, vendor paths, and conventional
  generated filenames/directories receive explicit source roles.
- Tree-sitter syntax intelligence (`tree-sitter-go 0.25.0`): packages,
  imports and aliases, `//go:build`/legacy build constraints, constants,
  variables, type aliases, structs, interfaces, fields, interface methods,
  functions, methods and receivers, generic declarations, Go test entry
  points, Unicode-aware ranges, diagnostics, and bounded call candidates.
- Unique local free-function targets produce Chakra-owned heuristic caller
  relations. Calls through selectors remain honest about ambiguity unless the
  syntax expression itself supplies a usable type qualifier.
- An optional gopls adapter supplies precise incoming and outgoing call
  hierarchy for the pinned workspace revision.
- All seven Chakra queries and MCP exposure, including atomic revisions,
  `require_fresh`, Git diff context, provenance/precision, budgets,
  truncation, cancellation, and graceful provider degradation.

## Install and runtime requirements

Syntax intelligence is fully offline: Go, gopls, module downloads, and
network access are not required. Chakra never invokes `go list`, builds the
repository, or downloads dependencies during syntax indexing.

Precise call-hierarchy enrichment optionally uses gopls 0.23.x. Install a
pinned gopls release with the Go toolchain, put `gopls` on `PATH`, or pass
`--gopls-path`; use `--no-gopls` for deterministic syntax-only operation.
Chakra starts `gopls serve` only for a precise Go query and reserves 768 MiB
in the bounded provider pool while the route is active.

## Precision tiers and limitations

- Precise: incoming and outgoing call hierarchy confirmed by gopls for the
  synchronized workspace revision.
- Syntax: declarations, containers, imports/build constraints, ranges,
  diagnostics, test hints, and call candidates.
- Heuristic: uniquely resolved local syntax call relations.
- Textual: plain text search hits.

The syntax tier does not type-check, resolve selectors through variable or
interface types, infer embedded-method promotion, compute implementations,
expand generated code, evaluate build tags, or run cgo. It indexes every
Git-visible `.go` file, so files excluded by the host's current build
configuration remain queryable and expose their build constraints as syntax
facts. Package/module relationships are metadata scopes rather than a full Go
package loader.

Provider locations outside captured Go documents are omitted. Provider
absence, missing call-hierarchy capability, crash, timeout, or cancellation
leaves the syntax graph available and reports degradation. Changes to
non-source module/workspace inputs are freshness inputs, but the current LSP
document delta contains source documents only (issue #71).

## Evidence

- Conformance: `docs/support/conformance/go.json` (14/14 scenarios).
- Adapter tests: `crates/chakra-language-go/tests/fixture_index.rs` and
  parser/indexer unit tests.
- Provider tests: `crates/chakra-provider-gopls/tests/lifecycle.rs` and the
  opt-in `tests/real_provider.rs` smoke test.
- Live and MCP tests: `crates/chakra-language/tests/live_updates.rs` and
  `crates/chakra-mcp/tests/contract.rs`.
- Corpus: `docs/support/corpus/results/go-prometheus__prometheus.json` and
  `docs/support/corpus/results/go-kubernetes__kubernetes.json`.
