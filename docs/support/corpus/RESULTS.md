# Public corpus evaluation results (issue #25)

Produced by `cargo run --release -p chakra-conformance -- corpus --emit docs/support/corpus/results` on macos/aarch64 (10 logical CPUs), 2026-08-21.

Measured values vary by machine and run; these artifacts are committed deliberately and are **not** diffed in CI. CI runs `chakra-conformance corpus --verify`, which checks artifact structure and manifest consistency only. Budgets live in `budgets.json`; refreshing budgets or baselines requires review.

| Language | Repository | SHA | Status | Cold index (s) | Peak RSS (MiB) | Symbols | Edges | Warm no-op (ms) | Scenarios failed |
|---|---|---|---|---|---|---|---|---|---|
| cpp | nlohmann/json | `cdf52ae9bef7` | pass | 0.51 | 99 | 11448 | 11877 | 58 | 0 |
| cpp | protocolbuffers/protobuf | `720e5468cebb` | pass | 3.60 | 921 | 138382 | 154239 | 85 | 0 |
| csharp | dotnet/runtime | `663c457b86cc` | pass | 18.21 | 1384 | 283524 | 364710 | 937 | 0 |
| go | kubernetes/kubernetes | `c44d2a82ef7f` | pass | 10.13 | 1951 | 310387 | 379660 | 518 | 0 |
| go | prometheus/prometheus | `98c983239715` | pass | 0.84 | 242 | 32172 | 40425 | 38 | 0 |
| hcl | terraform-aws-modules/terraform-aws-eks | `48a429f63cf9` | pass | 0.15 | 53 | 7488 | 8308 | 14 | 0 |
| hcl | terraform-aws-modules/terraform-aws-vpc | `0a36bd54069c` | pass | 0.15 | 46 | 7043 | 8254 | 16 | 0 |
| java | apache/kafka | `aa502fb153d9` | pass | 8.39 | 1562 | 216728 | 241541 | 236 | 0 |
| java | spring-projects/spring-boot | `e3d4b1ceb6d8` | pass | 4.00 | 957 | 161763 | 102692 | 597 | 0 |
| javascript | react/react | `eb8feb71096e` | pass | 2.61 | 523 | 82648 | 66026 | 121 | 0 |
| php | laravel/framework | `faf45dd2b154` | pass | 2.63 | 516 | 56729 | 118988 | 63 | 0 |
| php | symfony/symfony | `add4ddb9867b` | pass | 8.12 | 1174 | 122685 | 245871 | 237 | 0 |
| python | apache/airflow | `f8b8461e8191` | pass | 7.99 | 1612 | 282953 | 253255 | 364 | 0 |
| python | django/django | `d92b02090140` | pass | 3.71 | 663 | 122097 | 140489 | 136 | 0 |
| rust | BurntSushi/ripgrep | `3fce3b5bb023` | pass | 0.21 | 66 | 5212 | 8596 | 62 | 0 |
| rust | tokio-rs/tokio | `625954f36572` | pass | 0.51 | 118 | 16888 | 19117 | 105 | 0 |
| shell | nvm-sh/nvm | `6798d1dbc99e` | pass | 0.06 | 14 | 174 | 1060 | 12 | 0 |
| shell | ohmyzsh/ohmyzsh | `97e11051e2f8` | pass | 0.14 | 35 | 3895 | 4236 | 26 | 0 |
| typescript | microsoft/vscode | `4d9c292ee3e2` | pass | 11.98 | 2120 | 490374 | 310163 | 387 | 0 |
