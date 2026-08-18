# Changelog

All notable changes to Chakra are documented here. Releases use semantic
version tags prefixed with `v`.

## [Unreleased]

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

[Unreleased]: https://github.com/crystaldaking/crystal-chakra/compare/v0.1.1...develop
[0.1.1]: https://github.com/crystaldaking/crystal-chakra/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/crystaldaking/crystal-chakra/releases/tag/v0.1.0
