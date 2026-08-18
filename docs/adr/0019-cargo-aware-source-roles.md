# ADR-019: Cargo-aware, language-neutral source roles

Status: accepted
Date: 2026-08-18

## Context

Large repositories mix production declarations with integration tests,
examples, benches, fixtures, generated code and vendored sources. Returning
these as an undifferentiated symbol stream made exact searches noisy in the
Zed and PSP evaluations. Cargo already describes Rust workspace/package and
target boundaries, but those adapter-specific representations must not leak
into the domain or couple PHP and future languages to Cargo.

Discovery remains Git-aware: tracked plus untracked non-ignored files are the
canonical inventory. Source classification may annotate that inventory but
must never remove a file or alter its repository-relative identity.

## Decision

- Add language-neutral `SourceRole`, `SourceMetadata`, `SourcePackage` and
  coverage types to `chakra-domain`. Every indexed file has exactly one role:
  `production`, `test`, `example`, `bench`, `fixture`, `generated`, or
  `vendor`. Symbols expose the metadata of their declaring file.
- Keep Cargo command/JSON handling inside `chakra-git`. It consumes the shared
  Git source/classification inventory, then invokes `cargo metadata --no-deps
  --offline --locked` with fixed arguments for each uncovered workspace,
  capped at 64 invocations and a shared 30-second Cargo deadline. Output is
  bounded. Workspace members returned by one invocation are not invoked again.
- Package identity is the Cargo package name plus its repository-relative
  root. Exact Cargo production/test/example/bench targets override path
  classification. More specific fixture/generated/vendor path components take
  precedence for files that are not exact targets.
- If Cargo is unavailable, metadata fails, resolution would require a lockfile
  update, or the command/inventory budget is exceeded, indexing continues with a
  deterministic language-neutral path classifier. Coverage counts on
  `status` and `repo_map` distinguish Cargo-classified files from fallback
  files. No fallback fact is labeled as Cargo-derived. ADR-021 later extends
  the same language-neutral model with explicit Composer classification.
- Attach metadata to immutable graph files. A manifest-only classification
  change materializes and atomically publishes a new graph revision while
  retaining parsed source contributions; stable source is not reparsed.
- Add bounded `package`, repository-relative `path_prefix`, `include_roles`
  and `exclude_roles` filters to `repo_map` and `symbol_search`. Empty filters
  preserve all existing results. Filtering happens before the response limit,
  so excluded roles cannot consume the caller's result budget.
- Keep ranking separate from classification. ADR-020 consumes these source
  roles to rank production declarations ahead of fixture/import noise while
  also handling exact-name and symbol-kind relevance.
- Do not add `.chakra.toml` in v0.1.1. Built-in rules are deterministic and
  observable; project-specific policy would add configuration discovery,
  invalidation and precedence semantics without evaluation evidence yet.
  Reconsider a bounded policy only after real repositories demonstrate
  unavoidable false classifications, and specify it in a separate ADR.

## Alternatives considered

- Put Cargo target enums in query/domain types: rejected because shared query
  contracts must remain useful for PHP and future adapters.
- Exclude fixtures/generated/vendor files during indexing: rejected because a
  role is query metadata, not authorization to erase repository facts.
- Parse Cargo manifests directly: rejected for this slice because reproducing
  workspace inheritance, target discovery and nested-workspace behavior would
  duplicate Cargo semantics. Failed or unsafe-to-run metadata degrades
  explicitly to the bounded fallback instead.
- Run unlocked metadata to classify libraries without lockfiles: rejected
  because a read-only Chakra scan must not create or update `Cargo.lock` or
  access the network.
- Add project-specific configuration immediately: deferred as described above.

## Consequences

- `serde_json`, already workspace-managed elsewhere, is now a production
  dependency of `chakra-git` for the Cargo adapter. No Cargo protocol types
  cross into domain, engine, language or MCP layers.
- Cargo metadata work is bounded but can add reconciliation latency in large
  workspaces. ADR-005 pins manifest, lockfile, toolchain, and Cargo-config
  inputs in the same pre/post inventory and identity proof as sources, while
  retaining parsed source bodies across classification-only changes.
- A non-Cargo Rust file and PHP files without a usable Composer mapping remain
  queryable with explicit fallback classification. Partial coverage is normal
  and machine-visible.
- Source metadata is revision-scoped with the graph; callers never combine a
  new package classification with symbols from an older revision.

## Validation / follow-up

- Real temporary Git repositories cover workspace members, an independent
  nested workspace, exact Cargo targets, path fallback, ignored generated
  content and a staged rename.
- Engine tests cover default reachability, package/path/include/exclude
  filters across Rust and PHP, coverage output and invalid bounded inputs.
- Live reconciliation tests change package metadata and assert a newer fresh
  revision with zero stable-source reparses.
- ADR-020 supplies relevance ranking tests. ADR-005 records shared-inventory
  and classification-only freshness behavior.
