# Changelog

All notable changes to Chakra are documented here. Releases use semantic
version tags prefixed with `v`.

## [Unreleased]

No changes yet.

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
- PHP v0.1 is first-class at the syntax/query lifecycle level; PHP dynamic
  dispatch and runtime type resolution remain heuristic and no precise PHP
  provider is bundled.
- All development after this release follows the Gitflow policy in
  `CONTRIBUTING.md` and `AGENTS.md`.

[Unreleased]: https://github.com/crystaldaking/crystal-chakra/compare/v0.1.0...develop
[0.1.0]: https://github.com/crystaldaking/crystal-chakra/releases/tag/v0.1.0
