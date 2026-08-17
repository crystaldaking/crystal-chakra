# ADR-011: Bounded syntax indexing and graceful degradation

Status: accepted
Date: 2026-08-18

## Context

The original eager call-candidate representation made the pinned Zed corpus
(1,929 Rust files, 55.3 MB) take 179–186 seconds and approximately 5.8 GB peak
RSS before Chakra became queryable. The smaller `psp-app` PHP corpus exposed a
different problem: aggregate startup timing did not identify whether discovery,
reads, parsing, relationships, validation, composition, or publication owned
the cost.

SPEC §§33–37 require explicit resource bounds, private complete construction,
atomic publication, cancellation, and useful degraded behavior. ADR-010 removes
the eager same-name call fan-out, but allocation must remain bounded even for
pathological files or repositories.

## Decision

- Define `IndexBudgets` in `chakra-domain`, with validated safe defaults and
  hard configuration ceilings. One revision is limited to 100,000 source files,
  8 MiB per source, 128 MiB total source, 500,000 symbols, 1,000,000 edges, and
  1,000,000 compact call sites. Cold-start and phase-sampled RSS targets default
  to 120 seconds and 2 GiB. Their hard ceilings prevent accidental disabling of
  safety bounds.
- Treat count/byte budgets as deterministic construction controls. Treat wall
  time and RSS as observable targets only: crossing either produces a structured
  warning but never changes graph contents, because scheduler, allocator, and OS
  observations are not deterministic inputs.
- Discover supported files once through `chakra-git`, then read and partition
  the same lexical Git inventory into Rust and PHP. Files beyond file/source
  budgets are skipped deterministically. The admitted files remain sufficient
  for `repo_map`, text search, and any declarations that fit later graph limits.
- Split graph quotas between non-empty language adapters in proportion to their
  admitted file counts at cold start. Rebalance once if live reconciliation
  activates a language that previously had no files, then retain that allocation
  so ordinary file-count changes do not churn the whole graph. Each adapter
  constructs through `BoundedGraphBuilder`,
  which checks symbol, edge, and call-site limits before allocation. Relationship
  contributions are capped during construction, not after an unbounded vector
  has already been built.
- Never resolve calls against a truncated declaration catalog. If a language's
  symbol budget is exceeded, its call sites are omitted with an explicit
  `symbols → call_sites` degradation reason; a missing duplicate declaration
  therefore cannot make an ambiguous call appear uniquely resolved.
- Keep degradation causes capability-specific. Metadata distinguishes files,
  per-file bytes, workspace bytes, symbols, edges, and call sites, and identifies
  whether declarations, relationships, call sites, text search, or inventory
  were affected. Coverage reports retained and omitted counts.
- Measure Git inventory, source reads, parse/extraction, catalog construction,
  relationship construction, graph materialization, graph validation, language
  composition, live reconciliation, and revision publication separately.
  Source/parsed/graph retained categories and best-effort current/phase-sampled
  RSS are also exposed. macOS RSS uses `ps`; Linux reads `/proc/self/status`.
- Attach `IndexingStatus` to the immutable `WorkspaceSnapshot`. Query envelope
  schema v2 carries that exact revision's budgets, coverage, capabilities,
  degradation reasons, phases, and memory observations for every query. A
  degraded but reconciled revision is `status: degraded`, `freshness: fresh`.
- Reuse the same budgets during live reconciliation. Ordinary admitted-file
  edits still reparse only changed files and affected relationship owners. If a
  relationship budget is already exceeded or an incremental contribution would
  cross it, rebuild that bounded relationship layer in stable path order so the
  degraded graph is independent of edit history.
- Initial indexing accepts a shared cooperative cancellation flag and checks it
  between file/phase units. Cancellation returns a typed error and never
  publishes partial state. End-to-end MCP query cancellation remains issue #20.
- Do not add a scheduler or parallelism dependency here. Bounded parallel
  parsing is issue #21 and must use these work/memory contracts.

## Alternatives considered

- Fail startup when a source or graph limit is reached: rejected because files,
  text, and declarations below the limit are immediately useful and can be
  published consistently.
- Build everything and truncate the finished graph: rejected because it does
  not prevent the allocation spike the budgets exist to control.
- Let each language consume every global limit: rejected because a mixed
  workspace could allocate twice the advertised budget.
- Use elapsed time or RSS to stop at the current file: rejected because repeated
  runs could publish different entities for unchanged content.
- Resolve calls against only retained declarations: rejected because truncation
  could manufacture false uniqueness and violate provenance semantics.

## Consequences

- `chakra-language` now depends directly on the existing `chakra-git` crate so
  workspace discovery/read work is shared rather than duplicated by adapters.
  No third-party production dependency was added.
- Query envelope schema advances from v1 to v2. The new `indexing` member is
  additive but intentionally versioned because clients can now reason about
  incomplete capabilities instead of inferring completeness from `truncated`.
- Language graph quotas can leave unused capacity when one language has a much
  higher fact density than its file share. This conservative policy is stable
  and safe; measurements may justify a two-phase allocator later.
- The current v0.1 arena still duplicates/remaps language graphs during
  composition. Issue #16 owns structural sharing and transient publication
  memory; this ADR bounds contents without claiming that work is already
  structurally incremental.
- A degraded relationship layer may perform a complete bounded relationship
  rebuild to restore deterministic lexical allocation. The ordinary
  under-budget single-file path remains incremental.
- The first file of a previously absent language rebalances graph quotas and
  rematerializes cached facts for both adapters without reparsing unchanged
  files. Subsequent creates/deletes retain that allocation.

## Validation / follow-up

- Unit/integration tests cover invalid configuration, file/source/symbol/edge/
  call-site boundaries, deterministic degradation, incomplete-catalog call
  safety, cooperative cancellation, atomic indexing metadata, useful degraded
  queries, and a degraded one-file live update.
- An opt-in release harness accepts `CHAKRA_LARGE_REPOSITORY`, validates every
  published bound and phase, audits the graph, then executes `repo_map` and
  `symbol_search` through the query layer.
- On the pinned public Zed commit
  `bc538def4545534201bbfcac4e95ac34ea6501b6`, the release harness indexed 1,929
  files / 55,346,203 source bytes in 5.436 seconds, retained 116,197 symbols,
  197,549 edges, and 560,306 call sites, and stayed below default budgets. The
  test process reported approximately 1.09 GB current RSS; `/usr/bin/time -l`
  reported 1,189,871,616 bytes maximum RSS. Atomic publication took 2 µs in
  the harness.
- On the current private `psp-app` worktree (source not copied), the same harness
  indexed 1,158 PHP files / 7,568,594 bytes in 0.991 seconds; `/usr/bin/time -l`
  reported 182,452,224 bytes maximum RSS. Atomic publication took 1 µs.
- Issue #15 owns repeatable generated CI gates and the final v0.1.1 readiness
  record. Issue #16 owns structural publication, #17 freshness scan cost, #20
  end-to-end query cancellation, and #21 bounded parallelism.
