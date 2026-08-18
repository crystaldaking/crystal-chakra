# ADR-025: Query execution work budgets

Status: accepted
Date: 2026-08-18

## Context

ADR-024 bounds retained response sections, but `context`, `callers`, and
`diff_context` could still scan and aggregate every matching edge/call site
before truncating the final vector. `repo_map` similarly collected and sorted
the complete file inventory before returning one page. A result limit of 20
therefore did not bound CPU or intermediate allocation for a high-degree
symbol or large diff.

## Decision

- Each high-level response section gets independent construction limits
  derived from the requested result limit:

  | Work dimension | Formula | Minimum | Maximum |
  | --- | ---: | ---: | ---: |
  | examined files/symbols/candidates | `limit * 16` | 1,024 | 8,192 |
  | visited graph edges/call sites | `limit * 32` | 2,048 | 16,384 |
  | retained intermediate items | `limit * 8` | 2,048 | 4,096 |
  | section construction wall time | fixed safety cap | 250 ms | 250 ms |

  Request item limits (maximum 500) and ADR-024's independent 16–256 KiB
  response-section limits remain separate dimensions.
- The first exhausted construction dimension stops only the affected section.
  The envelope reports `examined_work_limit`, `graph_traversal_limit`,
  `intermediate_allocation_limit`, or `wall_time_limit` for that public
  section. The omitted amount remains unknown because computing it would
  defeat the bound.
- Edge relationship aggregation retains a deterministic bounded top-k prefix
  keyed by qualified name, entity id, and relationship kind while scanning.
  Repeated retained relationships continue accumulating occurrence counts and
  up to three representative locations.
- Exact simple and qualified-name resolution uses the graph's derived name
  index before traversal. `symbol_search` uses a separate case-folded index to
  seed its bounded top-k with exact candidates across every language partition
  before spending the remaining examined-symbol budget on the broad
  prefix/substring scan. Precise-provider
  relationship matching scans only symbols declared in the provider-reported
  file, avoiding repository-wide symbol intermediates on the enrichment path.
- Each language partition stores its file inventory in a persistent ordered
  map, allowing `repo_map` and text search to stream deterministic paths.
  `repo_map` retains only a bounded examined prefix, sorts that bounded set,
  and derives an exact omitted-file count from graph metadata when the scope
  covers the complete first-page inventory; it no longer allocates and sorts
  one summary per repository file.
- Ambiguous incoming call-site lookup is bounded before allocation and uses a
  bounded set rather than quadratic duplicate checks. The lookup limit is the
  smaller of remaining traversal and intermediate-item capacity. Outgoing
  candidate expansion shares one examined-candidate budget across the selected
  caller and preserves ADR-010's per-call fan-out signal.
- `diff_context` bounds changed-file and changed-symbol inspection before
  relationship traversal. Response-byte truncation of one section never
  narrows the independently budgeted downstream scope: item-scoped file and
  symbol ids are captured before byte truncation. Diff adapters must supply a
  deterministic, adapter-bounded inventory order because the query layer
  intentionally examines only a bounded prefix.
- A completed section retains exact occurrence counts. If a work cause marks
  the section incomplete, counts and representative evidence describe the
  deterministic examined prefix and must not be interpreted as repository
  totals.
- Structured query events report files and symbols examined, candidates
  examined, edges and call sites visited, retained intermediate items,
  bounded response bytes, diff/provider wait time, and construction time.
  MCP serialization measurements remain at the adapter boundary under
  ADR-024.

The 250 ms wall-time cap is a final safety net after deterministic count
budgets. It is independent of ADR-012's cooperative cancellation context,
which is polled through freshness, Git, providers, and CPU loops.

## Alternatives considered

- Truncate only completed vectors: rejected because it leaves CPU and peak
  allocation proportional to repository degree.
- Stop after the first `limit` edges: rejected because repeated edges could
  consume every slot and insertion order would replace deterministic ranking.
- Compute exact omitted counts after a work stop: rejected because the count
  itself requires the traversal the budget intentionally prevented.
- Add a generic graph/actor framework: rejected because one-hop bounded loops
  and ordered standard collections are sufficient.

## Consequences

- High-degree answers can be partial even when their encoded response is
  small; the precise section and cause are always visible.
- Count-based stops are deterministic for an unchanged revision. A wall-time
  stop can retain a different-length prefix on different machines, but its
  returned ordering and incompleteness remain deterministic and explicit.
- File-index updates pay persistent ordered-map insertion cost in exchange for
  eliminating repository-sized query sorts. The exact-name index adds bounded
  per-symbol metadata in exchange for eliminating arena-wide exact lookup.
  Entity ids and revision semantics do not change.
- Cooperative cancellation can stop an in-flight query earlier, while these
  section-local limits guarantee that a request cannot traverse unbounded
  high-degree adjacency even without client cancellation.

## Validation / follow-up

- Release regressions with 3,000 incoming edges, 3,000 repeated ambiguous
  call sites, 5,000 allocation-pressure call sites, 3,000 files, and 3,000
  changed symbols complete with the expected typed stop causes and
  deterministic repeated results.
- Zero-duration unit coverage exercises the wall-time cause without sleeping;
  separate unit cases cover examined, traversal, and allocation causes.
- The large-repository release gate records these query bounds alongside
  indexing and resource measurements; ADR-012 covers adapter-neutral
  cancellation and deadline propagation.
