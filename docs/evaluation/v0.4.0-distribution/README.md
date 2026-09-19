# 0.4.0 distribution follow-up

On 2026-09-18 the actual optimized Linux x86-64 Chakra binary passed local
package and installer checks. [Recorded result](linux-container.json).
This is a Debian bookworm/amd64 Docker container on a macOS ARM64 host.
It is not a native x86-64 hardware run, an Ubuntu 22.04 compatibility check,
or final immutable release provenance.

The test verified the archive and both installer checksums, installed the
binary into an isolated HOME, resolved `chakra` in a fresh interactive Bash,
preserved existing shell configuration, repeated installation without a
second PATH block, and preserved the binary and shell configuration after
checksum and download failures. The installed `chakra update --check`
contacted GitHub and correctly reported local 0.4.0 as current compared with
the published 0.3.2. Automatic checks were disabled in the environment;
manual checks remain enabled by design.

The newer-release update branch, offline update behavior, automatic-check
behavior, Windows and macOS Intel execution, final native build matrix,
GitHub provenance generation/verification, and public asset downloads are
not covered by this run. Existing unit/fixture tests are separate evidence.
The source is an uncommitted candidate; rebuild after candidate freeze.

## Reproduction

Use the image and initialized named volumes from `tools/run_lsp_tests.sh`.
Run from the repository root; the script fixes the expected version at 0.4.0.
The image ID used by this run is recorded in the result. These commands build
and test locally and do not publish anything:

```sh
docker run --rm --init --platform linux/amd64 \
  -v "$PWD:/workspace:ro" \
  -v chakra-lsp-target:/workspace/target \
  -v chakra-lsp-cargo-registry:/usr/local/cargo/registry \
  -v chakra-lsp-cargo-git:/usr/local/cargo/git \
  chakra-lsp-test cargo build --locked --release --package chakra-cli --bin chakra

mkdir -p target/kmp-fix/linux-package
docker run --rm --init --platform linux/amd64 \
  -v "$PWD:/workspace:ro" \
  -v chakra-lsp-target:/workspace/target:ro \
  -v "$PWD/target/kmp-fix/linux-package:/evidence" \
  -v "$PWD/docs/evaluation/v0.4.0-distribution/linux-package-smoke.py:/smoke.py:ro" \
  chakra-lsp-test python3 /smoke.py
```

The [smoke script](linux-package-smoke.py) retains the archive, scripts,
manifest and step output under `target/kmp-fix/linux-package/`. Its temporary
HOME is removed on exit. Public update discovery depends on network access
and the currently published version; a newer published release will require
an explicit expected-outcome update, not a silent relaxation of assertions.

## Release workflow correction

The candidate workflow previously shipped and attested only the four native
archives. It now also includes `install.sh` and `install.ps1` in the shared
`SHA256SUMS`, attestation subjects and publication asset list. Installer smoke
jobs download the assembled bundle, verify it and execute its installer
scripts. README download commands point to release assets.

`python3 tools/test_release_bundle.py` passed four tests exercising the actual
workflow shell code: exact asset/checksum coverage, missing archive, unexpected
archive, and modified installer rejection. The preflight identity test passed
all ten branch/tag cases; `python3 tools/test_install.py` passed all twelve
Unix installer fixtures. Both workflow YAML files parse and their job graphs
have no missing dependencies or cycles. These checks do not execute GitHub
Actions, PowerShell or attestation services. No release was published.

## Controlled update-handler coverage

On 2026-09-18 two additional hermetic tests exercised the actual manual and
automatic update handlers against a local HTTP server. The manual handler
returned 2 for a newer release with the current platform's archive, 0 for
equal/older releases, and 1 for a newer release without that archive or an
HTTP 503. The automatic handler reported an available update, persisted the
attempt time with zero failures, and skipped the next check without another
request. This covers handler decisions with controlled release metadata;
it does not replace installed-executable/native-runner distribution checks.

`cargo test --locked -p chakra-cli` passed all 83 tests, including both new
cases. `cargo clippy --locked -p chakra-cli --all-targets -- -D warnings`,
`cargo fmt --all -- --check` and `git diff --check` passed. Local logs are in
`target/kmp-fix/update-contract/`. Production update code is unchanged; all
61 added lines are inside the existing `#[cfg(test)]` module. No new network
override, dependency, public API or publication was introduced.

## Committed candidate: native macOS ARM64, 2026-09-19

`cargo build --locked --release -p chakra-cli --bin chakra` succeeded at
`c2b843b2a2eb1cc1402af786e42cc27a6c2a5f52`. The recorded Rust source hashes
were unchanged throughout the build. The resulting `chakra 0.4.0` binary has
SHA256 `a7d673bc5069b3a7b68b976cf0eee68d2bbd62bff438ee7df785dcbac7b6586f`.

The actual archive passed clean installation into a temporary HOME, discovery
in a fresh interactive zsh, repeat installation with one managed PATH block,
and checksum/download failure preservation. Existing `.zshrc` content and a
shell canary survived; the installed binary remained byte-identical and no
temporary replacement binary remained. The test removed its temporary HOME.

Evidence is in `target/kmp-fix/final-candidate-macos/`: `identity.json`,
`result.json`, `build.log`, `package-smoke.py`, and `package/{result,steps}.json`.
This is a local native ARM64 package check using the repository installer;
it does not test Intel macOS, Windows, update discovery, a remotely assembled
release bundle, GitHub attestations or public release downloads.

The same binary passed the integrated four-client configuration sequence:
dry-run, idempotent setup, JSON diagnostics, sanitized report, overwrite
protection, conflicting-registration redaction, and removal preserving user
instructions. Logs are in `target/kmp-fix/final-candidate-macos/setup-report/`.
An initial harness assertion incorrectly required removal to restore the
pre-setup newline count; the existing contract preserves all bytes outside
markers, including setup separators. That assertion was corrected to match
the documented lifecycle test, and the complete sequence then passed.
Production code was unchanged. No agent-client sessions ran in this check.

## Committed candidate: Linux container, 2026-09-19

The current runtime candidate was rebuilt with `cargo build --locked --release
-p chakra-cli --bin chakra` in Linux/amd64 Docker. Build source hashes remained
unchanged. Its actual package passed checksum verification, clean installation,
fresh interactive Bash PATH discovery, repeat installation with one PATH
block, and binary/shell-configuration preservation on checksum and download
failure. Manual public update discovery also passed. See the [current Linux
result](linux-current-candidate.json) for binary/archive/installer hashes and
source identity; detailed logs are under its recorded evidence directory.

This supersedes the historical Linux run for current-code package acceptance.
It still uses amd64 emulation on macOS ARM64 and does not cover Windows,
Intel macOS, controlled newer-release discovery, or public release provenance.
