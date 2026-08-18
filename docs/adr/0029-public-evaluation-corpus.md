# ADR-0029: Pinned public evaluation corpus and budgeted runner

Status: accepted
Date: 2026-08-18

## Context

Issue #25 requires mandatory product evidence for every supported language
from real, large, active public repositories, while keeping ordinary CI
deterministic and independent of GitHub availability. The parity contract
(ADR-0026) makes `CORPUS-01` a mandatory capability, so the evaluation must
produce machine-readable artifacts the support matrix can consume.

## Decision

**Selection.** `docs/support/corpus/manifest.json` is the selection authority:
per language, one or two complementary repositories (library/tool plus
application/framework/monorepo shapes), each pinned to an immutable commit
SHA with license, size, and rationale recorded. Selection used size, recent
maintenance activity, primary-language share, and project shape — not stars
alone. Sources are never redistributed as fixtures; all licenses permit local
evaluation use.

**Fetching.** `tools/fetch_corpus.py` (stdlib-only, opt-in) performs shallow
checkouts of exact pinned SHAs into `target/corpus/` and records per-checkout
source file/line counts. Rust code never touches the network; a missing cache
entry is a skip, not a fetch. Re-fetching is a no-op when the cache matches
the pin.

**Evaluation.** `chakra-conformance corpus` (same tooling crate as the
conformance harness, ADR-0028) runs the scenario/budget schema per repository
after verifying the checkout HEAD against the pinned SHA: cold index (wall
time, peak RSS, coverage counts), warm no-op, deterministic fingerprint,
one-file edit, editor-style atomic replace, rename/delete, temporary syntax
error, clean and changed `diff_context`, high-degree bounded queries,
cooperative cancellation, and cache restoration. Edit scenarios mutate the
cached checkout and always restore the pinned SHA afterwards.

**Budgets.** `docs/support/corpus/budgets.json` holds per-language starting
budgets sized at roughly 25–100× observed values — tripwires for
order-of-magnitude regressions, not SLOs. Budget or baseline changes require
review and deliberate commits.

**Artifacts and CI.** Results are committed under
`docs/support/corpus/results/` with a human summary in `RESULTS.md` naming
the producing machine and date. Because measured values vary by machine, CI
does not diff results; it runs `chakra-conformance corpus --verify`, a
non-networked structural check of artifacts against the manifest. Budgets are
enforced at evaluation time by the runner (exit code = failed scenarios).

## Alternatives considered

- **CI-fetched corpus on every PR** — rejected: the issue requires default PR
  tests to be network-independent; the corpus is opt-in/scheduled evidence.
- **Full-history clones** — rejected: several selections exceed 1 GB with
  history; shallow pinned-SHA checkouts are sufficient for index evaluation.
- **Tight performance budgets from day one** — rejected: budgets start as
  order-of-magnitude tripwires and tighten through reviewed refreshes.
- **Redistributing corpus slices as fixtures** — rejected: license and
  attribution burden; the manifest points at upstream pins instead.

## Consequences

- First real-corpus finding already recorded: `symfony/symfony@add4ddb9`
  contains a Latin-1-encoded file, and the workspace source scan aborts the
  entire cold index on a non-UTF-8 read instead of degrading past the file.
  The artifact records `cold-index: fail`; the fix (skip-with-diagnostic) is
  tracked under #32 PHP parity.
- New language issues (#27–#36) inherit corpus evaluation: once a language
  has an adapter, its manifest entries are evaluated and `CORPUS-01` can flip
  to pass.
- Cancellation is asserted with pre-cancelled tokens (deterministic, no
  sleeps); mid-flight cancellation coverage remains a follow-up.
- RSS is sampled at index phase boundaries (via `/proc` on Linux, `ps` on
  macOS); per-query RSS attribution is out of scope.

## Validation / follow-up

- Real evaluation executed 2026-08-18 on macOS/aarch64 against tokio,
  ripgrep, laravel/framework, and symfony/symfony; artifacts committed.
- `chakra-conformance corpus --verify` passes and runs in CI.
- Follow-ups: non-UTF-8 source degradation (#32), mid-flight cancellation
  coverage, corpus refresh cadence per the contract §7 review policy.
