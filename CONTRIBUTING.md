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
jdtls with a JDK 21 runtime, csharp-ls on the .NET 10 SDK, and terraform-ls
0.39.x. `tools/run_lsp_tests.sh` wraps image build and test execution; only
Docker is required on the host.

Run the full suite, including the real-provider tests:

```sh
./tools/run_lsp_tests.sh
```

Without arguments the wrapper first runs `cargo test --locked --workspace`,
then runs each existing ignored real-provider smoke-test target (rust-analyzer,
clangd, gopls, and terraform-ls) explicitly. This keeps the single command
green without accidentally enabling unrelated ignored benchmarks and
large-workspace gates that require external inputs. Provider adapters without
a real-server test target are still covered by their hermetic lifecycle suites
and by the image's executable/version checks. Any arguments replace the
default test selection, so a selective provider run looks like:

```sh
./tools/run_lsp_tests.sh -p chakra-provider-gopls -- --ignored
```

The repository is mounted read-write at `/workspace`; `target/`, the Cargo
registry, and the Cargo Git cache live in named volumes (`chakra-lsp-target`,
`chakra-lsp-cargo-registry`, `chakra-lsp-cargo-git`), so reruns stay
incremental and the host `target/` is never touched by the root-owned
container. Remove those volumes to force a cold rebuild.

The image is about 5 GB; a cold build takes on the order of 15–20 minutes
(mostly toolchain downloads) and is then fully cached by Docker. A cold
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
2. Re-read entries affected by every late `fix:` commit; update historical
   implementation wording rather than publishing an earlier design.
3. Run the full required checks on the final release branch, verify the
   release range with `git diff --check <previous-tag>..HEAD`, and inspect the
   generated support/corpus artifacts.
4. Confirm the release commit, annotated tag, and GitHub release all identify
   the same version and commit.
5. Run the release workflow's manual preflight from the final `main` commit
   before tagging. It must build and package every supported target without
   publishing a release.
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
