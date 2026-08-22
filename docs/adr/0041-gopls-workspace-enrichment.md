# ADR-0041: gopls workspace enrichment

Status: accepted
Date: 2026-08-21

## Context

ADR-0027 selected gopls 0.23.x as the optional precise provider for Go.
Chakra also needs useful Go intelligence when neither the Go toolchain nor
gopls is installed, and it must keep provider protocol state outside the
domain and query layers.

Go modules and workspaces can contain build-tagged files, tests, generated
sources, vendored packages, generic declarations, interfaces, and method
calls whose targets require type information. Tree-sitter can represent the
stable syntax facts, while gopls supplies revision-scoped call hierarchy for
type-directed relationships.

## Decision

- Go syntax support is fully offline and uses `tree-sitter-go` 0.25.0 for
  package/file containers, imports and build constraints, declarations,
  fields, interface methods, functions, methods, Go test entry points,
  diagnostics, and bounded call candidates.
- Git-visible `go.mod` files define nearest module scopes by their `module`
  directive. A `go.work` directory supplies a workspace fallback for sources
  outside a nested module. `go.sum` and `go.work.sum` are freshness inputs,
  not source documents.
- The CLI registers gopls as a dormant Go route in the bounded provider pool.
  Activation reserves 768 MiB; an inactive route owns no process or memory
  reservation.
- Chakra invokes `gopls serve`. Discovery checks `PATH` only and never
  installs Go or gopls. `--gopls-path` selects an executable and `--no-gopls`
  disables the route.
- Readiness requires the LSP call-hierarchy capability. After the selected
  document and revision delta are synchronized, a
  `textDocument/prepareCallHierarchy` round trip is the request barrier.
- Incoming and outgoing relations come from gopls call hierarchy, are clipped
  to captured Go workspace documents, and carry `Provenance::Gopls` with
  precise precision. Syntax relations remain available as fallback.
- The shared `chakra-lsp` transport owns bounded messages and queues,
  cancellation, restart/backoff, revision deltas, process-group shutdown, and
  failure isolation. A missing executable or capability, crash, timeout, or
  cancellation never removes the published syntax graph.
- All Git-visible `.go` files are indexed. `_test.go`, vendor, and generated
  path/name conventions receive explicit source roles. Build constraints are
  recorded as syntax facts but are not evaluated to remove files from the
  materialized workspace graph.

## Alternatives considered

- **Require gopls for Go support.** Rejected because the local syntax graph,
  queries, diffs, and freshness guarantees must work without an external
  language server or toolchain.
- **Resolve selector calls from variable names at syntax tier.** Rejected
  because a receiver expression usually does not prove its Go type. Such
  calls remain unresolved or ambiguous until gopls supplies precise evidence.
- **Run `go list` during discovery.** Rejected for the offline core because it
  would make indexing depend on an installed toolchain, module downloads, and
  environment-specific build settings.
- **Evaluate build tags for the host platform.** Rejected because Chakra's
  syntax snapshot represents the Git-visible worktree, while provider
  enrichment may reflect the active gopls build configuration.

## Consequences

- Go has the same syntax, query, freshness, provenance, degradation, and
  public-corpus gates as the other first-class languages.
- Free-function calls with one local declaration resolve heuristically.
  Selector dispatch, interface implementations, embedded promotion, build
  selection, and cross-package type resolution remain gopls responsibilities.
- Files excluded by the current Go build configuration can still appear in
  the syntax graph, with their build constraints visible as import facts.
- `go.mod`, `go.sum`, `go.work`, and `go.work.sum` are revision-bound provider
  inputs and produce watched-file events independently of source-document
  deltas (ADR-0042).

## Validation / follow-up

- Parser and fixture tests cover packages, imports, build constraints,
  structs, interfaces, fields, methods, generics, `_test.go`, source roles,
  diagnostics, calls, and incremental reconciliation.
- Hermetic provider tests cover the call-hierarchy capability gate, revision
  synchronization, cancellation, timeout, restart, degradation, and
  orphan-free shutdown. The opt-in smoke test passed against gopls 0.23.0:
  initial incoming-call enrichment completed in about 114 ms and a revision
  edit exposed the second caller in under 1 ms.
- Go conformance passes 14/14 scenarios. The pinned Prometheus and Kubernetes
  corpora each pass 12/12; observed release cold indexes were about
  0.8–7.6 seconds with 248–1591 MiB peak RSS.
