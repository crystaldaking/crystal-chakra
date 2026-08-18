# First-Class Language Parity Contract

Status: accepted for v0.1.2 development
Authority: this document defines what "first-class language support" means as a
testable product contract. It refines SPEC §15 without weakening it.

Related: ADR-0026, `docs/support/` (machine-readable matrix), issue #22.

## 1. Motivation

Chakra must not use a vague or tiered meaning of language support. A language is
either **first-class** — it passes every mandatory capability in this contract —
or it is **not advertised**. There is no documented "partial" tier exposed to
users: if a mandatory capability cannot be met by the ecosystem's tooling, we
implement a trustworthy equivalent or we keep the language unadvertised.

The initial target list follows the GitHub Octoverse 2025 top languages plus the
pre-existing Rust and PHP support: TypeScript, JavaScript, Python, Java, C#,
PHP, Shell, C++, HCL, Go, Rust.

## 2. Definitions

- **Capability** — a single pass/fail requirement with a stable identifier
  (e.g. `SYNTAX-03`). Capabilities are the unit of conformance.
- **Mandatory capability** — required for first-class status. Marked *(M)*.
- **Conditional capability** — required only when the language ecosystem makes
  it meaningful (e.g. precise providers); if triggered, it is pass/fail like a
  mandatory one. Marked *(C)*.
- **Evidence** — a pointer to test artifacts that prove the capability:
  a conformance-suite result file, a named test, or a corpus evaluation record.
  Documentation prose alone is never evidence.
- **Equivalent implementation** — Chakra-owned code that delivers a mandatory
  capability the ecosystem provider cannot deliver (e.g. a resolver that
  compensates for a missing provider capability). Equivalents must be tested to
  the same standard as provider-backed capabilities.

## 3. Capability catalog

Every advertised language MUST pass each *(M)* capability. *(C)* capabilities
MUST pass when their trigger condition holds.

### 3.1 Discovery and project model

| ID | Level | Requirement |
|----|-------|-------------|
| DISC-01 | M | Git-aware discovery: tracked plus untracked non-ignored files of the language are discovered; ignored files are excluded; discovery never hardcodes `.git` layout. |
| DISC-02 | M | Ecosystem-aware project scopes: the language's project manifests (e.g. `Cargo.toml`, `composer.json`, `tsconfig.json`, `go.mod`, `pom.xml`/`build.gradle`, `*.csproj`, `package.json`) define project/package boundaries used by queries. |
| DISC-03 | M | Source roles: production, test, generated, and vendored sources are classified using ecosystem conventions and recorded with provenance. |

### 3.2 Syntax intelligence (Tree-sitter or justified equivalent)

| ID | Level | Requirement |
|----|-------|-------------|
| SYNTAX-01 | M | Maintained upstream grammar, pinned to an exact version; selection is recorded in the provider-selection ADR. |
| SYNTAX-02 | M | Declarations: top-level and nested symbol declarations with names and kinds. |
| SYNTAX-03 | M | Containers: namespaces/modules/classes and their nesting relations. |
| SYNTAX-04 | M | Imports: include/use/import statements and aliases as graph facts. |
| SYNTAX-05 | M | Ranges: byte-accurate declaration and reference ranges that survive edits. |
| SYNTAX-06 | M | Test hints: test functions/classes are identified per ecosystem conventions. |
| SYNTAX-07 | M | Diagnostics: actionable syntax diagnostics on parse errors, without losing the last good revision. |
| SYNTAX-08 | M | Bounded syntax call candidates: call sites are extracted with explicit work budgets and deterministic degradation. |

### 3.3 Precise provider

| ID | Level | Requirement |
|----|-------|-------------|
| PRECISE-01 | M | Provider decision recorded: a selected local precise provider, or a recorded deferral with an equivalent implementation plan (see §5). |
| PRECISE-02 | C | When a provider is integrated: precise definitions, references, and callers through the common query contract, with revision-scoped synchronization. |
| PRECISE-03 | C | Owned lifecycle: start, readiness, health state, cancellation, restart with backoff, and shutdown with no orphan processes. |
| PRECISE-04 | C | Capability reporting: the provider reports what it can answer; queries never silently route unsupported requests to it. |
| PRECISE-05 | C | Failure isolation: provider crash, timeout, or absence degrades to syntax intelligence with explicit provenance; queries still answer. |

### 3.4 Query and MCP contract

| ID | Level | Requirement |
|----|-------|-------------|
| QUERY-01 | M | All seven Chakra queries (`status`, `repo_map`, `search`, `symbol_search`, `context`, `callers`, `diff_context`) behave consistently for the language. |
| QUERY-02 | M | MCP exposure: every query is reachable through the MCP tools with identical semantics and error mapping. |
| QUERY-03 | M | `diff_context` scopes (worktree, base ref, merge base) include the language's files with correct changed-file attribution. |

### 3.5 Consistency invariants

| ID | Level | Requirement |
|----|-------|-------------|
| FRESH-01 | M | Atomic revisions: queries only observe atomically published revisions; a normal file change never triggers a whole-repository reindex. |
| FRESH-02 | M | `require_fresh` semantics: after a change barrier, results reflect the change (read-your-writes) or fail with a typed freshness error. |
| PROV-01 | M | Provenance and precision: every returned fact carries syntax/heuristic/precise/textual provenance; precision is never upgraded silently. |
| AMBIG-01 | M | Ambiguity: duplicate and ambiguous names are reported as ambiguity, not silently resolved to one candidate. |
| BUDGET-01 | M | Budgets: query limits, work budgets, truncation, and byte-first response budgets apply uniformly; truncation is reported explicitly. |
| CANCEL-01 | M | Cancellation: cooperative cancellation terminates indexing, provider requests, and queries without orphaned work. |
| DEGRADE-01 | M | Degradation: under resource pressure or provider failure the system returns bounded, provenance-honest degraded results instead of failing or blocking indefinitely. |

### 3.6 Evidence and documentation

| ID | Level | Requirement |
|----|-------|-------------|
| CONFORM-01 | M | The language implements the shared conformance scenario manifest (`#24`) and its per-language result file is emitted in CI. |
| CORPUS-01 | M | The language passes the pinned public large-repository evaluation (`#25`) covering cold index, no-op reconciliation, one-file edit, rename/delete, temporary syntax error, high-degree queries, cancellation, and degradation budgets. |
| DOCS-01 | M | Install/runtime requirements and measured limitations are documented; claims match the generated support matrix. |

## 4. Support matrix

The public statement of language support is a machine-readable matrix, not
prose:

- Per-language manifests live in `docs/support/languages/<language>.json` and
  conform to `docs/support/matrix.schema.json`.
- `tools/check_support_matrix.py` validates manifests, merges conformance and
  corpus results, and regenerates `docs/support/matrix.json` and
  `docs/support/SUPPORT_MATRIX.md`.
- CI runs the checker on every pull request. It fails when:
  - an advertised language misses any mandatory capability or evidence;
  - a manifest is invalid, stale, or its evidence pointers do not exist;
  - committed matrix artifacts differ from regenerated ones.
- A language is advertised only when every *(M)* capability (and every
  triggered *(C)* one) reports `pass` or `equivalent` with evidence.

## 5. Provider eligibility and equivalents

A local precise provider is eligible only when it satisfies all of:

1. **Maintenance** — active upstream maintenance (commits/releases within the
   last 12 months, responsive issue tracker).
2. **Capabilities** — definitions, references, and (directly or through a
   documented equivalent) call hierarchy, plus workspace synchronization.
3. **License** — license-compatible redistribution or documented user-side
   installation.
4. **Resource behavior** — bounded startup and steady-state memory acceptable
   for a local agent tool; measurable through the corpus evaluation.
5. **Lifecycle ownership** — the adapter can own start/health/restart/shutdown
   without orphan processes.
6. **Reproducible installation** — pinned version, documented install path,
   and probes that are reproducible without a developer-global setup.

If an eligible provider lacks a mandatory capability, we either implement a
trustworthy equivalent inside Chakra (tested to the same standard) or leave the
language unadvertised. Deferrals must be recorded as an ADR with the gap, the
rejected alternatives, and the equivalent-implementation plan.

## 6. Implementation mechanisms vs. public contract

Languages may differ in *how* a capability is met — upstream LSP server,
compiler frontend, Chakra-owned resolver, or hybrid — but not in *whether* it
is met. The matrix records the mechanism per capability for transparency; the
pass/fail gate does not reward or punish the mechanism.

## 7. Popularity-list reconsideration policy

- The target language list is reconsidered at every minor release planning
  against the latest GitHub Octoverse data and recorded usage signals.
- Adding a language requires a milestone issue and the full parity path;
  removing one requires an ADR documenting the rationale and user impact.
- The current list and the date of its last review are recorded in
  `docs/support/matrix.json` (`target_list_reviewed_at`).

## 8. Conformance result contract

Conformance harness (`#24`) and corpus evaluation (`#25`) must emit result
files consumable by the matrix checker:

- `docs/support/conformance/<language>.json` — per-scenario pass/fail with
  provenance assertions (not committed for unadvertised languages).
- Corpus evaluation records — per-repository, per-scenario measurements
  against declared budgets; stored per the corpus manifest.

The checker treats missing result files for an advertised language as a
failure: no language becomes first-class through documentation alone.
