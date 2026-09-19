# Kotlin language support

Status: in-progress (see `docs/support/languages/kotlin.json` and
`docs/language-parity-contract.md`). Selection record: ADR-0056. Kotlin is a
maintainer-accepted v0.4.0 addition (issue #209). Real-provider acceptance of
Maven, Gradle JVM, Android, and Kotlin Multiplatform now passes in the pinned
Docker image; `advertised: true` still waits for final candidate review and
validation. These are required v0.4.0 project shapes. The corpus already
has a recorded baseline; it does not substitute for a real kotlin-lsp run.

## What is supported

- Git-aware discovery of tracked and untracked non-ignored `.kt` and `.kts`
  files. A `*.gradle.kts` build script is both a Kotlin source and a Gradle
  project manifest, with Kotlin as the single syntax owner; `pom.xml`,
  `*.gradle`, and `*.gradle.kts` participate in freshness as shared JVM
  metadata inputs and are never indexed as Java sources.
- Project scopes reuse the Gradle/Maven JVM project model (nearest
  Git-visible `settings.gradle(.kts)` / build manifest). `src/test/kotlin`
  paths and Kotlin `*Test(s)` file stems receive explicit test roles, as do
  conventional Android variant and KMP source sets such as `testDebug`,
  `androidInstrumentedTest`, `commonTest`, and `iosSimulatorArm64Test`.
  Nearest-module metadata is verified for both Gradle and Maven.
- Tree-sitter syntax intelligence (`tree-sitter-kotlin-ng 1.1.0`, the
  tree-sitter-grammars grammar bound through `tree-sitter-language`):
  packages, imports and aliases, classes, interfaces, objects, companion
  objects, enum classes and entries, properties, secondary
  constructors, type aliases, extension-function receivers, annotation
  references, inheritance delegation (`Extends` for constructor invocation,
  `Implements` otherwise), Kotlin/JUnit `@Test` hints, Unicode-aware ranges,
  diagnostics, and bounded member, nullsafe-member, and function call
  candidates.
- Nested members retain the full enclosing container chain, such as
  `review::Alpha::Inner::work` and `review::Beta::Inner::work`. `.kts`
  scripts keep their file stem in the module path (`build.gradle.kts`
  becomes `root::build.gradle`, not a shared `root::source`). Snapshot
  codec `kotlin:s3` rejects the earlier shortened identities. Accessors do
  not get separate declaration identities; their calls belong to the property.
- All seven Chakra queries and MCP exposure, including atomic revisions,
  `require_fresh`, Git diff context, provenance/precision, budgets,
  truncation, cancellation, and graceful provider degradation.
- Conformance: 14/14 shared scenarios pass
  (`docs/support/conformance/kotlin.json`).

## Install and runtime requirements

Syntax intelligence is fully offline: a JDK, Gradle, Maven, Kotlin
toolchain, and network access are not required, and Chakra never runs
Gradle, Maven, or scripts during discovery or indexing.

Precise enrichment uses JetBrains' official standalone `kotlin-server`
distribution (Alpha), selected in ADR-0056. The pinned evaluation target is
`kotlin-server 263.4702.0` (SHA-256-verified in `tools/Dockerfile.lsp`);
the distribution bundles its own runtime and launches through
`bin/intellij-server --stdio`. Chakra appends the `--stdio` option itself.
For Maven projects, the server also needs `mvn` on `PATH`; the Docker
evaluation image supplies Maven 3.9.16. The Gradle fixtures pin Gradle
8.14.3 and Kotlin plugin 2.2.20; Android additionally uses AGP 8.11.1,
SDK platform 35 and build tools 35.0.0 under `ANDROID_HOME`. Precise project
import can run build tooling and download dependencies. This requirement does not apply to
Chakra's offline syntax indexing.
For Gradle, Chakra uses the project's executable wrapper, or `gradle` on
`PATH`, to inspect the project before starting the server. KMP projects use
an automatically generated source-set model in an owned temporary directory
outside the worktree. This preserves canonical source paths and resolves
libraries through Kotlin Gradle Plugin's IDE API. Normal source edits sync
incrementally; build-input edits regenerate the model and restart the precise
provider. The Docker image includes standalone Gradle 8.14.3.
Set the launcher path in the private `chakra.local.toml`:

```toml
schema_version = 1

[providers.kotlin-lsp]
path = "/absolute/path/to/kotlin-server/bin/intellij-server"
```

The CLI equivalent is `chakra serve --kotlin-ls-path /absolute/path/to/kotlin-server/bin/intellij-server`.
Alternatively, put a `kotlin-lsp` wrapper on `PATH` that forwards arguments
unchanged, for example `exec /absolute/path/to/kotlin-server/bin/intellij-server "$@"`.
Do not add another transport argument in that wrapper. Use `--no-kotlin-lsp` or
`providers.kotlin-lsp.enabled = false` for deterministic syntax-only
operation. The upstream caveats recorded for the selected provider are:
Android Gradle Plugin support is experimental
and Multiplatform is under development, and weekly pre-alpha builds move
fast — only the pinned version is a supported evaluation target. An absent
or failing server degrades honestly to syntax.

## Precision tiers and limitations

Standalone `.kts` files without a build manifest have syntax support only.
The pinned server reports `BLOCKED / noBuildSystemFound`; the real-adapter
regression verifies explicit degradation and no script execution. Empty
server responses in this case are not treated as complete precise results.
See the [acceptance evidence](../evaluation/v0.4.0-kotlin-acceptance.md).

Gradle Kotlin DSL (`*.gradle.kts`) also has syntax support only. Even after
successful project import, the pinned server does not resolve the tested
build-script functions. Chakra excludes these paths from precise queries
and document synchronization while retaining them as syntax sources and
project metadata, so build changes still invalidate the imported model.

- Precise: Kotlin-only Maven, Gradle JVM and Android projects use
  incoming/outgoing call hierarchy for the synchronized workspace revision.
  KMP and workspaces containing Java use semantic references and unique
  definitions to verify explicit call candidates from captured source.
  Java callers are verified by target-bound semantic references; the pinned
  server has no Java definition handler. This avoids its incomplete
  JS/Native and Kotlin/Java hierarchy results.
  Five real-provider scenarios have recorded passing evidence, including edits; KMP additionally
  verifies JVM/JS/Linux Native callers and source-root changes in Gradle. See
  [current acceptance evidence](../evaluation/v0.4.0-kotlin-acceptance.md).
- Syntax: declarations, containers, imports, ranges, diagnostics,
  annotations, delegation, test hints, and call candidates.
- Heuristic: uniquely resolved local syntax call relations.
- Textual: plain text search hits.

The syntax tier does not type-check, resolve overloads or nullable/safe
calls, infer delegated-property behavior, evaluate generics or coroutine
dispatch, expand generated or synthetic members, or run build scripts. It
indexes every Git-visible `.kt`/`.kts` file regardless of the host's build
configuration. Android/KMP source-set roles and `expect`/`actual` syntax
have regression coverage; this does not prove platform-specific semantic
resolution. Mixed Kotlin/Java repositories index both languages with independent
syntax graphs. Kotlin LSP synchronizes Java companion documents so Java-only
edits participate in the revision barrier. Java precise queries remain with
the Java provider, and Kotlin precise requests are never routed to jdtls.
The real mixed-JVM regression verifies both call directions and Java-only
edit freshness; method values are excluded from call relations.

KMP model coverage is currently incomplete for mixed builds containing
ordinary Kotlin/JVM or Kotlin/Android modules alongside KMP modules. Queries
inside imported modules retain verified precise relations but explicitly
mark results as truncated; queries outside imported roots retain fallback.
This must not be interpreted as complete cross-module analysis. KMP modules
within one Gradle build now resolve project artifacts through KGP's compilation
outputs and declared archive tasks; absent or ambiguous producers fail import.
Dependencies on included composite builds are rejected because their modules
are not exported, even if a project path matches one in the main build.

Validated `freeCompilerArgs` are parsed by the build's Kotlin compiler into
typed model fields. Unknown/conflicting flags, unsupported value types and
untracked `@argfile` inputs fail import. KMP compiler-plugin paths and options
are transferred from KGP's source-set configuration; conflicting platform
settings fail import. Typed compiler options are transferred using the evaluated
KGP 2.2.20 helper API. Arbitrary plugin-generated declarations and native Windows
path behavior remain unverified. Unsupported implicit call forms
also report incomplete coverage rather than a complete empty call list.
Calls inside lambdas and anonymous functions are not attributed to the
enclosing named function: their execution can be deferred. Until separate
callable ownership is represented, the semantic route reports incomplete
coverage for these bodies, including inline-lambda cases it cannot prove.

A null hierarchy preparation while the project is loading returns
`catching_up`, never an empty precise answer. Kotlin has a separate
180-second import/indexing readiness budget; the default one-second caller
wait can return while the owned worker continues. A later query can observe
completed indexing. Explicit operation cancellation/deadlines and shutdown
still stop the work. Document changes invalidate the earlier readiness proof.
The hook requires successful workspace import, waits for all observed
import/indexing progress to end and
discards answers spanning a new work-done epoch. The pinned server can
return approximate overload matches even from nonempty hierarchy requests
while indexing; such intermediate answers are never published as precise.
Progress tracking is bounded, and absent completion signals time out safely.
A prepared hierarchy item with no relations is a valid empty result after
the readiness checks; the KMP strategy additionally verifies the target's
unique definition before accepting a result.

Provider absence, crash, timeout, or cancellation leaves the syntax graph
available and reports degradation. `target/` directories are excluded from
discovery. Other build output follows Git visibility: Git-visible `.kt`/`.kts`
files under `build/` can be indexed; generated output should be Git-ignored.

## Verification

- Hermetic parser tests: declarations, extensions, companions, delegation,
  annotations and test hints, call forms, scripts, Unicode, malformed
  input, `expect`/`actual`, Android annotations, and native imports (`crates/chakra-language-kotlin/src/parser.rs`).
- Conformance fixture and emitted results:
  `fixtures/conformance/kotlin/`, `docs/support/conformance/kotlin.json`.
- Gradle/Maven module and Android/KMP source-role regressions:
  `crates/chakra-git/src/source_metadata.rs`.
- Hermetic lifecycle regressions with a scripted stdio peer, including
  delayed import, no premature `Ready`, cancellation during polling,
  bounded timeout, readiness after edits, legitimate empty answers, and
  rejection of intermediate answers during server indexing:
  `crates/chakra-provider-kotlin-lsp/tests/lifecycle.rs`.
- Five positive real kotlin-lsp scenarios in the pinned provider image:
  `crates/chakra-provider-kotlin-lsp/tests/real_provider.rs` (ignored by
  default; selected by `tools/run_lsp_tests.sh`). Maven, Gradle JVM, mixed
  Kotlin/Java, Android overloads involving an SDK type, and KMP common/target calls must pass
  before advertising the required scope. Each checks incoming calls,
  a second revision after an edit, outgoing calls, and provenance.
- Public-corpus evaluation: pinned `Kotlin/kotlinx.coroutines` and
  `square/okhttp` (mixed Kotlin/Java), 12/12 scenarios each
  (`docs/support/corpus/results/kotlin-*.json`, `docs/support/corpus/RESULTS.md`).
  These are the recorded 2026-09-13 measurements. The provider-lifecycle
  scenario uses a hermetic double, not the JetBrains server.

The 2026-09-16 provider recheck corrected the launcher contract: the pinned
binary's `--help` requires `--stdio`, not positional `stdio`. The Docker
wrapper forwards arguments unchanged, and image installation failures are
no longer masked. The smoke fixtures honor `CHAKRA_KOTLIN_LSP`; the
real-provider gate now covers Maven, Gradle JVM, Android and Multiplatform.

The earlier `262.9593.0` pin exits with an expired-build message. Its
replacement is `263.4702.0`. The first recheck revealed that Chakra declared
readiness before import finished; a longer direct probe established that
indexing eventually returned the correct caller. The readiness fix and
subsequent JVM and Android successes supersede that original failure. Kotlin remains
`in-progress` and `advertised: false` pending final candidate review and
validation, despite the recorded passing project scenarios. See the [Kotlin acceptance follow-up](../evaluation/v0.4.0-kotlin-acceptance.md)
for commands, results, and the limits of this evidence.
