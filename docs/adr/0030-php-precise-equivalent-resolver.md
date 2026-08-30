# ADR-030: PHP precise-equivalent resolver through strict-tier promotion

Status: accepted
Date: 2026-08-18

## Context

Issue #32 requires PHP to reach the same first-class parity gate as Rust
(#22, `docs/language-parity-contract.md`). ADR-018 deferred a precise PHP
provider after measuring Intelephense, Psalm, and PHPactor: none can serve
precise callers today (PHPactor reached 87.5% definition recall but
advertises no call hierarchy), so PRECISE-02 had no provider-backed path.
The parity contract allows a **Chakra-owned equivalent implementation**
instead of a provider, tested to the same standard (§2, §5).

ADR-0015 already resolves receiver-qualified PHP calls from bounded,
syntactically explicit type evidence (typed parameters, typed and promoted
properties, local `new`, `app(Foo::class)`, fluent reassignment) and rejects
ambiguous or dynamic receivers. Those resolutions are deterministic,
revision-consistent, and conservative — but they were all published as
`heuristic`, which understates the strongest tier and left #32 blocked.

The domain provenance model also lacked a variant for Chakra-owned precise
facts: the conformance provider double borrowed `Provenance::RustAnalyzer`
for non-Rust precise facts (recorded as a limitation in ADR-0028).

## Decision

- Add `Provenance::ChakraResolver` to the domain model: a Chakra-owned
  static resolver producing precise-tier facts from an explicit,
  deterministic evidence rule. The variant is additive and serde-compatible
  (`chakra_resolver`).
- Promote a receiver-resolved PHP call relation to
  `Precision::Precise`/`Provenance::ChakraResolver` only when **both** hold
  (the strict tier):
  1. the receiver type comes from syntactically explicit evidence: a typed
     parameter, a typed or constructor-promoted property, a local `new`,
     `app(Foo::class)`/`resolve(Foo::class)`, or an explicit scoped type —
     including a fluent reassignment that preserves such evidence; and
  2. the type catalog resolves the method to exactly one candidate
     declaration with an unambiguous inheritance traversal.
- Everything else stays heuristic: `$this`, `self`/`static`/`parent`
  receivers (late static binding is genuinely dynamic), dynamic or untyped
  receivers, missing or ambiguous candidates, and Laravel framework-magic
  relations (ADR-0017). Precision is never upgraded silently (PROV-01); the
  promotion rule is stated in code at the promotion point
  (`strict_call_site_tier` in
  `crates/chakra-language-php/src/indexer/catalog.rs`).
- The shared graph derives the `CALLS`/`TESTS` relation tier from the call
  site's own precision, so a promoted call site materializes
  `chakra_resolver`/`precise` edges that survive `callers`, `context`,
  `diff_context`, and MCP serialization unchanged. Non-promoted call sites
  keep the exact ADR-010/ADR-015 tiers.
- This amends ADR-0015: the relation tier is no longer uniformly heuristic;
  the strict subset above is precise. It pairs with ADR-018: the provider
  deferral now ships with the implemented equivalent the parity contract
  requires. ADR-0028's recorded limitation is resolved: the conformance
  provider double now uses `Provenance::ChakraResolver`.
- `status` reports a provider entry only when a precise-provider adapter is
  actually installed, under its real name and supported languages
  (`PreciseProvider::name`); an unconfigured engine reports an empty
  provider list instead of a fabricated rust-analyzer entry.

## Alternatives considered

- Integrate PHPactor as the precise PHP provider: rejected for this gate —
  the measured release advertises no call hierarchy (ADR-0018), so it cannot
  serve precise `callers`; it remains the recorded candidate for a future
  separately approved proof of concept.
- Do nothing and keep PHP unadvertised: rejected — #32 requires the parity
  gate, and the strict-tier evidence was already computed and conservatively
  bounded by ADR-0015.
- Relabel all receiver-resolved heuristic relations as precise: rejected —
  it would silently upgrade precision for evidence that is not
  single-candidate or not syntactically explicit, violating PROV-01.

## Consequences

- PHP callers/context answers contain a precise tier for explicitly typed,
  unambiguous receiver calls without any PHP runtime, Composer install, or
  language server.
- Strict-tier relations claim more trust, so the evidence rule must stay
  conservative: widening it (return-type inference, docblocks, container
  bindings) requires a new ADR.
- The precise-tier promotion is re-derived on every graph materialization
  from current declarations; a declaration edit that makes a method
  ambiguous demotes the relation back to heuristic on the next revision.
- The `PreciseProvider` trait gains a required `name` method; all
  in-workspace adapters and test doubles implement it.
- Known false-negative classes are unchanged and documented in
  `docs/languages/php.md`: dynamic dispatch, magic methods, string
  callables, `method_exists` dispatch, and framework container bindings stay
  unresolved or heuristic.

## Validation / follow-up

- Indexer tests prove promotion and non-promotion:
  `strict_tier_receiver_calls_promote_to_chakra_resolver_precise`
  (parameter, property, promoted property, local `new`, service locator,
  scoped type, and `TESTS` promotion),
  `non_strict_receiver_calls_stay_heuristic` (`$this`, `self`, `static`,
  dynamic receivers), and
  `ambiguous_inherited_candidates_stay_heuristic` (multi-candidate trait
  conflict).
- `realistic_php_fixture_exposes_bounded_syntax_intelligence` proves the
  promoted relation survives `callers`/`context` queries with
  `chakra_resolver`/`precise`; the Laravel fixture tests prove
  framework-magic relations stay heuristic.
- MCP contract coverage proves the promoted tiers serialize as
  `chakra_resolver`/`precise` end to end.
- `status_reports_an_installed_provider_with_its_name_and_languages` and the
  provider-absent conformance scenario cover honest provider reporting.
- The pinned corpus evaluation is re-run so committed artifacts reflect the
  resolver change; both languages remain 14/14 in the conformance harness.
