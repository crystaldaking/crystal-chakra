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
