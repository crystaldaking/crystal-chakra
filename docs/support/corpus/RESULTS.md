# Public corpus evaluation results (issue #25)

Produced by `cargo run -p chakra-conformance -- corpus --emit docs/support/corpus/results` on macos/aarch64 (10 logical CPUs), 2026-08-20.

Measured values vary by machine and run; these artifacts are committed deliberately and are **not** diffed in CI. CI runs `chakra-conformance corpus --verify`, which checks artifact structure and manifest consistency only. Budgets live in `budgets.json`; refreshing budgets or baselines requires review.

| Language | Repository | SHA | Status | Cold index (s) | Peak RSS (MiB) | Symbols | Edges | Warm no-op (ms) | Scenarios failed |
|---|---|---|---|---|---|---|---|---|---|
| cpp | nlohmann/json | `cdf52ae9bef7` | pass | 0.42 | 102 | 11451 | 11880 | 21 | 0 |
| cpp | protocolbuffers/protobuf | `720e5468cebb` | pass | 3.15 | 891 | 138697 | 154549 | 79 | 0 |
| csharp | dotnet/runtime | `663c457b86cc` | pass | 82.18 | 2353 | 325078 | 506840 | 801 | 0 |
| java | apache/kafka | `aa502fb153d9` | pass | 5.94 | 1996 | 216644 | 241836 | 156 | 0 |
| java | spring-projects/spring-boot | `e3d4b1ceb6d8` | pass | 3.49 | 956 | 161758 | 102723 | 452 | 0 |
| javascript | react/react | `eb8feb71096e` | pass | 2.07 | 522 | 82629 | 66023 | 105 | 0 |
| php | laravel/framework | `faf45dd2b154` | pass | 1.83 | 549 | 56067 | 118717 | 59 | 0 |
| php | symfony/symfony | `add4ddb9867b` | pass | 5.22 | 1230 | 121822 | 245502 | 212 | 0 |
| python | apache/airflow | `f8b8461e8191` | pass | 5.14 | 1626 | 278038 | 248193 | 239 | 0 |
| python | django/django | `d92b02090140` | pass | 2.10 | 651 | 117181 | 139240 | 129 | 0 |
| rust | BurntSushi/ripgrep | `3fce3b5bb023` | pass | 0.18 | 211 | 5195 | 8576 | 63 | 0 |
| rust | tokio-rs/tokio | `625954f36572` | pass | 0.49 | 124 | 16888 | 19117 | 90 | 0 |
| shell | nvm-sh/nvm | `6798d1dbc99e` | pass | 0.04 | 58 | 174 | 1060 | 12 | 0 |
| shell | ohmyzsh/ohmyzsh | `97e11051e2f8` | pass | 0.12 | 39 | 3895 | 4236 | 23 | 0 |
| typescript | microsoft/vscode | `4d9c292ee3e2` | pass | 7.65 | 2630 | 499484 | 315023 | 321 | 0 |
