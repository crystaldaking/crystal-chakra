# Conformance fixtures

These fixtures back the cross-language conformance harness (`crates/chakra-conformance`,
GitHub issue #24) defined by `docs/language-parity-contract.md` (CONFORM-01).

## Authorship and license

Every file under `fixtures/conformance/` is hand-written by the Chakra project
for this purpose. No third-party code is included. All files are licensed under
the project's MIT license (`LICENSE` at the repository root).

## Layout

Each language has its own directory:

- `rust/` — a small Rust project (`src/` plus `tests/`).
- `php/` — a small PHP project (`src/` plus `tests/`).
- `typescript/` — a small TypeScript project (`src/` plus `tests/`, with
  `package.json` and `tsconfig.json` project metadata).
- `python/` — a small Python project (`src/` plus `tests/`, with
  `pyproject.toml` project metadata).
- `javascript/` — a small JavaScript project (`src/` plus `tests/`, with
  `package.json` and `jsconfig.json` project metadata, mixing ES modules
  and CommonJS).
- `java/` — a small Maven-layout Java project (`src/main/java` plus
  `src/test/java`, with a `pom.xml` project manifest).

Inside each language directory:

- `manifest.json` — the data-driven scenario manifest. It lists the shared
  scenario catalog (`scenarios`: id, description, capability ids from
  `docs/language-parity-contract.md`) and the per-language `expectations`
  (qualified names, paths, counts) the harness asserts. Adding a new language
  means adding a new directory with a fixture tree and a manifest; the harness
  discovers it without code changes.

The fixtures deliberately contain the hard cases the scenarios assert:

| Fixture element | Scenario |
|-----------------|----------|
| `dup_a`/`dup_b` (rust), `DupA`/`DupB` (php), `dup_a`/`dup_b` (typescript/python/javascript), `dup_a`/`dup_b` packages (java): same function name in two scopes | `ambiguity` |
| `nested.rs` (`outer::inner`), `Nested.php` (`Conf\Nested\Inner`), `nested.ts` (`outer::inner` namespaces), `nested.py` (`Outer::Inner` classes), `nested.js` (`outer`/`inner` nested functions), `nested/Outer.java` (`Outer`/`Inner` nested classes) | `declarations-containers` |
| `use ... as audit_event`, `use ... as FormatHelperAlias`, `import { ... as audit_event }`, `from ... import ... as audit_event`, `const { ...: audit_event } = require(...)`, `import static ... record_conformance_event` (java) | `imports-aliases` |
| `src/` vs `tests/` trees (java: `src/main/java` vs `src/test/java`) | `source-roles`, `test-hints` |
| `dispatch_conformance_request` → `shared_unique_target` | `syntax-callers`, `provider-*` |
| `#[test] fn` / `test*` method / `it("...")` block / `test_*` function / `@Test` method (java) | `test-hints` |
| `CONFORMANCE_TEXT_NEEDLE` comment in the service file | `text-search` |
| `fan_in.rs` / `FanIn.php` / `fan_in.ts` / `fan_in.py` / `fan_in.js` / `FanIn.java`: 55 callers of one function (typescript/python/javascript/java: 55 plus one import/require/static-import-aliased call) | `high-degree-callers` |
| service file (broken and repaired at runtime) | `syntax-error-recovery` |
| files created/renamed/deleted at runtime | `file-lifecycle`, `diff-context-scopes` |

The syntax-error, file-lifecycle, and diff scenarios mutate a temporary copy of
the fixture at runtime; the committed fixture itself is always valid code.

## Result files

The harness emits one result file per language to
`docs/support/conformance/<language>.json`. Schema (version 1):

```json
{
  "schema_version": 1,
  "language": "rust",
  "scenario_count": 14,
  "passed": 14,
  "failed": 0,
  "scenarios": [
    {
      "id": "ambiguity",
      "description": "...",
      "capability_ids": ["AMBIG-01", "PROV-01"],
      "status": "pass",
      "provenance_assertions": ["..."],
      "details": ""
    }
  ]
}
```

- `status` is `pass` or `fail`; `details` is empty on pass and carries the
  failure message on fail.
- `provenance_assertions` lists the provenance/precision checks the scenario
  actually performed (PROV-01 evidence).
- Emission is deterministic: fixed field order, manifest scenario order, no
  timestamps, so re-running `chakra-conformance emit` is byte-identical.
- Regenerate with `cargo run -p chakra-conformance -- emit docs/support/conformance`.
