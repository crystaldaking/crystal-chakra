# ADR-002: In-memory graph representation

Status: accepted
Date: 2026-08-15
Last reviewed: 2026-08-18

## Context

v0.1 keeps all code intelligence in memory (roadmap §12 explicitly permits
no persistence). The query set needs: file → symbols, name lookup,
one-hop edge traversal (`callers`, `context`). Traversals are one hop deep;
SPEC §9 forbids eager repository-wide precise call graphs. The
representation must support the atomic publication model of ADR-001.

## Decision

`SymbolGraph` is an immutable/persistent in-memory graph:

- `rpds::RedBlackTreeMapSync<EntityId, Arc<Symbol>>` is a sparse ordered arena.
  Rust and PHP allocate from disjoint revision-local numeric ranges, allowing
  shallow language composition without id remapping. IDs remain strict
  revision-scoped identities per SPEC §10: a caller must still supply the
  matching workspace revision, even when an unchanged payload happens to reuse
  the same numeric id in the next revision;
- a persistent ordered file map stores `Arc<IndexedFile>` membership
  (`repo_map`) and the immutable source text captured for that same graph
  revision (`search` and bounded context snippets); source strings use
  `Arc<str>` so private snapshot construction does not repeatedly copy file
  bodies, and file summaries retain Git provenance/precision;
- persistent hash tries store `Arc<Vec<Edge>>` adjacency in both directions for
  `callers` / `context`; relationship vectors are also grouped by their owner
  file so a private revision can remove and replace one contribution exactly;
- a persistent compact call-site arena plus caller/lookup indexes for
  ambiguous and unresolved syntax calls; only uniquely resolved syntax calls
  become graph edges (ADR-010).

A workspace graph is a shallow immutable list of disjoint Rust/PHP partitions.
Queries traverse that facade, while live reconciliation mutates only a cloned
language partition. There is no third copied “combined graph” and no remapping
of unchanged entities or edges.

Private language materialization uses `BoundedGraphBuilder` (ADR-011). It
checks symbol, edge, and call-site quotas before allocation and reports exact
retained/omitted work. A truncated symbol catalog never resolves call sites,
because removing one declaration could otherwise manufacture false uniqueness.

Name resolution is a deliberate ordered linear scan over live arena entries
(exact for `resolve_name`, case-insensitive substring for `symbol_search`): a
name index would be cloned on every update while v0.1 repositories are small.
Add one only when measurements justify it.

Mutation happens only while building privately (`add_file` / `add_symbol` /
owned relationship and call-site replacement) with validation: files are
unique, key path must equal location file, endpoints must exist, and call-site
resolutions match the revision's callable catalog. After publication the graph
is immutable behind an `Arc`. Capturing text and syntax facts together is
required so a query cannot search filesystem bytes from a different revision
than its symbol results.

Those construction methods maintain the graph invariants in production.
An ordinary edit removes affected owner/caller contributions before replacing
changed declarations; removal refuses to delete a still-referenced entity.
Language delta assembly and shallow composition therefore do not repeat a
complete repository audit. An explicit `audit_consistency` diagnostic
remains available to tests and diagnostic callers: it independently rebuilds
the file, callable, and call-site indexes and compares the two adjacency
indexes as exact edge multisets. The file-index comparison preserves its
deterministic arena ordering. One outgoing-edge hash table both reserves the
edge multiplicity required by resolved call sites and is consumed by the
incoming mirror pass, so the complete audit remains expected-linear even for
high-degree callers and identical parallel edges.

## Alternatives considered

- `petgraph`: a generic graph library whose index types and algorithms we
  do not need at one-hop depth; an extra dependency to learn around a
  structure this small buys nothing now.
- SQLite (in-memory): persistence machinery without the persistence
  requirement; SQL types would pressure the domain boundary. Roadmap §12
  omits it from the first slice.
- Plain `Vec`/`HashMap` arenas: used initially, but measured live publication
  cloned/rematerialized all changed-language and combined facts. Rejected for
  v0.1.1 update paths while retained nowhere in the published representation.
- `imbl`: comparable persistent structures, but MPL-2.0 would broaden the
  repository's dependency license allow-list. `rpds` provides the required
  thread-safe structural sharing under MIT.
- Entity-component system / columnar layout: far beyond v0.1 needs.

## Consequences

- Lookups used by v0.1 queries are map hits or single linear scans with
  documented budgets; naive ranking is deliberate until benchmarks say
  otherwise (SPEC §33, roadmap §18).
- Complete old graphs stay readable through their snapshot `Arc`. A private
  update shares persistent roots and `Arc` payloads, copies only bounded trie
  paths/affected adjacency vectors, and replaces the snapshot with one atomic
  pointer swap. Source bodies remain `Arc<str>`.
- If future phases need deep traversal (`impact`), the adjacency maps
  extend without changing the public engine API.

## Validation / follow-up

- Graph invariants are unit-tested in `crates/chakra-engine/src/graph.rs`;
  end-to-end behavior over the Controller → Service → Provider scenario in
  `crates/chakra-engine/tests/scenario.rs`. Atomic-publication regressions run
  the full independent audit against the revisions visible to readers.
- Measure indexing/query latency under roadmap §18 before adding caching
  or alternative structures.
- Live integration additionally asserts physical `Arc` identity for an
  unchanged file and symbol, zero copied fact payloads, one rebuilt file, and
  immutable old/new query results.
