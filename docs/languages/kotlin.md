# Kotlin language support

Status: in-progress (see `docs/support/languages/kotlin.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0056. Kotlin is a
maintainer-accepted v0.4.0 addition (issue #209); `advertised: true` waits
for the remaining provider and corpus evidence recorded there.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.kt` and `.kts`
  files. A `*.gradle.kts` build script is both a Kotlin source and a Gradle
  project manifest, with Kotlin as the single syntax owner; `pom.xml`,
  `*.gradle`, and `*.gradle.kts` participate in freshness as shared JVM
  metadata inputs and are never indexed as Java sources.
- Project scopes reuse the Gradle/Maven JVM project model (nearest
  Git-visible `settings.gradle(.kts)` / build manifest). `src/test/kotlin`
  paths and Kotlin `*Test(s)` file stems receive explicit test roles.
- Tree-sitter syntax intelligence (`tree-sitter-kotlin-ng 1.1.0`, the
  tree-sitter-grammars grammar bound through `tree-sitter-language`):
  packages, imports and aliases, classes, interfaces, objects, companion
  objects, enum classes and entries, properties and accessors, secondary
  constructors, type aliases, extension-function receivers, annotation
  references, inheritance delegation (`Extends` for constructor invocation,
  `Implements` otherwise), Kotlin/JUnit `@Test` hints, Unicode-aware ranges,
  diagnostics, and bounded member, nullsafe-member, and function call
  candidates.
- All seven Chakra queries and MCP exposure, including atomic revisions,
  `require_fresh`, Git diff context, provenance/precision, budgets,
  truncation, cancellation, and graceful provider degradation.
- Conformance: 14/14 shared scenarios pass
  (`docs/support/conformance/kotlin.json`).

## Install and runtime requirements

Syntax intelligence is fully offline: a JDK, Gradle, Maven, Kotlin
toolchain, and network access are not required, and Chakra never runs
Gradle, Maven, or scripts during discovery or indexing.

Precise enrichment is **not yet integrated** (issue #209). ADR-0056 records
the selection of JetBrains' official `Kotlin/kotlin-lsp`; upstream currently
marks it Alpha, with Android Gradle Plugin support experimental and
Multiplatform support under development. The adapter, pinned test-image
distribution, license verification of the downloaded components, and the
real-server smoke test land with the same issue before Kotlin is
advertised. Until then, Kotlin precise queries report honest syntax
fallback — a healthy, intentional mode, not an error.

## Precision tiers and limitations

- Precise: none yet (provider integration pending; see above).
- Syntax: declarations, containers, imports, ranges, diagnostics,
  annotations, delegation, test hints, and call candidates.
- Heuristic: uniquely resolved local syntax call relations.
- Textual: plain text search hits.

The syntax tier does not type-check, resolve overloads or nullable/safe
calls, infer delegated-property behavior, evaluate generics or coroutine
dispatch, expand generated or synthetic members, or run build scripts. It
indexes every Git-visible `.kt`/`.kts` file regardless of the host's build
configuration. Android and Kotlin Multiplatform source-set behavior is
unverified and remains a documented limitation; mixed Kotlin/Java
repositories index both languages with independent syntax graphs, and
Kotlin precise requests are never routed to jdtls.

Provider absence, crash, timeout, or cancellation leaves the syntax graph
available and reports degradation. `build/` and `target/` build output is
never interpreted as source.

## Verification

- Hermetic parser tests: declarations, extensions, companions, delegation,
  annotations and test hints, call forms, scripts, Unicode, and malformed
  input (`crates/chakra-language-kotlin/src/parser.rs`).
- Conformance fixture and emitted results:
  `fixtures/conformance/kotlin/`, `docs/support/conformance/kotlin.json`.
- Public-corpus evaluation and the real kotlin-lsp smoke test: pending in
  issue #209.
