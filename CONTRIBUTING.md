# Contributing to Chakra

Thank you for improving Chakra. The project values small typed interfaces,
honest precision/provenance claims, deterministic tests, and readable Git
history.

## Development setup

Install the pinned Rust toolchain through rustup, then run:

```sh
cargo test --locked --workspace
```

Git is required by discovery, identity, diff, and live-worktree tests. A PHP
runtime is not required for PHP syntax indexing. rust-analyzer is optional;
the default suite uses hermetic provider peers.

## Gitflow (mandatory after v0.1.0)

`v0.1.0` is the repository's Gitflow boundary. After that tag:

1. Start ordinary work from current `develop`.
2. Create `feature/<topic>`, `fix/<topic>`, `refactor/<topic>`, or
   `docs/<topic>`.
3. Keep commits cohesive and use Conventional Commit-style subjects.
4. Open a pull request into `develop`; never push a development commit
   directly to `main` or `develop`.
5. Use `release/<version>` to stabilize a release from `develop`. Merge the
   release to `main`, create an annotated `vX.Y.Z` tag, then merge the release
   back to `develop`.
6. Use `hotfix/<version-or-topic>` from `main` only for urgent released-code
   fixes; merge it to both `main` and `develop` and tag the patch release.

`main` is released code. `develop` is next-release integration. Branch
protection is expected to enforce pull requests and passing CI on both.

## Required checks

Before requesting review, run:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo test --locked --manifest-path fixtures/rust/controller-service-provider/Cargo.toml
RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --no-deps
cargo deny check
git diff --check
```

If rust-analyzer is installed, also run the ignored real-provider smoke test
when provider behavior changes:

```sh
cargo test -p chakra-provider-rust-analyzer --test real_provider -- --ignored --nocapture
```

The repository's `AGENTS.md` defines additional mandatory self-review,
architecture-review, validation, staging, and commit gates for coding agents.

## Containerized language-server tests

Real-provider tests (`tests/real_provider.rs` in the provider crates) are
ignored by default because they need a language server on `PATH`.
`tools/Dockerfile.lsp` builds an image carrying every supported server pinned
to the versions recorded in `docs/languages/*.md`: rust-analyzer from the
pinned rustup toolchain, clangd 21, gopls 0.23.x with the Go toolchain,
pyright, vtsls with a resolvable TypeScript, bash-language-server 5.6.x,
jdtls with a JDK 21 runtime, csharp-ls on the .NET 10 SDK, terraform-ls
0.39.x, and Kotlin's standalone `kotlin-server 263.4702.0` with its bundled
runtime, Maven 3.9.16, Android SDK platform 35, and build tools 35.0.0.
Kotlin acceptance covers Maven, Gradle JVM, Android, and Multiplatform
projects and remains a 0.4.0 release gate; see
[Kotlin verification](docs/languages/kotlin.md#verification) for its current
evidence. `tools/run_lsp_tests.sh` wraps image build and test
execution; only Docker is required on the host.

Run the full suite, including the real-provider tests:

```sh
./tools/run_lsp_tests.sh
```

Without arguments the wrapper first runs `cargo test --locked --workspace`,
then runs each existing ignored real-provider smoke-test target
(rust-analyzer, clangd, csharp-ls, gopls, terraform-ls, pyright, vtsls, jdtls,
bash-language-server, and kotlin-lsp) explicitly. The vtsls target exercises
both TypeScript and JavaScript; the Kotlin target exercises five positive
scenarios (Maven, Gradle JVM, mixed Kotlin/Java, Android and KMP), plus
explicit fallback for standalone scripts. Android checks SDK overloads;
KMP checks source-set callers and build-input regeneration.
The default run also executes Kotlin's ignored `real_gradle_` import-contract
tests, which check the generated model before language-server startup.
Each provider target runs serially, and a
failure is reported without skipping later providers; the overall command
still exits with failure. This avoids accidentally enabling unrelated ignored
benchmarks and large-workspace gates that require external inputs. Provider
adapters are also covered by their hermetic lifecycle suites. PHP has no LSP
adapter and is covered by syntax/resolver and shared conformance tests. Any arguments
replace the default test selection, so a selective provider run looks like:

```sh
./tools/run_lsp_tests.sh -p chakra-provider-gopls -- --ignored
```

The repository is mounted read-write at `/workspace`; `target/`, the Cargo
registry, and the Cargo Git cache live in named volumes (`chakra-lsp-target`,
`chakra-lsp-cargo-registry`, `chakra-lsp-cargo-git`). Gradle distributions
and dependency caches use `chakra-lsp-gradle`; Maven uses
`chakra-lsp-maven`. Reruns stay incremental and the host `target/` is never touched by the root-owned
container. Remove those volumes to force a cold rebuild.

Earlier image builds were about 5 GB and took roughly 15–20 minutes; these
are historical estimates, not measurements of the image with Kotlin's
bundled runtime. Docker caches completed build layers. A cold
container test run additionally compiles the workspace into the target
volume once; subsequent runs start in seconds. The downloaded server
toolchains are x86-64 builds, so the wrapper explicitly selects
`linux/amd64`; ARM hosts need Docker's amd64 emulation.

Do not append workspace-wide `--include-ignored`: it also enables opt-in
harnesses gated on external inputs. `chakra-language/tests/large_workspace.rs` requires
`CHAKRA_LARGE_REPOSITORY` naming an external Git worktree, and
`chakra-mcp/tests/large_repository_gate.rs` must run with
`cargo test --release`. Without those prerequisites they fail by design; the
failure is unrelated to the language-server environment. Benchmark harnesses
under `chakra-conformance` have their own documented release commands. The
real-provider smoke tests themselves need nothing beyond the image.

## Release review

Before freezing an Unreleased changelog section on `release/<version>`:

1. Compare platform, runtime, provider, schema-version, and support claims
   with the final implementation, feature flags, accepted ADRs, and
   platform-specific tests.
   Keep an unpublished candidate labeled as such. Once publication gates
   pass, replace the preparation banner, set the changelog release date,
   and update the README's archive/source examples to the final version.
2. Re-read entries affected by every late `fix:` commit; update historical
   implementation wording rather than publishing an earlier design.
3. Run the full required checks on the final release branch, verify the
   release range with `git diff --check <previous-tag>..HEAD`, and inspect the
   generated support/corpus artifacts.
4. Confirm the release commit, annotated tag, and GitHub release all identify
   the same version and commit.
5. Run the release workflow's manual preflight from the candidate's matching
   `release/X.Y.Z` or `release/vX.Y.Z` branch before merging, then from the
   final `main` commit before tagging. It must build and package every
   supported target and pass
   the `language-acceptance` Docker job without publishing a release. This
   includes real Kotlin Maven, Gradle JVM, Android, and Multiplatform tests;
   the publication job depends on their success.
6. After pushing the annotated tag, wait for the tag-triggered release workflow
   and verify every archive, `SHA256SUMS`, and build-provenance attestation
   before closing the milestone.

## Licensing

Unless explicitly stated otherwise, contributions are submitted under the
project's MIT License. See `LICENSE` for the full terms.

## Architecture and scope

Read only the relevant sections of `docs/SPEC.md`, `docs/roadmap/v0.1.md`, and
accepted ADRs before substantial changes. MCP and language tooling are outward
adapters. Do not leak their protocol types into domain/query contracts, claim
heuristic facts as precise, or turn an ordinary edit into a full parse of the
repository.

Do not expand into deferred PHP type checking, multi-worktree orchestration,
historical materialization, persistence, semantic search, or a web UI without
an explicit roadmap decision.
