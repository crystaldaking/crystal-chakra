# Java language support

Status: first-class (see `docs/support/languages/java.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0027; shared LSP
client record: ADR-0032; jdtls lifecycle record: ADR-0036.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.java` files.
- Maven/Gradle project scopes and language-neutral source roles
  (ADR-0019): every `pom.xml` is a Maven module root (`<artifactId>` when
  parseable), every `settings.gradle(.kts)` is a Gradle project root
  (`rootProject.name` when parseable), a `build.gradle(.kts)` without a
  Gradle settings file is a project boundary named after its directory, and
  `src/test/java` / `Test*.java` / `*Test.java` / `*Tests.java` sources
  classify as tests.
- Tree-sitter syntax intelligence (`tree-sitter-java 0.23.5`): extraction
  covers classes, interfaces, enums, records, and annotation types with
  their methods, fields, enum constants, and constructors (recorded as
  methods named `constructor`); package and nested-class containers;
  single-type / static / wildcard import facts; `extends`/`implements`
  relations; JUnit 4/5 `@Test` test hints; byte-accurate ranges;
  actionable syntax diagnostics (ADR-0022); and bounded lazy syntax call
  candidates (ADR-0010).
- Import-aware syntax resolution: `import a.b.C` binds the type name `C`,
  `import static a.b.C.m` binds the member name `m`, and wildcard imports
  contribute package/type prefixes without enumerating members. Bare calls
  resolve through static-import aliases or when the method name is unique;
  `this.` calls qualify against the enclosing class; `Type.member()` calls
  qualify against the (imported) type; `new X()` records a constructor
  call; heritage resolves against nested-class, same-package, single-type
  import, and wildcard candidates in that order.
- A tested jdtls provider adapter (ADR-0027/0036) for definitions,
  references, and callers with revision-scoped synchronization and
  `Provenance::Jdtls` provenance. The shipped CLI registers it as a dormant
  route and activates it only when a precise Java query needs it (#26).
- All seven Chakra queries (`status`, `repo_map`, `search`, `symbol_search`,
  `context`, `callers`, `diff_context`) and their MCP exposure, with atomic
  revisions, `require_fresh`, provenance/precision, ambiguity reporting,
  budgets, truncation, and cancellation.

## Install and runtime requirements

- **Syntax intelligence** (always available): none. The grammar is compiled
  into Chakra and indexing runs fully offline: no JDK, no build tool, and
  no language server is required.
- **Provider adapter** (optional, activated on demand by `chakra serve`): the
  jdtls language server — a `jdtls`
  launcher or `jdt-language-server` binary on `PATH` (or an explicitly
  configured path) — plus a **JDK 21+** runtime. jdtls is a JVM
  application: ADR-0027 records a 1–2 GB heap profile, and the first
  project import on a cold workspace can take minutes. Chakra owns the
  lifecycle (ADR-0036): the per-workspace data directory lives under the OS
  temporary directory (`chakra-jdtls-<workspace hash>`, never inside the
  repository; relative or repository-contained custom `-data` paths are
  rejected), the post-synchronization readiness barrier is bounded by a
  configurable `readiness_timeout` (default 180 s), and every query keeps
  its own bounded wait budget. Its contract tests prove that absence, crash,
  and slow import degrade to syntax intelligence without orphaning a process.
  The CLI exposes `--jdtls-path`, `--no-jdtls`, and the readiness timeout;
  status reports the enabled adapter as `dormant` before its first query.

## Precision tiers

- **Precise** (`jdtls` adapter): definitions, references, and callers
  confirmed by the language server for the pinned workspace revision.
- **Syntax** (`tree_sitter`): declarations, containers, imports, ranges,
  diagnostics, call-site records.
- **Heuristic** (`tree_sitter`): resolved call and `extends`/`implements`
  relations.
- **Textual** (`text_search`): plain text search hits.

Corpus evidence (`docs/support/corpus/results/`) is syntax-tier: providers
are off by default in the corpus runner.

## Measured limitations

From the pinned public corpus evaluation (`docs/support/corpus/RESULTS.md`,
macOS/aarch64, 2026-08-20, release build):

- `spring-projects/spring-boot`: 8,667 Java files, 161,758 retained symbols,
  102,723 edges, 3.49 s cold index, 956 MiB phase-sampled peak RSS, and a
  452 ms warm no-op freshness barrier. One-file and atomic-replace probes
  reparsed exactly one file with zero full reconciliations; all 11 corpus
  scenarios passed. The overall workspace reports bounded degradation because
  three unusually dense secondary JavaScript files exceed their proportional
  graph quota; Java facts and every Java scenario remain complete.
- `apache/kafka`: 6,165 Java files, 216,644 retained symbols, 241,836 edges,
  5.94 s cold index, 1,996 MiB phase-sampled peak RSS, and a 156 ms warm
  no-op freshness barrier. One-file and atomic-replace probes reparsed exactly
  one file with zero full reconciliations; all 11 corpus scenarios passed.

Known false-negative classes — these stay unresolved or ambiguous rather
than being guessed:

- Instance calls through an untyped receiver (`obj.method()`,
  `this.field.method()`): resolved only when the receiver type is nameable
  from syntax (an imported type or the enclosing class); otherwise reported
  as unresolved or ambiguous candidates, never guessed.
- Overload resolution: Java method identity is name plus parameter types;
  the syntax tier keys on the simple name, so overloaded methods are
  ambiguous candidates rather than resolved.
- Reflective and annotation-driven dispatch (dependency injection, getters
  resolved through frameworks, lambda/method-reference targets), dynamic
  proxies, and Lombok/annotation-processed generated members.
- Calls inside field initializers and static/instance initializer blocks:
  they own no callable symbol, so their call sites stay unattributed.
- Members of anonymous classes have no stable syntax-tier container identity;
  their internal calls are omitted rather than attributed to the enclosing
  method. The enclosing method still owns the `new Type()` constructor call.
- Members contributed by `import static a.b.C.*` / `import a.b.*`
  wildcards are not enumerated: the wildcard records the import fact and a
  resolution prefix, but a member nameable only through it resolves only
  when the name is otherwise unique.

## Evidence

- Conformance: `docs/support/conformance/java.json` (14/14 scenarios),
  including a static-import member-alias hard case.
- Corpus: `docs/support/corpus/results/java-spring-projects__spring-boot.json`
  and `docs/support/corpus/results/java-apache__kafka.json`.
- Adapter tests: `crates/chakra-language-java/tests/fixture_index.rs`
  (declarations, containers, imports and aliases, ranges, test hints,
  diagnostics, call candidates, ambiguity, reconcile) and
  `crates/chakra-language-java/src/indexer.rs` unit tests (parallel
  determinism, cancellation, bounded lazy call fan-out).
- Provider contract tests: `crates/chakra-provider-jdtls/tests/lifecycle.rs`
  (fake-server lifecycle, delta sync, cancellation, crash restart,
  orphan-free shutdown). Real-server smoke was not run: no JDK is
  installed on the authoring machine; jdtls discovery and degradation are
  covered by the missing-executable test.
- Discovery/classification: `crates/chakra-git/src/discovery.rs` and
  `crates/chakra-git/src/source_metadata.rs` tests.
