# ADR-033: slot-keyed revision-local entity-id partitions

Status: accepted
Date: 2026-08-20

## Context

`SymbolGraph` allocated revision-local entity ids from hardcoded per-language
ranges: Rust incremented from 0, TypeScript from `1 << 62`, and PHP from
`1 << 63`, each with its own `next_*_entity_id` counter and `*_symbol_count`
field (`crates/chakra-engine/src/graph.rs`). Two languages filled the u64
space exactly; a third had to be squeezed in by halving the Rust range, and
the roadmap's eleven target languages did not fit the pattern at all. Every
new language also meant touching three separate sites (id allocation, symbol
counts, language enumeration) with subtly different per-language bounds
logic.

## Decision

- Partition the revision-local entity-id space with an explicit slot
  registry: a 4-bit slot tag in bits 60..64 (`slot << 60`) plus a 60-bit
  per-language counter, giving 16 slots of ~1.15e18 ids each. Slot
  assignment is fixed and documented in code (`language_entity_slot`):
  Rust = 0, Php = 1, TypeScript = 2, Python = 3; 12 slots remain.
- Replace the three counter/count field pairs with
  `next_entity_ids: [u64; 16]` and `symbol_counts: [u64; 16]`, indexed by
  slot. Id allocation, symbol accounting, and language enumeration all go
  through the slot map; adding a language assigns the next slot in exactly
  one place (`language_entity_slot` plus the `ENTITY_SLOT_LANGUAGES`
  iteration order).
- Numeric id values change for PHP (`1 << 63` → `1 << 60`) and TypeScript
  (`1 << 62` → `2 << 60`). This is safe and invisible outside the process:
  the v0.1 graph is in-memory only (ADR-002), no id is ever persisted, and
  `EntityId` is documented as strict identity within one graph revision
  only. Emitted conformance and corpus artifacts contain no entity ids, so
  no artifact regeneration was required (verified: re-emission of
  rust/php/typescript conformance results is byte-identical).

## Consequences

- A new language's id space is one slot assignment away, with no per-language
  bounds code and no renumbering of existing slots.
- Exhaustion of one language's 60-bit counter is still reported per language
  (`EntityIdSpaceExhausted`); the bound is unreachable in practice (larger
  than any admissible symbol count by orders of magnitude).
- Debug representations of ids are no longer human-mappable to a language by
  memory; derive the language from the slot tag (`id >> 60`) or the symbol
  key, not from memorized base constants.
