# C# language support

Status: first-class (see `docs/support/languages/csharp.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0027; shared LSP
client record: ADR-0032; runtime/result-boundary record: ADR-0037.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.cs` files.
- .NET project scopes and language-neutral roles: each `*.csproj` is a
  project boundary; `AssemblyName` and `IsTestProject` are read with bounded
  input; nested projects select the nearest boundary. `*.sln`, `*.slnx`,
  `Directory.Build.props/targets`, `Directory.Packages.props`, `global.json`,
  and project files are revision freshness inputs. `bin`/`obj` sources are
  generated; test projects, test paths, and `*Test.cs`/`*Tests.cs` are tests.
- Tree-sitter syntax intelligence (`tree-sitter-c-sharp 0.23.5`): block and
  file-scoped namespaces; classes, partial/generic classes, structs,
  interfaces, records, enums, delegates, nested types; methods, async and
  extension methods, constructors, properties, events, indexers, fields,
  enum members, operators, and local functions; `using`, aliases, and
  `using static`; base-type relations; byte-accurate ranges; actionable
  diagnostics; and bounded lazy call candidates, including scope-checked
  local extension-method candidates.
- xUnit (`Fact`/`Theory`), NUnit (`Test`/`TestCase`/`TestCaseSource`), and
  MSTest (`TestMethod`/`DataTestMethod`) attributes identify test symbols.
- A tested `csharp-ls` adapter for precise call hierarchy with revision-scoped
  C# document synchronization and `Provenance::CsharpLs`. The CLI registers it
  as a dormant route and activates it only for a precise C# query.
- All seven Chakra queries and MCP exposure, including atomic revisions,
  `require_fresh`, provenance/precision, ambiguity reporting, budgets,
  truncation, cancellation, and syntax fallback.

## Install and runtime requirements

- Syntax intelligence is fully offline: no .NET SDK, MSBuild, solution load,
  restore, or language server is required.
- Precise enrichment optionally uses `csharp-ls` 0.26.x and the .NET 10 SDK
  or later. A reproducible install is:

  ```sh
  dotnet tool install --global csharp-ls --version 0.26.0
  ```

  Put `csharp-ls` on `PATH` or pass `--csharp-ls-path`; use
  `--no-csharp-ls` for syntax-only operation. Chakra does not enable metadata
  URIs, and drops provider locations outside the captured worktree
  (ADR-0037). Status reports the configured route as `dormant` until use.

## Precision tiers and limitations

- Precise: `csharp-ls` call hierarchy for the pinned workspace revision.
- Syntax: declarations, containers, imports, ranges, diagnostics, call sites.
- Heuristic: resolved call and base-type relations.
- Textual: plain text search hits.

The syntax tier does not perform overload resolution, generic type inference,
extension-method type applicability/precedence, partial-type semantic merging,
or MSBuild condition evaluation. A local extension-syntax call resolves only
when its name and imported/current namespace identify one extension container;
overloads in that container stay ambiguous, and calls that need type binding
stay unresolved. Other calls through untyped receivers likewise remain
unresolved/ambiguous. A class or record's first `base_list` item is
syntactically ambiguous between a base class and an interface; Chakra corrects
the edge to `implements` when an indexed interface target resolves uniquely,
otherwise keeps the conservative candidate for the precise provider. Project
references affect `csharp-ls` workspace semantics but are not fabricated as
symbol graph edges.

Calls inside lambdas/anonymous methods are not attributed to the enclosing
method, because they have no stable syntax-tier callable identity. External
SDK/dependency definitions exposed only as `csharp:/` metadata URIs are
intentionally absent from Chakra results.

Project metadata changes (`.csproj`, `.sln`, `.slnx`, `Directory.Build.*`,
and `global.json`) are captured in the workspace revision and sent to an
active `csharp-ls` instance as watched-file events. They never masquerade as
text-document changes.

## Evidence

- Conformance: `docs/support/conformance/csharp.json` (14/14 scenarios).
- Adapter tests: `crates/chakra-language-csharp/tests/fixture_index.rs` and
  parser/indexer unit tests.
- Provider tests: `crates/chakra-provider-csharp-ls/tests/lifecycle.rs`.
- Discovery/classification: `crates/chakra-git/src/discovery.rs` and
  `crates/chakra-git/src/source_metadata.rs` tests.
- Corpus: `docs/support/corpus/results/csharp-dotnet__runtime.json`.
