# Public evaluation corpus (issue #25)

Chakra's cross-language evaluation runs against a pinned public corpus:
large, actively maintained, permissively licensed open-source repositories,
one or two complementary project shapes per language.

## Layout

- `manifest.json` — the selection authority: per language, the repositories
  with their pinned commit SHA, license, size, and selection rationale.
- `budgets.json` — per-language budgets (cold-index wall time, cold-index
  peak RSS, warm-noop wall time). Starting points, not SLOs.
- `results/` — committed machine-readable artifacts from real evaluation
  runs, one `<language>-<owner>__<repo>.json` per repository.
- `RESULTS.md` — human summary of the committed artifacts, including the
  machine and date that produced them.

## Fetching (opt-in, networked)

The corpus is fetched only by an explicit maintainer action; Rust code never
touches the network:

```sh
python3 tools/fetch_corpus.py --language rust   # one language
python3 tools/fetch_corpus.py                   # everything (large!)
python3 tools/fetch_corpus.py --list            # cache status only
```

Checkouts are shallow clones of the exact pinned SHA, cached under
`target/corpus/<owner>__<repo>/` (git-ignored, never redistributed).

## Running the evaluation

```sh
cargo run -p chakra-conformance -- corpus --language rust
cargo run -p chakra-conformance -- corpus --emit docs/support/corpus/results
```

For every repository of a supported language present in the local cache the
runner verifies the checkout HEAD against the pinned SHA (mismatches are
refused), then runs the scenario catalog: `cold-index`, `warm-noop`,
`fingerprint`, `one-file-edit`, `atomic-replace`, `rename-delete`,
`syntax-error`, `diff-context`, `queries`, `cancellation`, and
`cache-restore`. Missing checkouts and languages without a `chakra-language`
adapter are recorded as skipped repositories, not errors.

Edit scenarios mutate the cached checkout in place and always restore it
(`git checkout -- .` plus explicit removal of runner-created files; the
fetch tool's `.chakra-corpus.json` metadata is never touched). The final
`cache-restore` scenario proves the cache is back at the pinned SHA with a
clean worktree.

Peak RSS is reported from the indexer's phase-boundary samples
(`observed_phase_peak_rss_bytes`), which work on Linux (`/proc/self/status`)
and macOS (`ps`); on platforms without a sampler the artifact records
`"unavailable"`. These are phase-boundary samples, not an OS high-water
claim. Precise-provider phases are recorded as `not-configured`: provider
startup/failure/restart behavior is covered by the conformance suite with a
double, and corpus evaluation never requires a language server.

## Artifact schema (version 1)

Each result file contains: `schema_version`, `language`, `repository`,
`sha`, `status` (`evaluated`/`skipped` + `skip_reason`), `provider_phase`,
aggregate counts, and `scenarios` in catalog order. Every scenario carries
`status` (`pass`/`fail`/`skipped`), `details`, `phases` (named wall times),
`measurements` (scenario-specific, sorted keys — e.g. `symbols`, `edges`,
`peak_rss_bytes`, `wall_micros`), and `budget_verdicts`
(`budget`/`observed`/`limit`/`status`). The JSON structure is deterministic;
measured values vary by machine and run.

## Budget and baseline policy

- Results are **not** diffed in CI. CI runs
  `chakra-conformance corpus --verify`, which checks that committed artifacts
  parse, match the manifest (language/repository/SHA), and contain the full
  scenario catalog.
- Budgets in `budgets.json` are generous tripwires sized from a real local
  run (see `RESULTS.md` for machine/date). A budget failure identifies the
  language, repository SHA, scenario, budget, and observed value.
- Refreshing the corpus selection, budgets, or the committed result
  artifacts is a deliberate, reviewed commit — never a side effect of CI.

## Known findings

- `symfony/symfony` currently fails `cold-index`:
  `src/Symfony/Component/Cache/Traits/ValueWrapper.php` is genuinely
  ISO-8859-encoded (a Latin-1 `©`), and the source scan aborts the whole
  cold index on a non-UTF-8 file instead of degrading past it. This is a
  chakra-language robustness gap surfaced by the corpus, not a runner
  artifact; the failing result file is committed deliberately as the
  record. Follow-up belongs to the PHP parity work.
