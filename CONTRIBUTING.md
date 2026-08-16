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

## Architecture and scope

Read only the relevant sections of `docs/SPEC.md`, `docs/roadmap/v0.1.md`, and
accepted ADRs before substantial changes. MCP and language tooling are outward
adapters. Do not leak their protocol types into domain/query contracts, claim
heuristic facts as precise, or turn an ordinary edit into a full parse of the
repository.

Do not expand into deferred PHP type checking, multi-worktree orchestration,
historical materialization, persistence, semantic search, or a web UI without
an explicit roadmap decision.
