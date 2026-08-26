# Changelog

All notable changes to Chakra are documented here. Releases use semantic
version tags prefixed with `v`.

## [Unreleased]

## [0.1.3] - 2026-08-26

Post-v0.1.2 correctness, query ergonomics, corpus resilience, dependency
hygiene, and targeted maintainability follow-ups (milestone v0.1.3), plus the
agent-facing process rules the milestone was developed under: develop Chakra
with Chakra and file every discovered problem as an issue (`AGENTS.md`), and
record every pre-1.0 compatibility break explicitly (ADR-0043).

### Tooling

- The public-corpus fetcher retries transient Git transport failures with a
  bounded attempt budget and backoff, surfaces captured Git stderr, and fails
  closed; hermetic tests pin classification and attempt bounds (issue #69).
  Standard Python bytecode caches are ignored and the duplicate Git helper was
  removed, so the documented tooling test no longer dirties a clean checkout
  (issue #115).
- The accepted cargo-deny duplicate baseline (bitflags 1.3.2, syn 2.0.119,
  windows-sys 0.60.2) is recorded as exact-version skip entries with reasons
  and re-evaluation triggers, so new duplicates keep warning (issue #88).
- The Git cancellation/reaping regression uses an idle owned process and
  bounded parked startup/completion waits instead of CPU-spinning fixtures and
  load-sensitive subsecond assertions (issue #106).
- The public-corpus provider restart check tolerates only transient revision
  catch-up while preserving exact revision safety and hard failures for
  degraded providers (issue #118).

### Changed

- The eight `chakra-lsp`-based provider adapters (vtsls, pyright, clangd,
  gopls, csharp-ls, jdtls, bash-language-server, terraform-ls) now share one
  worker scaffolding crate, `chakra-provider-worker` (issue #94): the
  owner-thread event loop, session lifecycle with bounded restart/backoff,
  revision-scoped document synchronization, the post-synchronization request
  barrier, observability, cancellation, shutdown, and LSP-to-domain
  conversion. Language-specific seams are typed hooks (name/provenance,
  language ids, capability gates, readiness budgets, cold-start semantics,
  query strategies, mid-query document open) instead of ~1,000 copied lines
  per provider. rust-analyzer keeps its own worker: its custom transport,
  result cache, and `experimental/serverStatus` readiness are deliberately
  provider-specific. Public adapter APIs and hermetic lifecycle contract
  tests are unchanged.
- The eight syntax adapters with the common per-file fact shape (Go, Java,
  HCL, TypeScript, JavaScript, Python, Shell, C++) now share one indexing
  driver crate, `chakra-language-index` (issue #94): cold-build and
  reconcile scheduling, bounded parser workers, metrics and limits,
  relationship materialization, and graph publication. Language seams are
  typed `LanguageHooks` (parser, Git-aware discovery, worker naming, optional
  post-parse evidence pass — the C++ reclassification/promoted-call passes
  from this release). Rust (impl-block drafts), PHP (receiver-aware call
  resolution), and C# (extension-method delta machinery) keep their own
  indexers because their index semantics are genuinely language-specific.
  Public adapter APIs and adapter tests are unchanged.

### Added

- Terraform JSON syntax support (issue #86): `.tf.json`, `.tfvars.json`, and
  `.tftest.json` files are discovered as HCL-language sources and parsed
  through `tree-sitter-json` into the native-HCL entity model. JSON escapes
  are decoded for identities and expressions while ranges retain original
  source bytes; Terraform's literal exceptions and template escaping prevent
  false references (issue #112). Path-aware provider routing keeps these
  syntax-only documents out of terraform-ls and its lifecycle metrics (issue
  #113). Conformance, live-update/diff/MCP checks, and the pinned cztack corpus
  cover the feature; the public corpus now contains 20 repositories.
- `symbol_search` accepts `match_mode: "exact"` (issue #82): matching is
  limited to the exact case-folded simple/qualified name index, existing
  language/kind/source filters and budgets still apply, and truncation is
  reported only when the exact candidate set itself exceeds a bound. The
  default substring ranking is unchanged.

### Fixed

- C++ namespace-qualified free-function definitions such as `void ns::free()`
  are no longer misclassified as methods (issue #84): qualified callable
  definitions are reclassified against workspace type/namespace evidence
  during graph build and reconcile, an unproven qualifier keeps the
  conservative parse-time kind, and no file is reparsed for the pass.
- Unqualified calls inside C++ methods no longer commit to the member
  interpretation too early (issue #83): with workspace evidence a call
  resolves to a unique free function, reports honest ambiguity when several
  free functions match, stays a member call when only a same-type member
  exists, and retains both declaration domains as bounded ambiguous evidence
  on a genuine member/free collision (issue #111). clangd remains the precise
  path.
- Hook-induced structural changes in the shared language-index reconciler now
  participate in the graph delta even when their source file was not reparsed.
  C++ namespace evidence can therefore reclassify a retained definition in the
  same atomic revision without forcing a full index, and adding or removing
  that evidence reverses the classification deterministically (issues #114
  and #117).
- Detailed indexing phase history is returned by `status` instead of being
  repeated in every small query envelope. Non-status queries still carry the
  exact revision's coverage, capability, degradation, memory, scheduling, and
  publication evidence (issue #107).

### Breaking

- `status` now reports workspace-global provider-pool lifecycle/admission
  counters once under `data.provider_pool` instead of repeating them inside
  every `data.providers[].metrics.orchestration` entry (issue #61, policy
  ADR-0043). Per-provider `metrics` keeps only provider-local `cache` and
  `document_sync` sections. Query-local fallback metadata continues to explain
  pool saturation or queue timeouts through the typed `fallback_cause`/
  `fallback_reason` fields.
- Response schema 14 identifies the changed v0.1.3 status contract and the
  explicit `function_or_method` syntax target used for honest C++ collisions;
  v0.1.2 schema 13 is not reused (issue #108).

## [0.1.2] - 2026-08-21

### Added

- A first-class language parity contract and generated support matrix for all
  11 advertised languages (issues #22–#25): independent fixtures, a shared
  14-scenario conformance suite, and 12 release scenarios exercised across 19
  pinned public repositories. Machine-readable evidence prevents a language
  from being advertised before every mandatory gate passes.
- First-class Go support (issue #36): `tree-sitter-go` 0.25.0 syntax facts,
  `go.mod`/`go.work` project scopes, test/generated/vendor roles, bounded call
  candidates, live-update and MCP coverage, and optional on-demand gopls
  0.23.x call-hierarchy enrichment with revision synchronization, failure
  isolation, and owned shutdown (ADR-0041). The Prometheus and Kubernetes
  corpora pass all release scenarios.
- First-class HCL/Terraform support (issue #35): `tree-sitter-hcl` 1.1.0,
  Terraform module scopes and resource/module/data/output relationships,
  plus optional terraform-ls reference enrichment through the shared LSP
  lifecycle. The pinned terraform-aws-vpc and terraform-aws-eks corpora pass
  all release scenarios.
- First-class C++ support (issue #34): `tree-sitter-cpp` 0.23.4 across common
  translation-unit/header extensions, CMake/compile database scopes, bounded
  syntax relationships, and optional clangd call hierarchy. The nlohmann/json
  and protocolbuffers/protobuf corpora pass all release scenarios.
- First-class Shell support (issue #33): `tree-sitter-bash` 0.25.1 across
  sh/bash/zsh/ksh paths, ShellCheck project boundaries, syntax-derived
  function calls, and optional bash-language-server navigation enrichment.
  The ohmyzsh/ohmyzsh and nvm-sh/nvm corpora pass all release scenarios.
- First-class C# support (issue #31): `tree-sitter-c-sharp` 0.23.5, `.csproj`
  project scopes, C# declarations/tests/call candidates, and optional
  csharp-ls call-hierarchy enrichment. The pinned dotnet/runtime corpus passes
  all release scenarios within calibrated degraded-workspace budgets.
- Existing Rust and PHP support now pass the same full parity contract as the
  newly added languages (issues #32 and #37), including independent
  conformance, public-corpus, freshness, provenance, cancellation,
  degradation, MCP, and provider/equivalent-precision evidence.
- First-class Java support (issue #30): a `chakra-language-java` adapter
  crate (tree-sitter-java 0.23.5, `.java`) registered in the ADR-0031
  adapter registry after JavaScript; class/interface/enum/record/annotation
  declarations with methods, fields, and constructors (recorded as methods
  named `constructor`), package and nested-class containers, single-type /
  static / wildcard import facts, `extends`/`implements` relations, JUnit
  4/5 `@Test` test hints, bounded lazy call candidates (bare simple-name,
  `this.`, static-style `Type.`, `new X()` constructors, static-import
  aliases), and actionable syntax diagnostics. Maven `pom.xml` module
  scopes and Gradle `settings.gradle(.kts)`/`build.gradle(.kts)` project
  boundaries plus `src/main/java` vs `src/test/java` and
  `Test*.java`/`*Test.java`/`*Tests.java` source roles. The new
  `chakra-provider-jdtls` precise provider over chakra-lsp (additive
  `Provenance::Jdtls`) owns the jdtls lifecycle: per-workspace data
  directory under the OS tempdir (workspace-bound defaults and validation
  reject relative/repository-contained cache paths) and a configurable
  readiness bound for the slow first project import (ADR-0036). Conformance
  fixture (14/14 scenarios) and the pinned spring-projects/spring-boot plus
  apache/kafka corpus evaluations. Java is advertised first-class.
- Bounded multi-provider orchestration (issue #26, ADR-0035):
  rust-analyzer, vtsls (shared by TypeScript/JavaScript), pyright, and jdtls
  now coexist through strict language routing and start only on the first
  precise query. Deterministic active-provider, memory-reservation,
  concurrent-query, and queue limits provide priority/FIFO admission with
  observable syntax fallback under saturation. Idle/LRU eviction, activation
  backoff, cancellation, and owned pool/provider shutdown prevent unbounded
  work or orphan processes; failed eviction cleanup retains its reservation
  and is retried observably. Provider status distinguishes `dormant` from
  `not_configured` and reports pool lifecycle/capacity metrics.
- First-class JavaScript/JSX support (issue #29): a
  `chakra-language-javascript` adapter crate (tree-sitter-javascript
  0.25.0, `.js`/`.jsx`/`.mjs`/`.cjs`; the single grammar parses JSX
  natively) registered in the ADR-0031 adapter registry after Python; ES
  module import/export and alias facts, CommonJS `require()` and
  `module.exports`/`exports` facts (ADR-0034), class/nested-function
  containers, `extends` relations, jest/vitest/mocha test hints, bounded
  lazy call candidates with relative-import and require-alias resolution,
  and actionable syntax diagnostics. package.json project scopes plus
  jsconfig.json boundaries and `__tests__/`/`*.test.*`/`*.spec.*` source
  roles; the existing `chakra-provider-vtsls` precise provider now also
  serves JavaScript documents (`javascript`/`javascriptreact` language
  ids, shared `Provenance::Vtsls`, no new provider crate). Conformance
  fixture (14/14 scenarios) and the pinned react/react corpus evaluation
  (12/12, no degradations). JavaScript is advertised first-class.
- First-class Python support (issue #28): a `chakra-language-python`
  adapter crate (tree-sitter-python 0.25.0, `.py`/`.pyi`) registered in the
  ADR-0031 adapter registry after TypeScript; `import`/`from ... import`
  and alias facts (including relative imports), module/class/function
  containers with decorators recorded, base-class relations, pytest/unittest
  test hints, bounded lazy call candidates (bare, `self.`/`cls.`,
  module-alias, `ClassName()` constructor, and single-base `super()` calls),
  and actionable syntax diagnostics. pyproject.toml project scopes plus
  setup.py/setup.cfg boundaries and `test_*.py`/`*_test.py` source roles; a
  `chakra-provider-pyright` precise provider over the shared chakra-lsp
  client (additive `Provenance::Pyright`); conformance fixture (14/14
  scenarios) and the pinned django/django + apache/airflow corpus
  evaluations (12/12 each, no degradations). Revision-local entity ids now
  use the ADR-0033 slot registry (4-bit language slot + 60-bit counter)
  instead of hardcoded per-language bases; ids remain in-memory only.
- First-class TypeScript/TSX syntax support (issue #27, Part A): a
  `chakra-language-typescript` adapter crate (tree-sitter-typescript 0.23.2,
  TypeScript grammar for `.ts`/`.mts`/`.cts`, TSX grammar for `.tsx`)
  registered in the ADR-0031 adapter registry after PHP; ES module
  import/export and alias facts, namespace/class containers, heritage
  relations, jest/vitest/mocha test hints, bounded lazy call candidates with
  relative-import resolution, and actionable syntax diagnostics.
  package.json/tsconfig.json project scopes plus `*.test.*`/`*.spec.*`/
  `__tests__/` source roles; conformance fixture (14/14 scenarios) and the
  pinned microsoft/vscode corpus evaluation (12/12, degraded as designed by
  the workspace source-byte and symbol budgets).
- Shared `chakra-lsp` stdio client crate and the `chakra-provider-vtsls`
  precise provider for TypeScript (issue #27, Part B, ADR-0032): precise
  definitions/references/callers with bounded readiness, revision-scoped
  synchronization, cancellation, restart, and orphan-free shutdown; additive
  `Provenance::Vtsls`. TypeScript is advertised first-class.

### Changed

- The structured query/MCP response envelope schema is version 13 after the
  additive provider-pool, source-classification, provenance, repository-map,
  and per-language coverage extensions in this release.
- The project README now provides a complete source installation path, current
  Codex MCP configuration, tool guide, trust/freshness semantics, architecture,
  validation evidence, limitations, and contributor entry points.

### Performance and reliability

- Provider inputs are revision-bound and provider metadata deltas publish
  atomically with the syntax graph. All LSP adapters receive watched-file
  changes, while metadata-only updates and true no-delta revisions avoid
  rebuilding unchanged document contributions (ADR-0042).
- On macOS, live indexing uses notify's kqueue backend with bounded,
  non-recursive Git-visible source-directory watches, avoiding the observed
  FSEvents registration stall while preserving targeted fresh reconciliation
  (ADR-0005). Watcher startup, cancellation, joining, and test ownership are
  bounded and observable.
- Public-corpus release gates require an optimized binary, isolate scenarios
  by process, and count distinct callers so cross-scenario state and repeated
  call sites cannot create false-positive readiness evidence.

## [0.1.1] - 2026-08-18

### Added

- Receiver-aware PHP method resolution with bounded typed evidence, inheritance
  and trait precedence, plus receiver-aware deduplicated test relationships
  ([#1](https://github.com/crystaldaking/crystal-chakra/issues/1),
  [#6](https://github.com/crystaldaking/crystal-chakra/issues/6)).
- Deterministic Laravel container, route, dispatch, listener, scheduler,
  command and policy enrichment when Composer metadata confirms the framework
  ([#8](https://github.com/crystaldaking/crystal-chakra/issues/8)).
- Direct-base and merge-base `diff_context` scopes with explicit baseline
  semantics ([#3](https://github.com/crystaldaking/crystal-chakra/issues/3)).
- Actionable bounded Tree-sitter recovery diagnostics and known grammar-gap
  classification ([#7](https://github.com/crystaldaking/crystal-chakra/issues/7)).
- Cargo/Composer-aware package and source-role classification shared by
  repository and symbol filters
  ([#12](https://github.com/crystaldaking/crystal-chakra/issues/12)).
- Typed per-section truncation causes, construction-work counters and MCP
  serialization measurements
  ([#9](https://github.com/crystaldaking/crystal-chakra/issues/9)).
- A release-only generated Rust/PHP scale gate in routine CI, a pinned public
  Zed evaluation protocol and machine-readable readiness results
  ([#15](https://github.com/crystaldaking/crystal-chakra/issues/15)).

### Changed

- `symbol_search` now ranks production declarations ahead of noise, supports
  language/kind/namespace/source filters and seeds bounded search from a
  case-insensitive exact-name index across all language partitions
  ([#4](https://github.com/crystaldaking/crystal-chakra/issues/4)).
- `repo_map` now returns ranked monorepository/package overview groups and
  revision-scoped deterministic cursor pagination
  ([#5](https://github.com/crystaldaking/crystal-chakra/issues/5)).
- Repeated caller/test sites are aggregated with occurrence counts and bounded
  representative evidence; every response section and the complete MCP
  envelope have byte-first limits
  ([#13](https://github.com/crystaldaking/crystal-chakra/issues/13)).
- rust-analyzer startup, document synchronization, progress, restart and cache
  readiness remain bounded and revision-honest on large workspaces
  ([#14](https://github.com/crystaldaking/crystal-chakra/issues/14)).
- A reproducible PHP provider comparison keeps v0.1.1 on the measured syntax
  baseline instead of adding an unproven runtime dependency
  ([#2](https://github.com/crystaldaking/crystal-chakra/issues/2)).

### Performance and reliability

- Syntax call sites are stored compactly and resolved lazily under bounded
  candidate fan-out instead of materializing an eager combinatorial call graph
  ([#10](https://github.com/crystaldaking/crystal-chakra/issues/10)).
- Indexing enforces file/source/symbol/edge/call-site/time/RSS budgets and
  publishes explicit useful degradation instead of allocating past the limit
  ([#11](https://github.com/crystaldaking/crystal-chakra/issues/11)).
- Workspace revisions use persistent file-owned graph contributions and
  shallow Rust/PHP composition, preserving immutable readers without copying
  complete graph payloads ([#16](https://github.com/crystaldaking/crystal-chakra/issues/16)).
- Fresh no-op barriers use bounded authoritative metadata reconciliation and
  avoid rereading stable source bodies; one-file edits retain read-your-writes
  semantics ([#17](https://github.com/crystaldaking/crystal-chakra/issues/17)).
- Graph consistency validation is linear in stored facts and redundant
  publication hot-path audits were removed
  ([#18](https://github.com/crystaldaking/crystal-chakra/issues/18)).
- High-level queries stop during examined-item, graph-traversal,
  intermediate-allocation or wall-time limits and report the exact affected
  section ([#19](https://github.com/crystaldaking/crystal-chakra/issues/19)).
- MCP deadlines and caller cancellation now propagate through permit queues,
  freshness, Git, graph traversal and optional provider work while cleanup
  remains owned ([#20](https://github.com/crystaldaking/crystal-chakra/issues/20)).
- Rust/PHP parsing uses deterministic bounded parallel workers selected from
  configured, CPU and memory limits, with single-worker paths for small/live
  updates ([#21](https://github.com/crystaldaking/crystal-chakra/issues/21)).
- Fresh revisions now pin source files and Cargo/Composer classification inputs
  in one shared Git inventory and identity proof; metadata subprocesses obey
  the owning operation's cancellation and deadline.
- Editor-style atomic saves remain targeted across watcher backends: temporary
  non-source paths no longer force a full source-body reread, while the shared
  inventory and identity proof still determines the latest state.
- Precise Rust relationships require a post-provider worktree freshness proof,
  so a concurrently edited disk-backed caller cannot be attributed to an older
  syntax revision; `allow_stale` keeps provider-free syntax latency.

## [0.1.0] - 2026-08-16

### Added

- Git-aware Tree-sitter syntax intelligence for Rust and PHP.
- Atomically published in-memory workspace revisions with deterministic fresh
  read barriers and incremental file/relationship reconciliation.
- Git-aware staged, unstaged, untracked, rename, and delete context.
- Bounded `status`, `repo_map`, `search`, `symbol_search`, `context`,
  `callers`, and `diff_context` MCP tools.
- Optional revision-scoped rust-analyzer call-hierarchy enrichment with honest
  catching-up/degraded fallback.
- Git-object-aware repository identity and separate linked-worktree identity.
- Hermetic regression suites, fixtures, benchmarks/readiness measurements,
  dependency policy checks, and real-provider smoke coverage.

### Operational notes

- Core indexing requires Git but no API key, database, PHP runtime, Composer,
  embedding service, or telemetry service.
- Chakra is released under the MIT License.
- PHP v0.1 is first-class at the syntax/query lifecycle level; PHP dynamic
  dispatch and runtime type resolution remain heuristic and no precise PHP
  provider is bundled.
- All development after this release follows the Gitflow policy in
  `CONTRIBUTING.md` and `AGENTS.md`.

[Unreleased]: https://github.com/crystaldaking/crystal-chakra/compare/v0.1.3...develop
[0.1.3]: https://github.com/crystaldaking/crystal-chakra/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/crystaldaking/crystal-chakra/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/crystaldaking/crystal-chakra/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/crystaldaking/crystal-chakra/releases/tag/v0.1.0
