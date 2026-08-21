# HCL language support

Status: first-class (see `docs/support/languages/hcl.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0027; shared LSP
client record: ADR-0032; terraform-ls integration record: ADR-0040.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.tf`, `.tfvars`,
  `.tftest.hcl`, and generic `.hcl` files. Terraform/OpenTofu lock files are
  freshness metadata, not sources.
- Project scopes use the nearest directory containing Git-visible
  `.tf` configuration. Terraform tests, generated paths, and
  `.terraform`/`.terragrunt-cache` vendor trees receive explicit roles.
- Tree-sitter syntax intelligence (`tree-sitter-hcl 1.1.0`): resources, data
  sources, modules, variables, outputs, locals, providers, nested generic HCL
  blocks, provider/module imports, Terraform `run` tests, byte-accurate ranges,
  diagnostics, and bounded configuration traversals.
- Resource/data/module/variable/local/output traversals are represented as
  configuration references, not function calls. Unique local targets produce
  Chakra-owned syntax relations; ambiguity remains explicit.
- A tested terraform-ls adapter confirms repository-local incoming references
  and attributes them to provider document symbols. Outgoing relations remain
  syntax-derived because terraform-ls has no call-hierarchy capability
  (ADR-0040).
- All seven Chakra queries and MCP exposure, including atomic revisions,
  `require_fresh`, Git diff context, provenance/precision, budgets,
  truncation, cancellation, and graceful provider degradation.

## Install and runtime requirements

Syntax intelligence is fully offline: Terraform, OpenTofu, provider plugins,
terraform-ls, and network access are not required. Chakra never evaluates the
indexed configuration or downloads providers.

Precise incoming-reference enrichment optionally uses terraform-ls 0.39.x.
Install a pinned release from HashiCorp, put `terraform-ls` on `PATH`, or pass
`--terraform-ls-path`; use `--no-terraform-ls` for deterministic syntax-only
operation. Chakra starts `terraform-ls serve` only for a precise HCL query and
reserves 512 MiB in the bounded provider pool while the route is active.

Terraform CLI/OpenTofu and initialized provider schemas can improve the
language server's schema-aware behavior, but Chakra does not install or invoke
those tools itself. Missing schema/tooling state degrades only precise
enrichment; syntax facts remain available.

## Precision tiers and limitations

- Precise: repository-local incoming references confirmed by terraform-ls for
  the published document revision.
- Syntax: declarations, containers, imports, ranges, diagnostics, test hints,
  and bounded traversal candidates.
- Heuristic: uniquely resolved configuration-reference relations.
- Textual: plain text search hits.

The syntax tier does not evaluate expressions, expand dynamic blocks, resolve
computed module sources, model `for_each`/`count` instances, or load provider
schemas. Indirect and dynamically indexed traversals may stay unresolved or
ambiguous. Generic HCL is parsed and queryable, but Terraform-specific symbol
kinds apply only to recognized Terraform block shapes.

Terraform JSON configuration and variable/test forms are not indexed because
the selected grammar parses native HCL only; dedicated JSON syntax support is
tracked in GitHub issue #86.

Provider references with no enclosing document symbol and locations outside
captured workspace documents are omitted. Provider absence, missing
capabilities, crash, timeout, or cancellation leaves the syntax graph
available and reports degradation.

## Evidence

- Conformance: `docs/support/conformance/hcl.json` (14/14 scenarios).
- Adapter tests: `crates/chakra-language-hcl/tests/fixture_index.rs` and
  parser/indexer unit tests.
- Provider tests: `crates/chakra-provider-terraform-ls/tests/lifecycle.rs` and
  the opt-in `tests/real_provider.rs` smoke test.
- Live and MCP tests: `crates/chakra-language/tests/live_updates.rs` and
  `crates/chakra-mcp/tests/contract.rs`.
- Corpus: `docs/support/corpus/results/hcl-terraform-aws-modules__terraform-aws-vpc.json`
  and `docs/support/corpus/results/hcl-terraform-aws-modules__terraform-aws-eks.json`.
