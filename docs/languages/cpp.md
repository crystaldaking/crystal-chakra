# C++ language support

Status: first-class (see `docs/support/languages/cpp.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0027; shared LSP
client record: ADR-0032; clangd integration record: ADR-0039.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.c`, `.h`, `.cc`,
  `.cpp`, `.cxx`, `.hh`, `.hpp`, `.hxx`, `.ipp`, `.tpp`, and `.inc` files.
- Project scopes use the nearest `compile_commands.json`,
  `compile_flags.txt`, `CMakeLists.txt`, `meson.build`, `BUILD`, `BUILD.bazel`,
  or `.clangd` boundary, with a repository-relative path fallback. Test,
  vendor, and generated roles follow explicit path and filename conventions.
- Tree-sitter syntax intelligence (`tree-sitter-cpp 0.23.4`): translation
  units, namespaces, classes/structs/unions/enums, nested types, functions and
  methods, fields, aliases/concepts, includes, inheritance, byte-accurate
  ranges, diagnostics, common test macros, and bounded static calls.
- Optional clangd call-hierarchy enrichment with revision-scoped document
  synchronization, repository-only result conversion, cancellation, restart,
  and graceful degradation (ADR-0039).
- All seven Chakra queries and MCP exposure, including atomic revisions,
  `require_fresh`, provenance/precision, ambiguity reporting, budgets,
  truncation, cancellation, and current Git diff context.

## Install and runtime requirements

Syntax intelligence is fully offline: no compiler, CMake, LLVM, build-system
execution, language server, or network service is required. Chakra never runs
the indexed project's build configuration.

Precise enrichment optionally uses clangd 21+ from an LLVM distribution. Put
`clangd` on `PATH` or pass `--clangd-path`; use `--no-clangd` for deterministic
syntax-only operation. Chakra starts the route only for a precise C++ query and
reserves 2 GiB in the bounded provider pool while it is active.

For project-accurate results, provide a current `compile_commands.json` or
`compile_flags.txt` in the worktree. Without one, clangd may infer fallback
flags; Chakra keeps serving syntax facts and does not fabricate precision.

## Precision tiers and limitations

- Precise: repository-local incoming and outgoing call hierarchy reported by
  clangd for the published document revision.
- Syntax: declarations, containers, includes, inheritance candidates, ranges,
  diagnostics, test hints, and static call sites.
- Heuristic: uniquely resolved local free-function and qualified-call edges.
- Textual: plain text search hits.

C++ semantics depend heavily on preprocessing and compilation flags. The
syntax tier does not expand macros, instantiate templates, perform overload or
argument-dependent lookup, infer arbitrary receiver types, or model virtual
dispatch. An unqualified call inside a method is conservatively treated as a
member candidate, so a free function reached from a method can remain
unresolved at syntax tier (issue #83). Such calls stay unresolved or
ambiguous. Namespace-qualified out-of-line free-function definitions can be
classified as methods until bounded cross-file qualifier evidence is available
(issue #84). Headers are indexed as C++ syntax; `.c` files use the compatible
C subset of the selected grammar.

clangd results outside captured workspace documents are omitted. Provider
absence, missing call-hierarchy capability, crash, timeout, or cancellation
leaves the syntax graph available and reports degradation.

## Evidence

- Conformance: `docs/support/conformance/cpp.json` (14/14 scenarios).
- Adapter tests: `crates/chakra-language-cpp/tests/fixture_index.rs` and
  parser/indexer unit tests.
- Provider tests: `crates/chakra-provider-clangd/tests/lifecycle.rs` and the
  opt-in `tests/real_provider.rs` smoke test.
- Live and MCP tests: `crates/chakra-language/tests/live_updates.rs` and
  `crates/chakra-mcp/tests/contract.rs`.
- Corpus: `docs/support/corpus/results/cpp-nlohmann__json.json` and
  `docs/support/corpus/results/cpp-protocolbuffers__protobuf.json`.
