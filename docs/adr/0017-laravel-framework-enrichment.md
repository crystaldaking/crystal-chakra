# ADR-017: Deterministic Laravel framework enrichment

Status: accepted
Date: 2026-08-17

## Context

Generic PHP syntax cannot prove framework relationships such as a container
binding, route target, queued job handler, listener subscription, or policy
registration. Treating these as ordinary PHP calls would misstate their
semantics, while putting Laravel conventions into the generic parser would
make the PHP language adapter framework-specific.

Chakra nevertheless needs these relationships to make `context` and
`diff_context` useful on realistic Laravel worktrees. The implementation must
not require a PHP runtime, execute Composer, or allow framework guesses to be
reported as syntax/provider precision. It must also preserve per-file
incremental reconciliation and bounded responses.

The supported conventions follow the official Laravel documentation for the
[service container](https://laravel.com/docs/13.x/container),
[controllers and route actions](https://laravel.com/docs/13.x/controllers),
[events/listeners](https://laravel.com/docs/13.x/events),
[queued jobs](https://laravel.com/docs/13.x/queues),
[task scheduling](https://laravel.com/docs/13.x/scheduling), and
[policy registration](https://laravel.com/docs/13.x/authorization).

## Decision

- Detect Laravel only from the root `composer.json` when its direct `require`
  object contains `laravel/framework`, `laravel/lumen-framework`, or
  `illuminate/foundation`. Metadata is parsed with a 1 MiB input budget through
  `serde_json`; PHP and Composer are never executed.
- Treat unreadable, oversized, or invalid Composer metadata as an optional
  enrichment degradation: emit a diagnostic warning, disable Laravel facts,
  and continue publishing ordinary PHP syntax intelligence.
- Keep Laravel extraction in a separate `laravel` module owned by
  `chakra-language-php`. The generic `parser` module remains unaware of
  frameworks. The layer performs a second bounded Tree-sitter walk only for a
  detected Laravel worktree and resolves its symbolic facts against the normal
  PHP declaration catalog.
- Recognize only deterministic explicit forms: constructor type injection,
  container `bind`/`singleton`/`scoped`/`instance`, `app(Foo::class)` and
  `resolve(Foo::class)`, controller arrays and invokable route targets, job
  dispatch, `Event::listen`, job/command scheduling, explicit command arrays,
  and `Gate::policy`.
- Represent those semantics with typed graph edges: `BINDS`, `RESOLVES`,
  `ROUTES_TO`, `DISPATCHES`, `LISTENS_TO`, `SCHEDULES`, `REGISTERS`, and
  `AUTHORIZES_WITH`. Constructor injection uses the existing `DEPENDS_ON`
  edge. Top-level configuration expressions receive deterministic
  revision-local `Configuration` symbols so every edge has truthful endpoints.
- Label every framework symbol and relation with `heuristic` provenance and
  precision. Tree-sitter only supplies source structure/ranges; it does not
  upgrade a Laravel convention to syntax or provider precision.
- Expose these edges in an additive `related_relations` query section. Each
  item includes its direction relative to the requested/changed symbol because
  a generic relation section cannot imply direction as `callers` or `callees`
  does. Existing callers/tests sections keep their previous semantics.
- Cap framework symbols plus relations at 2,048 facts per file. Record detected
  mode, framework symbol/edge counts, truncated files, framework reparses, and
  relationship-contribution recomputations. Reconciliation reparses framework
  syntax only for changed PHP files and re-resolves only changed or
  declaration-dependent contributions before atomic workspace publication.
- Treat Laravel activation as index-lifecycle configuration. Editing
  `composer.json` requires restarting Chakra in this version; ordinary PHP
  source edits remain live and deterministically fresh.

## Alternatives considered

- Add Laravel cases to the generic PHP parser: rejected because it couples
  language syntax extraction to one framework and obscures provenance.
- Use regex/text matching: rejected because nested expressions, namespaces,
  imports, and source ranges would be fragile and computed forms could be
  mistaken for deterministic facts.
- Require Laravel reflection, a booted application, or a PHP language server:
  rejected because that executes project code or introduces an optional
  provider lifecycle into the deterministic baseline.
- Encode everything as `CALLS`, `DEPENDS_ON`, or a vague `RELATED_TO`: rejected
  because the semantics and direction would be misleading.
- Materialize dynamic macros, Eloquent magic, `__call`, reflection, runtime
  bindings, or computed class names: rejected because current source syntax
  cannot justify a bounded unique target.

## Consequences

- Production dependency added to `chakra-language-php`: workspace-managed
  `serde_json` (MIT/Apache-2.0), already used elsewhere in the workspace. It
  provides maintained, bounded Composer JSON parsing instead of an ad-hoc JSON
  recognizer.
- Laravel projects pay one additional bounded Tree-sitter pass per PHP file.
  Non-Laravel projects allocate no framework parser/index and publish no
  framework symbols or edges.
- Query consumers gain explicit high-value relations without losing ambiguity,
  direction, provenance, precision, revision, freshness, or response limits.
- Framework conventions not listed above remain absent rather than guessed.

## Validation / follow-up

- A realistic fixture covers container binding, constructor injection,
  service location, controller routes, invokable controllers, jobs, listeners,
  commands, schedules, and policies; a non-Laravel fixture proves the layer is
  inactive.
- Unit tests reject dynamic targets and prove the per-file fact cap.
- Live-update coverage edits one Laravel source, immediately queries fresh
  relations, and proves one framework file/contribution was recomputed.
- MCP end-to-end coverage verifies directed framework relations in `context`
  and `diff_context` with heuristic provenance/precision.
- Composer activation can become live in a later issue if source inventory and
  watcher configuration expand to treat metadata changes as an index-mode
  transition.
