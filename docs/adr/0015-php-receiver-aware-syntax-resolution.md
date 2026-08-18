# ADR-015: Receiver-aware PHP syntax call resolution

Status: accepted
Date: 2026-08-17

## Context

ADR-010 stopped expanding an unknown PHP receiver to every same-named method,
but most real PHP member calls consequently remained unresolved. In the
`psp-app` evaluation, this was much more honest than name fanout yet still
discarded useful source-level evidence: constructor injection, typed
parameters, imports, and Laravel's `app(Foo::class)` form frequently make a
receiver type syntactically explicit.

Chakra must improve those results without turning its Tree-sitter adapter into
a PHP type checker or introducing a mandatory PHP language server. Any inferred
target remains a syntax-backed heuristic about runtime dispatch.

## Decision

- Resolve PHP namespace references before catalog lookup. Class/interface/
  trait aliases, grouped imports, function imports, absolute names, and
  `namespace\...` names are normalized to Chakra's `::`-separated qualified
  names. An unqualified function inside a namespace is looked up in that
  namespace; Chakra does not invent PHP's runtime global-function fallback.
- Infer a single receiver type only from bounded, explicit syntax:
  - `$this`;
  - typed parameters, including anonymous- and arrow-function parameters;
  - typed properties and constructor-promoted properties;
  - local `new Foo` assignments;
  - `app(Foo::class)` and `resolve(Foo::class)`;
  - explicit scoped types and `self`, `static`, or `parent`;
  - a same-variable fluent reassignment such as `$service =
    $service->configure()`, which preserves the prior explicit type evidence.
- Do not infer from variable names, arbitrary return values, docblocks,
  framework container bindings, dynamic class strings, magic methods, or
  branch-dependent assignments. Unknown or conflicting evidence remains an
  unresolved call site.
- Retain the inferred receiver type and its typed source (`parameter`,
  `promoted_property`, `service_locator`, and so on) on the shared call-site
  model. `receiver_hint` remains bounded diagnostic source text and is never a
  type identity.
- Resolve inherited methods with a PHP-private type catalog built from parsed
  class/interface extension, implementation, and trait-use facts. Lookup checks
  the receiver itself, used traits, extended parents, and then implemented
  interfaces; multiple declarations in the first producing tier remain
  unresolved instead of being guessed. An interface-typed receiver may resolve
  to the interface declaration; this does not claim a concrete runtime
  implementation.
- Publish the selected declaration container as the call-site lookup
  qualifier while retaining the original inferred receiver type separately.
  The materialized `CALLS`/`TESTS` relation remains `heuristic`; the receiver
  evidence itself remains `tree_sitter`/`syntax`.
- Expose receiver evidence on materialized syntax call relations through an
  optional `call_site` object in `RelatedSymbol`. Precise provider relations
  and non-call relationships do not fabricate syntax evidence.
- Apply ADR-012 to receiver-resolved test calls: repeated calls retain their
  occurrence evidence but produce one test/target relationship, while dynamic
  or ambiguous receivers produce no `TESTS` relation.
- Keep receiver facts in reusable per-file parse results. Every private graph
  materialization rebuilds the lightweight method/type catalog and re-resolves
  call sites against current declarations. A declaration edit therefore
  reparses and recomputes only its affected file contributions while unchanged
  callers are re-resolved in the next atomic revision.

## Alternatives considered

- Resolve every same-name method after finding a plausible receiver token:
  rejected because a token such as `$service` is not type evidence and would
  restore ADR-010's false fanout.
- Implement flow-sensitive PHP type inference: rejected because aliases,
  unions, conditionals, framework containers, magic methods, and dynamic PHP
  semantics quickly become a partial type checker. v0.1.1 needs a bounded
  syntax improvement, not a second static analyzer.
- Add PHP-specific target ids to the shared graph contract: rejected because
  revision-local ids cannot be cached in parsed per-file facts. Qualified type
  evidence composes cleanly with private revision materialization.
- Require a PHP language server: rejected because PHP remains useful at the
  deterministic syntax tier and provider lifecycle/licensing is a separate
  product decision.

## Consequences

- The shared call-site/query contract gains additive receiver type/source
  evidence. Rust call sites set these PHP-specific inference fields to `None`.
- PHP parsing keeps small callable-local maps for parameters/assignments and a
  class-local property map. Work is bounded by the parsed file and does not
  create repository-wide per-call candidate lists.
- Inheritance lookup traverses only known syntax relations with a visited set;
  malformed cycles terminate deterministically.
- Conservative gaps remain visible: return-type propagation, arbitrary
  factory methods, dynamic properties, generic/docblock types, and runtime
  container bindings are unresolved.

## Validation / follow-up

- Parser tests cover aliases, grouped/function imports, properties,
  constructor promotion, closure parameters, local construction, service
  locators, fluent reassignment, scoped calls, and dynamic negative cases.
- Index tests cover same-name fanout rejection, class/interface/trait lookup,
  unresolved dynamic receivers, and declaration changes without reparsing an
  unchanged caller.
- MCP coverage proves that a resolved PHP caller exposes syntax receiver
  evidence while the relation remains heuristic.
- A 256-call synthetic measurement verifies one compact call site and one
  selected target per typed receiver without ambiguity expansion.
- A release-mode `psp-app` evaluation indexed 1,158 PHP files and 62,700 call
  sites in about 1.2 seconds on the evaluation machine. The intended
  regressions resolved exactly two `candidateCutoff` calls and 25 `syncStatus`
  calls, while `ExpirePendingTransactionsJob::handle` received only its eight
  syntactically justified call occurrences and no unrelated same-name methods.
