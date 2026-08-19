# Public corpus evaluation results (issue #25)

Produced by `cargo run -p chakra-conformance -- corpus --emit docs/support/corpus/results` on macos/aarch64 (10 logical CPUs), 2026-08-19.

Measured values vary by machine and run; these artifacts are committed deliberately and are **not** diffed in CI. CI runs `chakra-conformance corpus --verify`, which checks artifact structure and manifest consistency only. Budgets live in `budgets.json`; refreshing budgets or baselines requires review.

| Language | Repository | SHA | Status | Cold index (s) | Peak RSS (MiB) | Symbols | Edges | Warm no-op (ms) | Scenarios failed |
|---|---|---|---|---|---|---|---|---|---|
| php | laravel/framework | `faf45dd2b154` | pass | 1.83 | 549 | 56067 | 118717 | 59 | 0 |
| php | symfony/symfony | `add4ddb9867b` | pass | 5.22 | 1230 | 121822 | 245502 | 212 | 0 |
| rust | BurntSushi/ripgrep | `3fce3b5bb023` | pass | 0.18 | 211 | 5195 | 8576 | 63 | 0 |
| rust | tokio-rs/tokio | `625954f36572` | pass | 0.49 | 124 | 16888 | 19117 | 90 | 0 |
| typescript | microsoft/vscode | `4d9c292ee3e2` | pass | 7.65 | 2630 | 499484 | 315023 | 321 | 0 |
