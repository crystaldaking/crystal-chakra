# Chakra — Product & Architecture Specification

Status: architectural source of truth
Repository: `crystal-chakra`
Product: Chakra
Binary: `chakra`

## 1. Purpose

Chakra is a local Code Intelligence Layer for AI coding agents such as Codex, Kimi, Claude Code, OpenCode, and other MCP-capable clients.

Its job is to expose compact, current, structured facts about source code and Git state:

- repository structure;
- symbols and definitions;
- references and implementations;
- callers/callees and dependency relationships;
- source context;
- tests related to a change;
- Git diff and metadata;
- impact candidates;
- provenance and precision of every non-trivial fact.

Chakra is not:

- an autonomous coding agent;
- an IDE;
- an LLM reasoning engine;
- a vector database wrapped around source chunks;
- a universal shell execution MCP;
- a code modification service.

Core product principle:

> The LLM reasons. Chakra provides evidence-backed facts about the codebase.

## 2. Product hypothesis

Modern coding agents can already search and read files. Chakra is valuable only if it materially improves one or more of:

- number of search/read/tool calls required to locate relevant code;
- input tokens consumed while gathering context;
- time to identify the correct change surface;
- completeness of related files/tests discovered;
- correctness of impact analysis;
- reliability immediately after an agent edits files;
- isolation when multiple agent worktrees exist.

The project must validate this hypothesis early rather than spending months building every future capability.

## 3. North-star architecture

Long-term conceptual model:

```text
AI agent
   │ MCP
   ▼
Chakra query API
   │
   ▼
EffectiveWorkspaceGraph
   =
CommitSnapshot
+
WorktreeOverlay
+
WorkspaceEnrichment
```

### CommitSnapshot

Materialization-independent, immutable, commit-addressed facts that can be derived without checking out a live workspace.

Examples:

- Git tree/file inventory;
- content hashes;
- Tree-sitter-derived declarations and syntax relationships;
- offline import/dependency candidates;
- Git metadata.

A `CommitSnapshot` must **not** be defined as a precise LSP index of a commit.

### WorktreeOverlay

Materialized filesystem delta relative to a base commit:

- modified files;
- staged changes;
- unstaged changes;
- untracked files;
- deleted files;
- renamed files;
- syntax/index deltas derived from those changes.

### WorkspaceEnrichment

Materialization-dependent semantic facts derived from a live workspace and language provider, such as rust-analyzer.

Examples:

- precise definitions;
- precise references;
- implementations;
- semantic type information;
- lazy precise call hierarchy results.

This layer is workspace-scoped and revision-aware. It is not intrinsically reusable for an arbitrary non-materialized Git commit.

## 4. Canonical state and derived state

Canonical source of truth:

```text
Git objects + materialized filesystem/worktree state
```

Derived state:

- syntax index;
- Code Graph;
- language-provider enrichment;
- query caches;
- persistent caches if later justified.

A cache is never trusted solely because it is readable. Compatibility and actual Git/filesystem state must be validated.

## 5. Atomic workspace revisions

Queries must never observe partially applied updates.

A logical update follows:

```text
published revision N
      ↓
observe input change
      ↓
build delta privately
      ↓
validate/update derived state
      ↓
construct revision N+1
      ↓
atomic publish
      ↓
queries see N+1
```

A query may observe revision N or N+1, but never a hybrid.

## 6. Freshness model

A filesystem watcher is a notification mechanism, not a freshness proof.

Chakra must distinguish layers such as:

- filesystem observation revision;
- syntax/index revision;
- language-provider synchronization state;
- published workspace revision.

Queries should support a freshness requirement conceptually equivalent to:

- `allow_stale`
- `require_fresh`

A fresh syntax query must observe the latest reconciled filesystem state.

A fresh **precise** query must not return stale precise provider data while claiming it is current. Chakra must establish a provider synchronization barrier or degrade honestly to the newest lower precision result with provider state metadata.

Example:

```json
{
  "freshness": "fresh",
  "precision": "syntax",
  "provider_state": "catching_up"
}
```

is acceptable when current precise data is not yet available.

Returning old `precise` data as if it describes the current file revision is not acceptable.

## 7. Provenance and precision

Every fact whose reliability depends on its source must carry explicit metadata.

Suggested precision classes:

- `precise`
- `syntax`
- `heuristic`
- `textual`

Suggested provenance examples:

- `rust_analyzer`
- `tree_sitter`
- `git`
- `text_search`
- `heuristic`
- future providers

The exact Rust types may differ, but the distinction is mandatory.

## 8. Code Graph

The Code Graph is the primary structured read model used by the query layer.

Nodes should model repository/code entities with strong types. Symbol specializations should generally be represented through `SymbolKind` rather than a separate top-level domain type per programming-language construct.

Representative symbol kinds:

- module/namespace;
- function/method;
- class/struct/trait/interface;
- enum;
- constant;
- field/property;
- test;
- other provider-specific kinds.

Representative typed edges:

- `CONTAINS`
- `DEFINES`
- `REFERENCES`
- `CALLS`
- `IMPORTS`
- `IMPLEMENTS`
- `EXTENDS`
- `TESTS`
- `DEPENDS_ON`
- `BINDS`
- `RESOLVES`
- `ROUTES_TO`
- `DISPATCHES`
- `LISTENS_TO`
- `SCHEDULES`
- `REGISTERS`
- `AUTHORIZES_WITH`
- `MODIFIED_BY`

Avoid vague edges such as `RELATED_TO` unless their semantics are explicitly defined.

## 9. Call graph strategy

Do not build a repository-wide precise call graph by eagerly invoking LSP call hierarchy for every symbol.

Preferred long-term model:

```text
whole-repository syntax call candidates
+
lazy precise enrichment on demand
```

Precise call edges may be resolved when queries such as `callers`, `context`, or `impact` require them, then cached against the relevant workspace/provider revision.

Any traversal must be bounded by depth/result budgets.

## 10. Identity model

Do not promise a globally stable symbol ID across arbitrary refactors.

Separate:

### EntityId

Strict identity within a specific graph/workspace revision.

### SymbolKey

Language-aware lookup key using relevant properties such as language, qualified name, container, kind, and path.

### SymbolFingerprint

Best-effort fingerprint for correspondence across revisions.

### Lineage

Optional mapping between an entity in one revision and a probable corresponding entity in another. Lineage may be heuristic and must be marked accordingly.

## 11. Repository and file identity

Repository identity must not rely only on an absolute local path.

The design must account for:

- path moves;
- multiple worktrees;
- repositories without a remote;
- remote URL changes;
- local-only repositories.

Files should use normalized repository-relative paths scoped by the required repository/workspace/commit identity.

## 12. Git model

Git is a first-class subsystem.

Chakra must understand, as capabilities evolve:

- HEAD and commit;
- staged and unstaged changes;
- untracked files;
- deletes/renames;
- branches;
- linked worktrees;
- Git operations that can alter filesystem and metadata state.

Never hardcode the `.git` directory layout. Linked worktrees can use a `.git` file and shared administrative storage. Resolve Git paths using Git-aware APIs/commands.

Potential implementation candidates include the Git CLI, `gix`, `git2`, or a justified combination. Do not implement Git object semantics from scratch.

## 13. Commit snapshot semantics

A historical commit that is not materialized can only be indexed with materialization-independent techniques.

Therefore:

```text
CommitSnapshot(commit)
```

contains offline/syntax-tier information unless that commit is explicitly materialized and separately enriched.

Precise language-provider enrichment for a historical commit requires materialization, for example a detached temporary worktree. Historical materialization is **not required in v0.1** and should not become default behavior without explicit resource/security/lifecycle design.

If precise enrichment is cached in the future, treat it as an optimization with a richer environment fingerprint, not as intrinsic commit truth.

## 14. Future snapshot compatibility

If persistent commit snapshots are introduced later, a cache key must account for more than commit SHA. Relevant compatibility inputs can include:

- repository identity;
- commit SHA;
- Chakra index format version;
- graph model version;
- parser/query version;
- language/provider versions where relevant;
- indexing configuration fingerprint.

SQLite schema version and semantic index format version are distinct concepts.

Cache existence must be justified by benchmarks comparing restoration to deterministic rebuild.

## 15. Language provider architecture

Language intelligence is an adapter boundary.

Core APIs should express Chakra concepts rather than LSP protocol structs.

A provider may expose capabilities such as:

- symbols;
- definitions;
- references;
- implementations;
- semantic relationships;
- provider synchronization state.

Provider capabilities differ by language and tool.

"First-class language support" means:

- integrated lifecycle;
- capability reporting;
- common Chakra query contract;
- provenance/precision metadata;
- graceful degradation;
- testing against the common provider contract where possible.

It does **not** mean identical semantic precision across languages.

## 16. Rust provider

The initial precise provider is rust-analyzer.

It must be treated as an owned child/service with explicit:

- start;
- initialization;
- health state;
- synchronization tracking;
- restart policy;
- cancellation/shutdown;
- error/degraded state.

Avoid orphan processes.

Do not assume that rust-analyzer can cheaply provide a whole-repository precise call graph.

For v0.1, rust-analyzer advertises Rust-only capability. The provider adapter
must ignore PHP documents, and the query layer must not send PHP symbols to it.
PHP remains fully usable through current Tree-sitter syntax intelligence when
no precise PHP provider is configured.

## 17. Future multi-worktree provider resource model

Long term, precise language providers are logically worktree-scoped because uncommitted worktree states must not leak into each other.

However, running one heavyweight language server per worktree without bounds is unacceptable.

Future provider orchestration must include a resource manager with concepts such as:

- maximum active providers;
- memory/process budget;
- idle timeout;
- LRU/idle eviction;
- warm/cold states;
- on-demand activation;
- graceful fallback to syntax intelligence.

Multi-worktree provider orchestration is deferred beyond v0.1.

## 18. Syntax intelligence

Tree-sitter is the syntax-aware baseline.

It should support efficient extraction of:

- declarations;
- symbol containers;
- imports;
- function/method/class/struct/trait-like constructs;
- syntax call candidates;
- test/framework hints where practical.

Use incremental parsing when measurements and implementation shape justify it.

Do not attempt to recreate a full type system with Tree-sitter.

## 19. Text search

Provide fast exact/regex search, using ripgrep or an equivalently mature approach unless Phase 0 demonstrates a reason otherwise.

Search must respect project exclusions by default:

- `.gitignore`;
- Git-ignored generated outputs;
- binaries;
- `target/`;
- other configured ignores.

A text match is a textual candidate, not a precise code reference.

## 20. File discovery and watcher policy

Default indexed scope should approximate:

```text
Git-tracked files
+
untracked non-ignored files
```

Do not recursively watch generated/vendor trees merely because they exist.

For large repositories, watcher implementation must account for platform limitations such as inotify/watch descriptor pressure. Prefer a strategy that can reconcile from actual filesystem/Git state if events are dropped.

Submodules should initially be treated as separate repositories rather than silently merged into the parent graph.

## 21. Incremental indexing

A normal file change should follow approximately:

```text
filesystem event
→ debounce/coalesce
→ read latest file state
→ content hash / change detection
→ parse affected file
→ compute graph/index delta
→ update private next revision
→ atomic publish
```

Never infer that one watcher event equals one user edit. Editors often save using temporary file replacement/rename sequences.

Queues in watcher/indexing pipelines should be bounded.

## 22. Reconciliation

Watcher events can be dropped or reordered. Git operations can invalidate assumptions.

Chakra therefore needs deterministic reconciliation points that compare derived state with actual Git/filesystem state.

Fresh queries may trigger a lightweight reconciliation barrier when required.

## 23. Query layer

The query/application layer must be independent of MCP transport.

Potential user-facing capabilities:

- `status`
- `repo_map`
- `search`
- `symbol_search`
- `symbol`
- `references`
- `implementations`
- `callers`
- `callees`
- `dependencies`
- `dependents`
- `context`
- `impact`
- `diff_context`
- `history`

v0.1 intentionally implements only a subset; see `docs/roadmap/v0.1.md`.

## 24. Symbol resolution

Queries must not assume a human-readable name uniquely identifies a symbol.

Preferred flow:

```text
symbol_search(query)
→ candidates
→ EntityId / stable-in-revision reference
→ entity-based queries
```

High-level queries may auto-resolve when there is one unambiguous candidate.

Ambiguity should be returned, not guessed away.

## 25. `context`

`context` is a primary agent-oriented query intended to replace many low-level reads/searches.

It can combine bounded information such as:

- resolved symbol;
- definition/location;
- signature/docs;
- selected callers/callees;
- implementations;
- related tests;
- nearby/related files;
- relevant Git metadata;
- bounded source snippets.

Every component retains provenance/precision and freshness metadata.

## 26. `diff_context`

`diff_context` is a primary product feature.

Conceptually:

```text
Git/worktree diff
→ changed files
→ changed symbols
→ graph relationships
→ related callers/dependencies/tests
→ bounded structured result
```

The result must distinguish facts from heuristics.

Missing-test suggestions, if any, must be explicit deterministic heuristics, not hidden LLM reasoning.

## 27. `impact`

Impact analysis is graph traversal plus documented deterministic rules.

Potential inputs include:

- direct/transitive callers;
- implementations;
- dependent modules;
- public API surfaces;
- related tests.

Every result should explain why it appears and how far it is from the changed entity.

Traversal must be bounded.

A standalone full `impact` tool is deferred until after initial product validation; v0.1 may expose a smaller impact slice through `diff_context`.

## 28. Query response envelope

MCP/query results should be structured, versioned, and bounded.

Conceptual fields:

```json
{
  "schema_version": 7,
  "workspace_id": "...",
  "revision": 42,
  "freshness": "fresh",
  "status": "ready",
  "truncated": false,
  "truncation": [],
  "data": {}
}
```

`truncated` is a summary convenience; every true value must be backed by at
least one bounded, typed entry in `truncation` that identifies the affected
response section, the budget cause, the configured limit, and the omitted
amount when it is known without unbounded work. Workspace-wide ambiguity or
indexing degradation is status data, not a reason to mark an unrelated query
section incomplete.

The exact schema is an implementation decision and should be tested as a contract.

## 29. Query budgets

Potentially large queries must support sensible limits such as:

- `max_results`;
- `max_nodes`;
- traversal `depth`;
- source inclusion;
- test inclusion;
- history inclusion.

Current high-level query collections additionally have independent serialized
byte budgets. Repeated caller/test relations are aggregated by caller and
relationship target with an exact occurrence count and bounded representative
source evidence, so repeated sites do not consume unrelated result slots.
Construction also has per-section examined-item, graph-traversal,
intermediate-allocation, and wall-time budgets. A work-truncated section
reports its cause explicitly; occurrence counts in that section describe the
examined prefix rather than claiming repository-total completeness.

Never return an unbounded graph dump through MCP.

## 30. MCP architecture

MCP is a thin transport adapter over the query layer.

Do not model the domain in terms of MCP SDK structs.

Prefer the current maintained Rust MCP SDK rather than implementing the protocol manually.

Long-term transports may include Streamable HTTP and a stdio bridge, but v0.1 should choose the minimum transport required for real agent validation.

## 31. Daemon discovery and local IPC

Long-term `chakra serve` should own runtime state. If a separate `chakra mcp` bridge is introduced, daemon discovery must be explicit and deterministic.

Preferred direction for local same-user IPC:

- Unix domain socket on Unix-like systems;
- named pipe or appropriate same-user local IPC on Windows;
- explicit fallback strategy.

Do not depend on random localhost port scanning.

If HTTP is exposed directly:

- bind to loopback by default;
- never bind `0.0.0.0` by default;
- use an explicit same-user/session authentication strategy when the transport can be reached by other local processes;
- avoid leaking source data to unauthenticated local callers.

The exact daemon/bridge model is deferred until it becomes necessary for product validation.

## 32. Storage

Do not assume persistent graph storage is required.

v0.1 should prefer deterministic rebuild unless startup benchmarks prove it is unacceptable.

Future storage may use SQLite for:

- repository registration/config;
- metadata;
- cache bookkeeping;
- compatible snapshots if justified.

An in-memory graph may remain the active query representation.

## 33. Performance philosophy

Design for large repositories, but measure before adding complexity.

Relevant future scales include:

- ~100k files;
- 1M+ LOC.

Desired interactive feel after indexing:

- fast symbol lookup;
- fast direct relationship queries;
- bounded `context`;
- sub-second ordinary single-file update where practical.

Targets are engineering budgets, not unsupported SLA claims.

## 34. Resource budgets

All long-lived or potentially explosive resources need explicit bounds:

- watcher queues;
- indexing queues;
- graph traversal;
- source snippets;
- MCP response size;
- child language-provider processes;
- caches;
- concurrent expensive queries.

Current high-level queries bound result items, serialized response bytes,
examined candidates, visited edges/call sites, retained intermediate items,
and section construction time as separate dimensions.

Future multi-worktree language-provider orchestration must include explicit provider/memory limits.

## 35. Concurrency

Avoid a global `Mutex<Everything>`.

Preferred properties:

- immutable/read-mostly published revisions;
- private update construction;
- per-workspace ownership/synchronization;
- bounded channels;
- cancellation-aware long operations.

Do not introduce an actor framework unless the code benefits concretely.

Avoid holding locks across `.await` where possible.

## 36. Lifecycle and cancellation

Every background task and child process must have:

- an owner;
- cancellation mechanism;
- shutdown behavior;
- observable failures.

Potentially long operations such as initial indexing, provider requests, and graph traversal should be cancellable.

## 37. Reliability and degraded behavior

Chakra should survive:

- syntax errors during editing;
- a failed rust-analyzer process;
- Git operations in progress;
- dropped watcher events;
- deleted files/worktrees;
- incompatible/corrupted future cache data.

The system should expose clear states such as:

- initializing;
- indexing;
- ready;
- degraded;
- stale;
- failed.

## 38. Security

Chakra runs against potentially untrusted repositories.

Requirements:

- no arbitrary MCP shell execution;
- no shell string concatenation from user/repository-controlled paths;
- structured process arguments;
- path normalization and traversal protection;
- conservative symlink handling;
- same-user local transport defaults;
- controlled child-process environment;
- explicit trust decisions if future provider behavior can execute repository code/build scripts.

## 39. Privacy

Core Chakra should work offline after required local tools are installed.

Do not send source code to external AI, embedding, analytics, or telemetry services by default.

## 40. Rust engineering baseline

Use current pinned stable Rust and Edition 2024.

At project bootstrap, the pinned baseline is Rust `1.97.1` unless Phase 0 finds a newer stable release before the first implementation commit and updates the repository consistently.

Use Cargo resolver 3.

Prefer:

- strong domain types;
- newtypes/enums over stringly typed state;
- typed errors;
- focused crate/module APIs;
- minimal public surfaces;
- explicit ownership.

Avoid:

- recoverable `panic!`/`unwrap`/`expect`;
- giant modules;
- accidental async blocking;
- unowned background tasks;
- boolean parameter soup;
- unnecessary clone-heavy hot paths.

`unsafe` is forbidden by default.

## 41. Cargo workspace direction

The project should be a modular monolith in one Cargo workspace.

A possible end-state crate map is:

```text
chakra-domain
chakra-graph
chakra-git
chakra-language
chakra-language-php
chakra-language-rust
chakra-storage (when needed)
chakra-engine
chakra-mcp
chakra-cli
```

v0.1 should not create every future crate prematurely. Crates exist to enforce meaningful compile-time boundaries, not to mirror every noun in this spec.

## 42. Dependency policy

Use workspace-managed dependencies where useful.

Before adding a heavy dependency, consider:

- maintenance;
- security advisories;
- licenses;
- transitive dependency size;
- compile cost;
- binary impact;
- API stability.

Use `cargo-deny` or the current appropriate equivalent for dependency policy and advisory/license checks.

## 43. Observability

Use structured tracing, likely with `tracing` unless Phase 0 identifies a better fit.

Useful spans/events include:

- repository scan/index;
- file parse/index;
- revision publish;
- query execution;
- provider requests/state;
- Git reconciliation;
- watcher events.

Do not log source bodies by default.

## 44. Testing strategy

Use layers:

- unit tests;
- integration tests;
- Git temporary-repository tests;
- provider contract tests;
- MCP end-to-end tests;
- regression tests for revisions/freshness;
- targeted benchmarks.

Prefer deterministic synchronization tests over arbitrary sleeps.

Do not make the default test suite depend on globally installed rust-analyzer. Keep optional real-provider integration coverage separate where necessary.

## 45. Required architectural regressions over project lifetime

As corresponding features are introduced, cover:

- incremental update without full reindex;
- atomic revision publication;
- read-your-writes freshness;
- syntax-error degradation/recovery;
- provider crash/degraded fallback;
- rename/delete without ghost graph entities;
- Git diff → changed symbol mapping;
- future multi-worktree isolation;
- future cache invalidation;
- future daemon restart/recovery;
- future bridge/daemon state sharing.

## 46. AI-friendly repository design

Repository guidance is intentionally layered:

- `AGENTS.md`: small mandatory operating rules;
- `.agents/skills/*`: reusable workflows such as review/validation/commit;
- `docs/SPEC.md`: architecture north star;
- `docs/roadmap/v0.1.md`: current scope authority;
- `docs/adr/*`: durable architectural decisions.

Do not turn `AGENTS.md` into a duplicate of this specification.

## 47. Git as a first-class development artifact

The repository must use Git from project initialization.

Agents should commit at meaningful implementation boundaries after self-review and validation. No Git hooks are required to enforce the agent workflow; project instructions and skills define the process.

History should remain readable and useful for later agents.

## 48. MVP strategy

The complete architecture is intentionally larger than the first product slice.

`docs/roadmap/v0.1.md` defines the only mandatory features for the first validation release.

v0.1 exists to answer:

> Does Chakra make a real coding agent measurably better at navigating and understanding live Rust and PHP repositories?

If the answer is not demonstrated, do not blindly build every later feature in this document.

## 49. Explicit non-goals before validation

Unless promoted by a later roadmap decision, do not build:

- multi-worktree orchestration;
- arbitrary historical commit materialization;
- persistent graph snapshot reuse;
- semantic/vector search;
- cross-repository graph;
- eager precise whole-repository call graph;
- web UI;
- distributed workers;
- Jira/Slack/runtime-log knowledge graph;
- autonomous code modification.

## 50. Product evaluation

After v0.1, evaluate Chakra on real coding-agent tasks.

Compare agent runs without Chakra vs with Chakra where practical.

Useful signals:

- search/read/shell calls;
- input tokens spent gathering repository context;
- latency to identify relevant change surface;
- related tests/files discovered;
- task success/correctness;
- frequency of stale-context mistakes;
- perceived usefulness of `context` and `diff_context`.

Use the results to decide which north-star capabilities deserve implementation next.
