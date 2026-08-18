# PHP precise-provider evaluation for v0.1.1

This record evaluates issue
[#2](https://github.com/crystaldaking/crystal-chakra/issues/2). Values are
local measurements, not cross-machine service-level guarantees. The raw
corpus and ground truth live in `fixtures/php/provider-evaluation`; the
syntax runner and provider-neutral LSP harness are committed with the result.

## Decision summary

Do not integrate a precise PHP provider in v0.1.1. Keep the current
Tree-sitter tier as the always-current source and consider PHPactor in a
separately scoped definition/reference-only proof of concept.

All three evaluated providers improve definition resolution, but none exposes
LSP incoming/outgoing call hierarchy. Intelephense has the best result on this
small corpus but its server licence does not permit redistribution and limits
intended use to an individual using an LSP-compatible IDE or editor. Psalm is
MIT and Composer-local, but advertises neither references nor call hierarchy
and failed the forced-crash restart probe. PHPactor is MIT, supports
definitions/references, and restarted cleanly, but still lacks call hierarchy
and missed one call that Chakra already resolves at the syntax tier.

Because no provider is integrated, no provider result can enter Chakra or be
labelled current in v0.1.1. A future adapter must bind every result to both the
workspace revision and provider document state, use a bounded synchronization
barrier, and otherwise return current syntax facts with catching-up/degraded
metadata.

## Corpus and scoring

The synthetic corpus models patterns observed during the non-sensitive
`psp-app` evaluation without copying application code:

- duplicate `handle` methods and an unknown receiver;
- promoted-property and local-`new` receiver types;
- flow through a typed factory return;
- a generic container `make(class-string<T>)` result;
- a PHPDoc-typed property;
- service-locator interface resolution;
- trait methods, jobs, routes, container bindings, and tests;
- a runtime method name that deliberately has no single static target.

Eight calls have one statically provable target and one duplicate-name call
has no target. The dynamic-name call is retained as a negative corpus pattern
but excluded from definition precision/recall because resolving the variable
itself is not the same as resolving a callable. A provider definition is
accepted only when its returned range contains the expected declaration name
in the expected file.

## Environment

- Date: 2026-08-17
- OS: macOS 26.6.1, arm64, Apple M4, 16 GiB
- PHP: 8.5.9
- Composer: 2.10.2
- Node.js: 26.5.1
- Rust: pinned repository toolchain, release profile for syntax measurements

Provider versions were current maintained releases available during the run:

- [PHPactor 2026.07.22.0](https://github.com/phpactor/phpactor/releases/tag/2026.07.22.0),
  PHAR SHA-256 `8c0155380b9d7559a12f35ddf8d09c1dc23e72f1797498038251fc35ad15574d`;
- [Intelephense 1.18.5](https://github.com/bmewburn/vscode-intelephense/releases/tag/v1.18.5),
  free features and no licence key;
- [Psalm 6.16.1](https://github.com/vimeo/psalm/releases/tag/6.16.1),
  installed Composer-locally outside the corpus.

## Syntax baseline

Command:

```sh
cargo run --locked --release -p chakra-language-php \
  --example evaluate_provider_corpus
```

| Measurement | Observed value |
| --- | ---: |
| Corpus PHP files | 17 |
| Initial syntax index | 24.185 ms |
| Symbols / edges / call sites | 78 / 77 / 14 |
| Syntax errors | 0 |
| Definition TP / FP / FN / TN | 5 / 0 / 3 / 1 |
| Precision / recall | 100% / 62.5% |
| Nine-case lookup batch, median / p95 (500 runs) | 21 / 21 µs |
| Serialized nine-case response | 1,982 bytes |
| Single-file reconciliation | 11.785 ms |
| Files reparsed | 1 of 17 |
| Generic / framework relationship owners recomputed | 1 / 2 |

The three syntax false negatives are the typed factory return, generic
container return, and PHPDoc property. The syntax tier resolves the promoted
property, service-locator class constant, job receiver, trait method, and test
local receiver. It does not invent an edge for the unknown duplicate
`handle`.

## Provider results

Before each provider run, the corpus was copied into a new temporary directory
and `composer dump-autoload --no-scripts --no-dev` generated local autoload
metadata. The LSP harness opened the resulting 26 PHP documents. Providers
were installed outside the corpus, and the default Cargo suite remains
independent of PHP, Composer, Node.js, and any provider installation.

| Measurement | PHPactor 2026.07.22.0 | Intelephense 1.18.5 free | Psalm 6.16.1 |
| --- | ---: | ---: | ---: |
| Initialize response | 96.2 ms | 188.6 ms | 808.1 ms |
| First successful definition | 1,798.2 ms | 61.9 ms | 304.7 ms |
| Later definition range | 0.7–2.3 ms | 0.2–11.8 ms | 0.3–0.5 ms |
| Definition TP / FP / FN | 7 / 0 / 1 | 8 / 0 / 0 | 8 / 0 / 0 |
| Definition precision / recall | 100% / 87.5% | 100% / 100% | 100% / 100% |
| Typical definition response | 198 bytes | 402 bytes | 197 bytes |
| References | 7 locations / 4 files | 11 locations / 4 files | unsupported |
| Reference request / response | 10.5 ms / 1,244 B | 3.4 ms / 1,971 B | method not found / 163 B |
| LSP call hierarchy | unsupported | unsupported | unsupported |
| Post-`didChange` definition | 2.0 ms, current | 1.6 ms, current | 17.2 ms, current |
| Advertised text sync | full | incremental | full |
| Main process RSS after probes | 106,832 KiB | 186,272 KiB | 27,360 KiB |
| Installed footprint in this run | 4.48 MB PHAR | 149 MiB `node_modules` | 59 MiB Composer `vendor` |
| Forced-crash detection | 3.9 ms | 8.8 ms | 1.3 ms |
| Restart initialize / definition | 112.0 / 30.3 ms | 198.2 / 20.5 ms | initialization exceeded 30 s |
| Graceful shutdown after restart | clean | clean | not reached |

RSS is the directly owned server process only; provider workers and shared
runtime pages can make it understate total memory. Installed footprint is the
filesystem allocation of the evaluated installation, not the download size.

For synchronization, the harness sent a full-content `didChange` replacing a
known method with a missing method, immediately requested its definition, and
then restored the document. No provider returned the old definition for the
changed name, and PHPactor/Intelephense/initial Psalm all restored the correct
definition. This proves the observed document flow, not a general
workspace-revision barrier.

For cancellation, the harness sent `$/cancelRequest` immediately after a
valid broad workspace-symbol request (PHPactor/Intelephense) or definition
request (Psalm). Each tiny-corpus request completed normally before a
cancelled response was observable. The connection remained usable, but these
measurements do not prove that long provider work is cooperatively cancelled.
PHPactor's generic language-server package explicitly documents cancellation
support; a Chakra adapter would still need its own bounded deadline and child
lifecycle.

## Capability and licence evidence

PHPactor's official [navigation documentation](https://phpactor.readthedocs.io/en/master/reference/navigation.html)
documents definitions and references. Its generic
[language-server package](https://github.com/phpactor/language-server)
documents STDIO, text synchronization, request cancellation, initialization,
and an MIT licence. The concrete initialize response advertised definitions,
references, workspace symbols, and full sync, but no call hierarchy. The
[standalone installation](https://phpactor.readthedocs.io/en/master/usage/standalone.html)
supports an on-demand PHAR and requires PHP 8.2 or newer.

Intelephense's official [installation and capability documentation](https://github.com/bmewburn/vscode-intelephense/wiki/Installation)
advertises definitions, references, incremental synchronization, and an
optional storage directory. The concrete server advertised those capabilities
but no call hierarchy. Its [server licence](https://github.com/bmewburn/vscode-intelephense/blob/master/LICENSE.txt)
is not MIT: it is personal, non-transferable, restricts intended use to an
individual paired with an LSP-compatible IDE/editor, prohibits redistribution,
and reserves named premium features. That is not a sufficient basis for a
Chakra-managed or bundled provider.

Psalm is MIT and its official [installation guide](https://psalm.dev/docs/running_psalm/installation/)
supports a project-local Composer dependency. Its official
[language-server documentation](https://psalm.dev/docs/running_psalm/language_server/)
lists diagnostics, go-to-definition, hover, and limited completion, and warns
that large framework projects may need initialization timeouts around 240
seconds. The concrete server matched those limits: it did not advertise
references, workspace symbols, document symbols, or call hierarchy.

## Reproduction

The provider harness uses Python's standard library and does not participate
in normal tests:

```sh
python3 tools/evaluate_php_lsp.py --help
```

For each provider, copy `fixtures/php/provider-evaluation` to a fresh temporary
Git-independent directory, run Composer's no-script autoload dump there, and
pass the provider command after `--`. Important provider-specific arguments:

- PHPactor: use `language-server`, a dedicated `XDG_CACHE_HOME`, and a
  pre-created `indexer.index_path` outside the source inventory;
- Intelephense: use `--stdio` and dedicated `storagePath` /
  `globalStoragePath` initialization options; no licence key was supplied;
- Psalm: use the corpus `psalm.xml`, `--root`, and the Composer-local
  `psalm-language-server` executable.

The harness has fixed initialize, request, and restart deadlines. It records
capabilities, definition cases, references, call hierarchy, synchronization,
cancellation, RSS, forced-crash detection, restart, and graceful shutdown as
JSON. It never writes provider facts into Chakra.
