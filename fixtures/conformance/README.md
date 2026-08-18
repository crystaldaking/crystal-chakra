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
| `dup_a`/`dup_b` (rust), `DupA`/`DupB` (php): same function name in two scopes | `ambiguity` |
| `nested.rs` (`outer::inner`), `Nested.php` (`Conf\Nested\Inner`) | `declarations-containers` |
| `use ... as audit_event`, `use ... as FormatHelperAlias` | `imports-aliases` |
| `src/` vs `tests/` trees | `source-roles`, `test-hints` |
| `dispatch_conformance_request` → `shared_unique_target` | `syntax-callers`, `provider-*` |
| `#[test] fn` / `test*` method | `test-hints` |
| `CONFORMANCE_TEXT_NEEDLE` comment in the service file | `text-search` |
| `fan_in.rs` / `FanIn.php`: 55 callers of one function | `high-degree-callers` |
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
