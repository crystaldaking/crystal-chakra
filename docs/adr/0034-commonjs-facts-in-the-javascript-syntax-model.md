# ADR-0034: CommonJS facts in the JavaScript syntax model

Status: accepted
Date: 2026-08-20

## Context

JavaScript has two coexisting module systems. ES modules
(`import`/`export`) map onto the TypeScript adapter's extraction almost
unchanged, but real JavaScript codebases still make heavy use of CommonJS:
`const x = require("./m")` bindings and `module.exports`/`exports`
assignments. The TypeScript adapter deliberately skipped CommonJS —
TypeScript sources overwhelmingly use ES module syntax — but for first-class
JavaScript support (#29) skipping it would silently drop import facts,
import-alias call resolution, and heritage resolution for a large share of
real repositories (the pinned react/react corpus included). ADR-0027
selected the grammar (tree-sitter-javascript) and provider (vtsls, shared
with TypeScript over ADR-0032); it did not decide how CommonJS appears in
the syntax fact model.

## Decision

CommonJS is recorded with the same fact shapes ES modules already use; no
new symbol kinds, edge kinds, or provenance variants are introduced.

- A `require("...")` call with exactly one string argument, bound by a
  `const`/`let`/`var` declarator or used as a bare expression statement, is
  recorded as an `Import`-kind symbol fact named by its statement text —
  identical in shape, provenance (`tree_sitter`), and precision (`syntax`)
  to an ES `import` statement fact.
- `require` bindings feed the same alias maps as ES imports:
  `const foo = require("./m")` binds a namespace alias `foo`;
  `const { a, b: c } = require("./m")` binds named aliases
  (`a` → `m::a`, `c` → `m::b`), including shorthand and default-value
  patterns. Only relative specifiers resolve to module paths; a bare
  specifier (`require("react")`) records the fact but no alias, mirroring
  unresolvable ES package imports.
- Alias timing matches each module system's semantics: ES import aliases
  are hoisted (collected in a pre-pass before the main visit, as in the
  TypeScript adapter); `require` aliases apply in evaluation order
  (collected during the visit, so a binding is visible from its statement
  onward).
- `exports.name = <function>` and `module.exports.name = <function>`
  record a module-level function declaration fact; with any other value
  they record a constant fact. `module.exports = <expression>` records no
  symbol: the assigned identifiers already carry their own declarations,
  and inventing an extra entity would duplicate them.
- `require("...")` is never recorded as a call candidate — it is an import
  fact, and treating it as a call would flood every CommonJS file with an
  unresolvable `require` call site.

## Consequences

- Heritage edges, scoped call qualifiers, and constructor calls resolve
  through `require` aliases exactly as through ES import aliases
  (demonstrated by the adapter fixture: `class StripeProvider extends
  PaymentProvider` resolves through `const { PaymentProvider } =
  require("./provider.js")`).
- Known false-negative classes, reported rather than guessed: dynamic or
  non-literal requires (`require(expr)`, template specifiers),
  `Object.assign(module.exports, ...)`, export-name enumeration from
  `module.exports = { a, b }` object literals, and `require` calls nested
  in arbitrary expressions.
- JSX needs no separate decision: the single tree-sitter-javascript
  grammar parses JSX natively (verified by an adapter parse test), so
  `.jsx` sources use the same parser and extraction paths as `.js`.
- JavaScript precise facts carry `Provenance::Vtsls`; the provider is
  shared with TypeScript (ADR-0032), so no new provenance variant or
  provider crate exists.
