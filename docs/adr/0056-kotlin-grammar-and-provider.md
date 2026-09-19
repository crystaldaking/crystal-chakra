# ADR-0056: Kotlin grammar and provider selection (issue #209)

Status: accepted
Date: 2026-09-13

## Context

Issue #209 (milestone v0.4.0) adds first-class Kotlin under the language
parity contract (ADR-0026): offline syntax intelligence plus evidence-backed
precise enrichment with honest fallback. The grammar must match Chakra's
pinned Tree-sitter core (0.26) and be reproducibly pinnable; the provider
decision must prefer JetBrains' official standalone language server with
recorded capability evidence.

## Decision

### Grammar: `tree-sitter-kotlin-ng` 1.1.0 (tree-sitter-grammars)

- Selected crate: `tree-sitter-kotlin-ng = "1.1.0"`, the
  `tree-sitter-grammars/tree-sitter-kotlin` grammar named as the candidate
  in #209. MIT license. It binds through `tree-sitter-language`, the same
  ABI-agnostic mechanism already used by `tree-sitter-hcl 1.1.0` in this
  workspace, so it compiles against the pinned 0.26 core without an ABI
  exception. Version pinned in `Cargo.lock`.
- Covers `.kt` and `.kts`; parser error recovery is handled by the shared
  bounded diagnostics path like every other grammar.
- **Rejected: `tree-sitter-kotlin` 0.3.8 (fwcd).** Its `tree-sitter`
  dependency is `>= 0.21, < 0.23`, ABI-incompatible with the pinned 0.26
  core. Upgrading or dual-coring Tree-sitter for one language is not
  justified.

### Provider: JetBrains `Kotlin/kotlin-lsp`, evaluated with recorded gaps

- The official standalone server is the first choice per #209. Upstream
  documentation currently marks it Alpha, with Android Gradle Plugin
  support experimental and Multiplatform under development; those states
  are re-checked at implementation time and recorded in
  `docs/languages/kotlin.md`.
- The adapter follows the existing bounded worker pattern (shared transport
  where applicable, revision-bound readiness, cancellation, restart/backoff,
  no-orphan shutdown, worktree isolation). Kotlin precise requests are never
  routed to jdtls merely because both are JVM tooling.
- Syntax-only operation remains a healthy, intentional mode; precise
  support is advertised only for project shapes the pinned provider proves
  in the validation environment. Distribution license terms of the actual
  downloaded components are verified before the server is pinned in the
  test image.

### Source and metadata ownership

- `.kt` and `.kts` are Kotlin sources; `build.gradle.kts` and
  `settings.gradle.kts` are Kotlin syntax sources *and* project metadata.
  There is exactly one syntax owner (Kotlin) — Java adapters never index
  `.kts`. Gradle/Maven metadata files are provider inputs for both the Java
  and the Kotlin routes (`metadata_languages` returns both), reusing the
  existing JVM project-model evidence instead of a parallel model.
- Offline discovery never runs Gradle, Maven, or scripts. Kotlin test
  sources follow the conventional `src/test/kotlin` layout alongside the
  existing role classifications.
- Nested declarations carry the complete enclosing container chain for
  qualified names and parent lookup (`Alpha::Inner::work` differs from
  `Beta::Inner::work`). The corrected parser uses snapshot codec `kotlin:s2`;
  `kotlin:s1` snapshots are invalidated through the existing codec gate so
  cached shortened identities cannot survive an upgrade.

### Configuration and diagnostics

- Kotlin registers one provider key `kotlin-lsp` in the shared
  configuration surface (ADR-0053) and one row in the doctor provider table
  (issue #207) with the same readiness rules as every other language.
  Multiplatform source sets and Android shapes are documented limitations
  unless the pinned provider proves them.

## Alternatives considered

- **fwcd `tree-sitter-kotlin` 0.3.8.** Rejected on ABI (above); it is also
  the older grammar line relative to the tree-sitter-grammars successor.
- **kotlin-language-server (fwcd, community).** Kept as the recorded
  alternative if the official server's Alpha gaps block parity evidence;
  the official server is evaluated first per the issue.
- **A Kotlin-specific project model.** Rejected: Kotlin/JVM builds are
  Gradle/Maven; the existing JVM metadata evidence is reused and `.kts`
  build scripts are indexed as Kotlin sources.

## Consequences

- Workspace gains `tree-sitter-kotlin-ng` (MIT) as the only new grammar
  dependency; `Language::Kotlin` becomes the twelfth advertised language
  once every mandatory parity capability passes.
- Snapshot versions, conformance fixtures, support matrices, and the
  provider test image all gain Kotlin entries; `advertised: true` waits for
  the parity evidence recorded in #209.

## Validation / follow-up

- Hermetic parser tests (declarations, extensions, companions, scripts,
  Unicode, malformed input), deterministic emitted conformance results, a
  pinned real-server smoke test in the provider image, and public-corpus
  evaluation records in `docs/languages/kotlin.md`.

## Addendum 2026-09-13: verified kotlin-lsp distribution and pin

Distribution facts re-checked against the official `Kotlin/kotlin-lsp`
RELEASES.md and the repository LICENSE:

- The standalone server is Apache-2.0 (the VS Code *extension* moved to the
  JetBrains Free Plugin License; the standalone archive remains Apache-2.0).
  Weekly pre-alpha builds are published per platform with SHA-256
  checksums.
- Pinned for the provider test image: `kotlin-server 262.9593.0`,
  linux-x64 `kotlin-server-262.9593.0.tar.gz`, SHA-256
  `2d99d8e198fbe4aa8f4481e37799724ce94803b4ea12a60b416040e3fcd7cc5e`
  (fetched from download-cdn.jetbrains.com on 2026-09-13).
- The distribution bundles its own runtime (no host JDK required);
  `bin/intellij-server` is the documented launcher (the legacy
  `kotlin-lsp.sh` is deprecated). Call hierarchy
  (`prepareCallHierarchy` + incoming/outgoing) is supported since
  v262.4739.0 and matches Chakra's call-hierarchy trio exactly.
- Alpha caveats recorded for the release notes: Android Gradle Plugin
  support is experimental, Multiplatform is under development, and weekly
  pre-alpha builds move fast, so the pinned version — not "latest" — is the
  only supported evaluation target.

- The Chakra adapter (`chakra-provider-kotlin-lsp`) follows the shared
  worker pattern with the `kotlin` language id, `Provenance::KotlinLsp`,
  one `kotlin-lsp` provider key in configuration (ADR-0053), and discovery
  of a `kotlin-lsp` executable wrapper on PATH. Kotlin precise requests are
  never routed to jdtls.

## Addendum 2026-09-16: executable launch contract

The pinned Linux binary's `--help` confirms `intellij-server --stdio`.
The adapter supplies that option; the PATH wrapper forwards it unchanged.
The earlier positional `stdio` invocation was incorrect. Hermetic lifecycle
peers now enforce the actual argument contract, and the real-provider smoke
uses a Kotlin/Maven fixture with `CHAKRA_KOTLIN_LSP` as its override.

The old `262.9593.0` binary now exits with an expired-build message. The
evaluation image therefore pins official `kotlin-server 263.4702.0`, SHA-256
`1e11d2e5fefbf9ea215ad8dd6be95f2222897cd086e8cb7a661a52084a590405`,
from the upstream release list and checksum. It bundles a Java 25 runtime.
The test image also supplies SHA-512-verified Maven 3.9.16, because this
server invokes external `mvn` when importing the Maven smoke project.
No Rust dependency is added. Offline discovery/indexing still never invokes
build tools.

The repository's Apache-2.0 LICENSE is not sufficient evidence for the
earlier blanket claim about the complete standalone distribution. The
archive includes bundled component licenses; that repository license
must not be presented as the license for every bundled component.

At the initial recheck, real-server validation **failed**: after initialization,
Chakra reported
`Ready` with empty incoming calls while project import is still underway.
A diagnostic raw-LSP probe after successful import obtained a hierarchy item
but timed out on incoming calls within 30 seconds. The following addendum
records the longer probe, identified readiness cause, and correction. The
original evidence remains in [the initial recheck](../evaluation/v0.4.0-language-recheck.md);
[current acceptance results](../evaluation/v0.4.0-kotlin-acceptance.md) supersede it.

## Addendum 2026-09-16: required Kotlin release scope and readiness fix

The maintainer explicitly requires Kotlin support for v0.4.0, including
Android and Multiplatform as well as Gradle/Maven Kotlin/JVM. These are
release acceptance criteria; passing only the Maven fixture does not satisfy
them. The real-provider test target now contains separate project fixtures
for Maven, Gradle JVM, Android SDK overloads, and Multiplatform common/target
callers with `expect`/`actual` declarations. Kotlin
remains unadvertised until the required project evidence passes.

The earlier incoming-call timeout was investigated with a longer raw-LSP
probe. The pinned server eventually returned the correct caller after its
index finished (about 87 seconds from cold startup under amd64 emulation).
An initialization response or null hierarchy preparation is therefore not
a Kotlin readiness proof. The Kotlin adapter now waits for a hierarchy item
and its requested relations under a separate 180-second readiness budget.
Polling empty preparation responses pumps the owned session, checks caller
cancellation and shutdown, and uses a bounded 250-ms interval. After a
document delta, the previous synchronization proof is invalidated.

A follow-up Android raw-LSP probe found that even nonempty hierarchy answers
can be approximate during indexing: `target(Context)` incorrectly included
the caller of `target(Int)`. Definition and reference requests were ambiguous
as well. After both import/indexing progress tokens ended, all three requests
resolved the overload correctly. Kotlin therefore additionally requires an
observed idle work-done epoch before querying, and discards answers if any
work begins during the query, even if it also ends before the response.
The shared worker tracks at most 64 active tokens of at most 256 bytes;
overflow cannot establish readiness. State resets on session restart.
Only the Kotlin adapter opts into this readiness policy. Providers without
work-done notifications keep their existing policies. Missing completion
signals lead to bounded fallback, never a guessed precise result.

As with jdtls, the caller's short wait can return `catching_up` while the
owner completes import; an explicit caller cancellation or execution
deadline still cancels that work. Warm queries keep the shorter request
budget. A prepared hierarchy item with zero relations is a valid empty
answer; a null preparation response cannot falsely publish `Ready`.
The generic worker exposes synchronization-barrier and idle-work epoch
observations plus a cancellable event wait to the Kotlin hook. Other providers keep their
existing query policy, and no LSP types enter the domain/query layers.

Offline Kotlin source classification also loads the shared JVM metadata
model (it previously loaded only for Java despite the Kotlin routing arm).
Android variant and KMP test source sets are recognized by bounded path
conventions without executing build scripts. `expect`/`actual`, Android
annotations, and native platform imports have parser regression coverage;
this syntax evidence does not establish precise platform resolution.

## Addendum 2026-09-16: `.kts` module identity correction

The module-path helper stripped `.kt` before `.kts`, so every script file
(`build.gradle.kts`, `settings.gradle.kts`) fell through to the `"source"`
stem and collided on one module qualified name per package. The suffix
order is corrected (`.kts` first), scripts keep their file stem
(`root::build.gradle`), and a regression test pins distinct module
identities for sibling scripts. Because `.kts` module identities change,
the snapshot codec advances from `kotlin:s2` to `kotlin:s3`; the codec
gate invalidates both earlier codecs. Committed Kotlin conformance
artifacts are unaffected (re-emitted and diffed clean).


## Addendum 2026-09-18: KMP import and call-query boundary

Inspection of the pinned server established two independent KMP defects.
Its Gradle importer reads Java source sets instead of the Kotlin target graph.
Its Kotlin call-hierarchy renderer requires a Java light method when rendering
function parameters; JS and Native functions can therefore disappear even
when definitions, references, diagnostics, and expect/actual resolution work.
Increasing the readiness timeout does not repair either defect.

The Kotlin adapter is developing a definition-confirmed query strategy for
KMP. The existing Kotlin syntax crate supplies bounded call-expression and
containing-function candidates from immutable provider documents. LSP
references discover incoming candidates and a unique LSP definition must
confirm every published binding. Callable values, ambiguous definitions,
and unsupported syntax cannot become guessed precise edges. Incomplete
queries report truncation or fallback. Queries retain the worker's revision,
synchronization barrier, idle-work epoch, deadline, and cancellation rules.
The adapter now depends on the existing `chakra-language-kotlin` crate;
no new third-party library is needed for this strategy.

A prepared target-aware JSON model must live outside the project's Gradle
root: otherwise the pinned server selects Gradle again and replaces the
model. A default-None `ProviderHooks::initialization_root` overrides only
LSP initialization and workspace-folder import. Process cwd, synchronized
source documents, URI validation, and published locations continue to use
the canonical worktree. Other providers keep their existing behavior.
A hermetic lifecycle test verifies both roots through the actual worker.

At this prototype stage, automatic preparation, validation against build-input
changes, and the default real-provider release gate were still missing. The
next addendum records their implementation. A manually prepared model alone
does not close the release gate.


## Addendum 2026-09-18: owned Gradle preparation

The prepared-model prototype is replaced by automatic provider-owned
preparation. The fixed embedded Gradle init script obtains KMP dependencies
from KGP's IDE import API and serializes the pinned server's JSON model into
an owned temporary directory. The Kotlin adapter reuses workspace-managed
`tempfile` and Unix `nix`; it adds no new dependency version. The worker calls
a bounded preparation hook with its shutdown/cancellation check before
spawning the LSP session. Offline indexing and MCP gain no execution API.

A language hook can request a session restart for a published input delta
before document notifications or queries are sent. Kotlin opts in for build
metadata and Gradle/buildSrc source changes; normal source changes retain
delta synchronization. `ProviderInput::matches_metadata` and
`ProviderWorkspace::inputs_for` expose only typed, revision-bound input facts.
The Kotlin adapter checks these observations around preparation and queries.
It rejects a stale model with `CatchingUp` and rejects queries outside the
imported source roots. No precise graph becomes a commit-snapshot fact.

The shared worker also forwards notifications through a nonblocking observer
hook. Kotlin retains only a bounded import-status enum and requires successful
`intellij/workspaceImportState` completion in addition to the idle work epoch.
An import failure must not publish a precise empty result merely because
indexing progress ended. Other providers retain their existing policies.

The default real-provider matrix now passes Maven, Gradle JVM, Android, and
KMP, with an additional Gradle source-root edit/regeneration assertion. The
standard rebuilt-image run also passes without a prepared Native cache.
Real Gradle cancellation probes verify termination both for interruptible
tasks and for task code that catches thread interruption. They do not prove
ownership of arbitrary plugin-spawned independent processes.

Source-root containment uses a canonical existing ancestor, retaining missing
generated directories while rejecting external or dangling symlink paths.
This normalizes Java/Rust Windows path forms; native Windows verification
is still pending.

Compiler language/API versions, effective progressive mode, and opt-ins are
carried into source-set facets. An unset API version follows the configured
language version. This is covered by a real Gradle import-contract regression.
KGP supports [compiler options at extension, target and task levels](https://kotlinlang.org/docs/gradle-compiler-options.html#how-to-define-options);
the adapter reads resolved compilation options instead of assuming that
source-set settings alone capture them. Conflicting effective settings cannot
be silently represented as one common facet. Broader compiler-argument and
mixed/composite graph coverage remain review items before the final release
candidate is accepted.


## Addendum 2026-09-18: Java companion synchronization

The pinned server imports Java and returns target-bound Java references to
Kotlin declarations, but omits Java edges from Kotlin call hierarchy and
does not implement Java definition or hierarchy requests. Raw probes also
verify overload discrimination and buffer-only Java edits. Mixed JVM queries
therefore use the semantic strategy: Kotlin references are independently
checked with unique definitions; Java references establish binding to the
verified Kotlin target, while bounded Java syntax identifies the invocation
and owner. Method values do not become calls. Kotlin outgoing definitions
can resolve to Java declarations.

`ProviderHooks::supports_query_language` separates query ownership from
companion synchronization, defaulting to the existing synchronization policy.
Kotlin synchronizes Kotlin and Java documents but accepts only Kotlin queries.
Unsupported queries are rejected before entering the worker queue. The engine
exposes a typed language-presence query over the captured workspace without
introducing LSP types or a second source of truth. Source revision publication
and delta synchronization remain unchanged.

The Java and Kotlin syntax adapters share data-only call-range DTOs in
`chakra-language-index`; grammar-specific bounded parsing stays in each
adapter. Kotlin provider dependencies on the existing Java and language-index
workspace crates add no third-party package or version. The first real
Kotlin/Java regression passed in 90.20 seconds, including Java-only edit
freshness and Kotlin-to-Java outgoing calls. The expanded overload/method-value regression also passed in 103.51 seconds.
Final repository validation remains a separate gate.

## Addendum 2026-09-18: free compiler arguments

The KMP model now carries `freeCompilerArgs` through the build's Kotlin CLI
argument parser into typed compiler-argument JSON. A real Gradle regression
first demonstrated loss of `-Xcontext-parameters` and
`-Xannotation-target-all`; their common/JVM/JS facet fields now survive.
Using the server's `additionalArguments` string would corrupt some quoted
backslash paths, so the adapter does not construct a shell-like argument
string or implement another CLI parser.

Each contributing compilation is parsed using its own compiler argument
class. A common facet retains properties supported by the metadata compiler;
backend-only properties such as KGP's injected JS output-module name remain
on their platform facets. Conflicting effective common settings, unknown
flags, unsupported argument value types, positional source inputs, `@argfile`
inputs and internal `-XXLanguage` argument objects fail the owned import instead of producing
misleading precise results. Tokens are bounded to 1,024 and 64 KiB per
compilation; existing model-size, preparation deadline and cancellation
bounds remain in force. Argument files are rejected before Kotlin expands
them because their contents are not yet tracked as revision-bound import
inputs and would bypass those argument bounds. Real Gradle negative cases cover an unknown flag and
conflicting common settings.

The implementation uses the pinned KGP compiler classes and JDK bean
introspection; no new dependency or MCP execution surface is introduced.
The compiler-plugin classpath gap is addressed by the addendum below.
Remaining typed options and mixed/composite build support still require
review. This addendum does not claim full compiler-plugin or
experimental-feature parity.

## Addendum 2026-09-18: KMP project artifact dependencies

The KMP exporter now resolves `IdeaKotlinProjectArtifactDependency` inside the
exported Gradle build. The original two-module JVM/JS fixture failed because
KGP represents platform dependencies as compilation-output artifacts, whereas
the adapter only handled source-set and external binary dependencies.

The adapter indexes KGP compilation output directories and each platform
target's declared archive task, using Gradle's `archiveFile` provider for its
main compilation. It does not infer jar names, target names or source sets
from filesystem naming patterns. An artifact must map to one producing
source-set collection for its owning project; KGP's `resolved` operation then
creates source dependencies with the original dependency kind. The producer
index is bounded to 4,096 artifact paths. Missing or ambiguous producers fail
import, and all resolved source modules must exist in the exported graph.

KGP's `buildPath` is checked as well as `projectPath`: `rootProject.allprojects`
only exports the top-level build. Included builds can have the same `:` project
path and different build identities; those dependencies are rejected before
model publication instead of being aliased to the host's modules. This is a
correctness boundary, not implementation of composite-build precise support.
A real Gradle regression covers both a valid two-module graph and rejection
of a subsequent composite import without retaining the previous model.

This expands precise model preparation within the existing language adapter.
No dependency, domain/query contract, offline build execution or Git snapshot
semantics change is introduced. The richer real-LSP fixture additionally
checks incoming/outgoing calls across KMP modules, alongside JVM/JS/Native,
overload exclusion, source edits and Gradle/source-root regeneration.

## Addendum 2026-09-18: KMP compiler-plugin configuration

The exporter previously omitted compiler plugins configured through Gradle
plugin APIs, even though import reported success. KGP's source-set language
settings now supply compiler-plugin classpaths and option tokens to the
existing typed facet fields. This API covers metadata and Native tasks as
well as JVM/JS; reading JVM's task-level `pluginOptions` on a Native task is
not valid. The source-set classpath is retained instead of combining platform
compiler artifacts into an invented common configuration.

All contributing compilations must agree on plugin options, and their plugin
artifacts must be represented in the chosen source-set classpath. Conflicts
fail import before publication. Free compiler arguments are merged with these
settings without shell quoting; plugin paths become canonical existing files.
Each facet is bounded to 256 paths, 1,024 options and 64 KiB of combined strings,
in addition to the existing whole-model and owned-process bounds.

This remains live adapter enrichment. No dependency, public API, MCP command,
offline indexing or snapshot semantics change is introduced. The regression
checks Serialization and All-open artifact/option transfer and invalidation
after a platform-only classpath change. Real-server validation is recorded
separately in the Kotlin acceptance report; transferring a plugin's settings
does not prove arbitrary generated-declaration or compiler-plugin compatibility.

## Addendum 2026-09-18: Gradle Kotlin DSL query boundary

Protocol probes with the pinned server and Gradle 8.14.3 successfully imported
a JVM project and resolved a control `.kt` call, but returned null hierarchy
preparation and empty references/definitions for functions in
`build.gradle.kts`. A preliminary probe with automatically selected Gradle
9.7 showed the same behavior; the pinned probe is the release evidence.

The Kotlin adapter now excludes `*.gradle.kts` from precise path support and
LSP document synchronization using the existing worker path hook. Its public
provider wrapper forwards that path-support decision to the engine. Direct
queries are rejected before entering the worker queue; a rejected script
query does not degrade a healthy `.kt` session. Gradle scripts remain Kotlin
syntax sources and tracked project metadata. This does not remove their
build-input invalidation or execute them during offline indexing.

No shared-worker API, domain contract, dependency or publication semantics
change is introduced. Standalone `.kts` has separate real fallback evidence;
this path filter is specific to Gradle Kotlin DSL, not a blanket exclusion
of Kotlin script syntax. Validation and the exact supported precision are
recorded in the Kotlin acceptance report.

## Addendum 2026-09-19: typed KGP compiler options

The live KMP exporter uses the project's KGP compiler-options helper to
populate typed compiler arguments before applying source-set settings and
parsing free argument tokens. A manual subset omitted options such as
`allWarningsAsErrors` and JVM `javaParameters`. KGP stages free argument
tokens in `freeArgs`; the exporter verifies that list matches the bounded
input, clears the staging list, then parses those tokens with Kotlin's
platform argument parser. Positional leftovers and invalid flags remain
errors. Shared source sets still require compatible projected arguments
across their contributing compilations.

This conversion depends on the evaluated KGP helper API (2.2.20); an absent
or incompatible helper fails project import instead of silently using
default compiler settings. No dependency, offline execution, domain API or
workspace publication change is introduced.

## Addendum 2026-09-19: separate document and metadata selection

The Gradle DSL path exclusion exposed a shared delta-filtering bug: the
document predicate also filtered provider metadata. Consequently a changed
`build.gradle.kts` could leave the KMP model stale. Workspace deltas now
offer separate document and input predicates; the existing method retains
its previous behavior for callers. The worker uses path-aware document
selection and language-scoped metadata selection. A build file can therefore
invalidate a session even when its source dialect is not sent as a document.

The additional domain API contains only language/path predicates, without
LSP or MCP types. It preserves revision-bound delta calculation and atomic
publication; it does not trigger repository syntax reindexing. The real KMP
source-root-change test is the end-to-end regression for this boundary.
