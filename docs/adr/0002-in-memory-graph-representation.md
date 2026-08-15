# ADR-002: In-memory graph representation

Status: accepted
Date: 2026-08-15

## Context

v0.1 keeps all code intelligence in memory (roadmap §12 explicitly permits
no persistence). The query set needs: file → symbols, name lookup,
one-hop edge traversal (`callers`, `context`). Traversals are one hop deep;
SPEC §9 forbids eager repository-wide precise call graphs. The
representation must support the atomic publication model of ADR-001.

## Decision

`SymbolGraph` is a plain struct of std collections:

- `Vec<Symbol>` arena; `EntityId` is the arena index (strictly
  revision-scoped identity per SPEC §10);
- `HashMap<RepoRelativePath, Vec<EntityId>>` for file membership
  (`repo_map`);
- `HashMap<EntityId, Vec<Edge>>` in both directions for `callers` /
  `context` adjacency.

Name resolution is a deliberate linear scan over the arena (exact for
`resolve_name`, case-insensitive substring for `symbol_search`): a name
index would be cloned on every update while v0.1 repositories are small.
Add one only when measurements justify it.

Mutation happens only while building privately (`add_symbol` / `add_edge`
with validation: key path must equal location file, endpoints must exist);
after publication the graph is immutable behind an `Arc`.

## Alternatives considered

- `petgraph`: a generic graph library whose index types and algorithms we
  do not need at one-hop depth; an extra dependency to learn around a
  structure this small buys nothing now.
- SQLite (in-memory): persistence machinery without the persistence
  requirement; SQL types would pressure the domain boundary. Roadmap §12
  omits it from the first slice.
- Persistent collections (`im`): considered in ADR-001; deferred until
  update cost is measured.
- Entity-component system / columnar layout: far beyond v0.1 needs.

## Consequences

- Lookups used by v0.1 queries are map hits or single linear scans with
  documented budgets; naive ranking is deliberate until benchmarks say
  otherwise (SPEC §33, roadmap §18).
- The representation is clone-friendly, which keeps ADR-001's private
  update construction trivial.
- If future phases need deep traversal (`impact`), the adjacency maps
  extend without changing the public engine API.

## Validation / follow-up

- Graph invariants are unit-tested in `crates/chakra-engine/src/graph.rs`;
  end-to-end behavior over the Controller → Service → Provider scenario in
  `crates/chakra-engine/tests/scenario.rs`.
- Measure indexing/query latency under roadmap §18 before adding caching
  or alternative structures.
