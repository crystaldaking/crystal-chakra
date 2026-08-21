# ADR-0040: terraform-ls reference enrichment

Status: accepted
Date: 2026-08-21

## Context

ADR-0027 selected terraform-ls 0.39.x as the optional HCL/Terraform provider.
The server advertises definitions, references, document symbols, and workspace
symbols, but not call hierarchy or type hierarchy. Terraform configuration
also has no honest function-call interpretation for resource, data, variable,
local, module, and output traversals.

Chakra needs useful HCL relationship queries without labelling configuration
entities as functions or making Terraform, OpenTofu, provider downloads, or a
language server prerequisites for the syntax tier.

## Decision

- HCL syntax support remains fully offline and uses `tree-sitter-hcl` 1.1.0
  for Terraform/OpenTofu and generic HCL declarations, imports, diagnostics,
  test hints, and bounded traversals.
- The domain adds `CallTargetKind::Configuration`. Terraform resources and
  traversals use this kind while retaining the existing `Calls` query edge;
  user-visible evidence therefore distinguishes a configuration reference
  from a function call.
- The CLI registers terraform-ls as a dormant HCL route in the bounded
  provider pool. Activation reserves 512 MiB; an inactive route owns no
  process or reservation.
- Chakra invokes `terraform-ls serve`. Discovery checks `PATH` only and never
  installs terraform-ls, Terraform/OpenTofu, or providers.
  `--terraform-ls-path` selects an executable and `--no-terraform-ls`
  disables the route.
- Readiness requires definitions, references, and document symbols. After
  revision-scoped document synchronization, `textDocument/references` is the
  request barrier because terraform-ls exposes no separate quiescence signal.
- Incoming relations are obtained from references and attributed to the
  narrowest enclosing provider document symbol. They are clipped to captured
  HCL documents and carry `Provenance::TerraformLs` with precise precision.
  Outgoing relations remain Chakra's bounded syntax configuration-reference
  equivalent because the provider has no call hierarchy.
- `.tf` and `.tftest.hcl` files use the `terraform` language id; `.tfvars`
  uses `terraform-vars`; generic `.hcl` uses `hcl`.
- The shared `chakra-lsp` transport owns bounded messages and queues,
  cancellation, restart/backoff, revision deltas, process-group shutdown, and
  failure isolation. Missing capabilities, executable, crash, timeout, or
  cancellation leave syntax results available.
- Directories containing Git-visible `.tf` files define nearest
  Terraform module scopes. Lock/version/config files are revision freshness
  inputs but not source documents. `.terraform` and `.terragrunt-cache` are
  vendor paths; Terraform test files have the test role.

## Alternatives considered

- **Treat traversals as function calls.** Rejected because it would give
  misleading semantics to configuration references.
- **Require terraform-ls for all HCL queries.** Rejected because generic HCL
  and Terraform syntax intelligence must remain local, deterministic, and
  available without external tooling.
- **Build an eager schema graph from downloaded provider packages.** Rejected
  for v0.1.2 because it adds network/cache/toolchain state outside the
  Git-derived workspace revision and expands the milestone beyond the parity
  contract.
- **Publish provider locations outside captured documents.** Rejected because
  those locations cannot carry the workspace revision's freshness guarantee.

## Consequences

- `callers` and `context` can answer Terraform reference questions at syntax
  tier and can precisely confirm incoming references when terraform-ls is
  active, without conflating resources with functions.
- Syntax resolution is intentionally bounded. Dynamic expressions,
  provider-schema semantics, computed module sources, `for_each`/`count`
  instances, and references created through indirection may remain unresolved
  or ambiguous.
- Provider document-symbol attribution omits top-level references that have no
  enclosing configuration symbol. Locations outside the captured worktree are
  omitted.
- Terraform locks, version files, and CLI configuration are revision-bound
  provider inputs and produce watched-file events independently of source
  document deltas (ADR-0042).
- Terraform JSON configuration and variable/test forms are not indexed by the
  native-HCL grammar; a dedicated JSON adapter path remains tracked in issue
  #86.

## Validation / follow-up

- The capability probe passed against terraform-ls 0.39.0 for definition,
  references, document symbols, workspace symbols, and text synchronization;
  call hierarchy and type hierarchy were absent as expected.
- The opt-in real-provider test returned one precise incoming reference and
  then two after a revision edit. Hermetic lifecycle tests cover capability
  gates, synchronization, cancellation, restart, degradation, and orphan-free
  shutdown.
- HCL conformance passes 14/14 scenarios. The pinned terraform-aws-vpc and
  terraform-aws-eks corpora each pass 12/12; observed release cold indexes were
  about 0.12–0.13 seconds with 49–77 MiB peak RSS.
