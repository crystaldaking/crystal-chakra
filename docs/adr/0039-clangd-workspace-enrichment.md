# ADR-0039: clangd workspace enrichment

Status: accepted
Date: 2026-08-21

## Context

ADR-0027 selected clangd 21+/22.x as the optional C/C++ provider. clangd
advertises definitions, references, call hierarchy, and workspace symbols,
but project-accurate semantics depend on a compilation database or equivalent
flags. Background indexing can also consume gigabytes on a large workspace.

Chakra needs precise C/C++ callers without making clangd, a compiler, or a
build-system invocation a startup requirement. Provider results must remain
revision-scoped, repository-local, bounded, and distinguishable from the
Tree-sitter graph.

## Decision

- C/C++ syntax support remains fully offline and uses `tree-sitter-cpp`
  0.23.4 for translation units and headers, declarations, includes,
  diagnostics, test hints, and bounded static call candidates.
- The CLI registers clangd as a dormant C++ route in the bounded provider
  pool. Activation reserves 2 GiB; an inactive route owns no process or
  reservation.
- Chakra invokes `clangd --background-index --limit-results=500 --log=error`.
  Discovery checks `PATH` only and never installs LLVM. `--clangd-path`
  selects an explicit executable and `--no-clangd` disables the route.
- A compilation database (`compile_commands.json`) is preferred. CMake,
  Meson, Bazel, `compile_flags.txt`, and `.clangd` files are captured as
  freshness/classification inputs, but Chakra does not execute a build system
  to generate flags. Missing project flags reduce provider accuracy and never
  remove syntax results.
- Readiness requires LSP call hierarchy. After revision-scoped document
  synchronization, `textDocument/prepareCallHierarchy` is the request barrier
  because clangd exposes no separate quiescence signal.
- Incoming and outgoing relations are obtained from clangd call hierarchy,
  clipped to captured C/C++ documents, and carry `Provenance::Clangd` with
  precise precision. Results outside the materialized worktree are dropped.
- The shared `chakra-lsp` transport owns the process group, bounded message
  and request queues, cancellation, restart/backoff, revision deltas, and
  orphan-free shutdown. Missing capability, executable, crash, timeout, or
  cancellation degrades to the current syntax graph.
- The provider route accepts `.c`, `.h`, `.cc`, `.cpp`, `.cxx`, `.hh`, `.hpp`,
  `.hxx`, `.ipp`, `.tpp`, and `.inc` documents. `.c` uses the LSP language id
  `c`; the remaining documents use `cpp`.

## Alternatives considered

- **Run clangd eagerly.** Rejected because most queries need only syntax facts
  and the background index has a material memory cost.
- **Generate `compile_commands.json` inside Chakra.** Rejected because that
  would execute arbitrary project build configuration and cross the product's
  shell-execution boundary.
- **Publish clangd's external/SDK locations.** Rejected because those files
  are not captured by the current workspace revision and cannot carry its
  freshness guarantee.
- **Use libclang in the core graph.** Rejected because it couples core
  architecture to one language provider and still requires project flags.

## Consequences

- C++ callers can be precise when clangd has usable compile flags, while the
  same queries stay available at syntax/heuristic tiers without LLVM.
- Templates, macros, overloads, ADL, virtual dispatch, and generated include
  paths may exceed the syntax tier; Chakra reports unresolved or ambiguous
  candidates rather than guessing.
- Build metadata is a freshness input, but the shared provider workspace
  currently synchronizes captured source documents only. Extending provider
  deltas to non-source inputs remains tracked in issue #71.

## Validation / follow-up

- The capability probe passed against Apple clangd 21.0.0 with definition,
  references, call hierarchy, workspace symbols, document symbols, rename,
  and text synchronization advertised.
- A real-provider test returned one precise incoming call initially and two
  after a revision edit; hermetic lifecycle tests cover synchronization,
  restart, timeout/cancellation, degradation, and shutdown.
- C++ conformance passes 14/14 scenarios. The pinned `nlohmann/json` and
  `protocolbuffers/protobuf` corpora each pass 11/11 within committed budgets.
