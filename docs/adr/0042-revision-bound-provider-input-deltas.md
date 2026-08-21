# ADR-0042: Revision-bound provider input deltas

Status: accepted
Date: 2026-08-21

## Context

Precise providers initially synchronized only captured source documents.
Project manifests, lockfiles, toolchain selectors, compilation databases, and
language-server configuration already affected live discovery and freshness,
but a metadata-only workspace revision could not notify an active provider.
Continuing to query that process risked attributing results based on its older
project model to the newer Chakra revision.

The contract must remain transport-neutral, atomic with the syntax graph,
language-scoped, and cheap for an unchanged revision. Metadata files are
freshness inputs, not text documents, so fabricating `didOpen` or `didChange`
would also be incorrect LSP behavior.

## Decision

- Add a transport-neutral `ProviderInput` to `chakra-engine`. It carries a
  repository-relative path, the registered languages affected by that input,
  and an opaque filesystem identity. No LSP type crosses the provider/query
  boundary.
- Publish the complete provider-input map in the same immutable
  `WorkspaceSnapshot` and compare-and-publish operation as the graph,
  lifecycle, and indexing metadata. A provider can therefore pin source and
  project inputs to exactly one workspace revision.
- Derive language ownership centrally from Git-visible ecosystem metadata:
  Cargo, Composer, npm/TypeScript, Python, Maven/Gradle, .NET, ShellCheck,
  C/C++ build metadata, Terraform, and Go module/workspace inputs. A shared
  file may affect more than one language.
- On Unix, use length, device, inode, mode, mtime, and ctime (including
  nanoseconds) as the strong identity, matching live source reconciliation.
  Platforms without an equivalent identity conservatively report a changed
  input on each new revision rather than risk hiding a change.
- Extend `ProviderWorkspaceDelta` with sorted, language-scoped
  created/changed/deleted input paths. Unchanged snapshots preserve shared map
  identity for the constant-time no-delta path.
- LSP adapters translate input deltas only to
  `workspace/didChangeWatchedFiles`. Source documents retain their existing
  `didOpen`/`didChange`/`didClose` rules; metadata inputs never masquerade as
  text documents.

## Alternatives considered

- **Restart every active provider on any metadata change.** Safe but needlessly
  expensive and loses provider caches; watched-file events are the standard
  incremental signal.
- **Fold metadata into the source document catalog.** Rejected because it
  invents language ids, text versions, and document lifecycle for files that
  are not parsed source.
- **Let each adapter rediscover metadata from disk.** Rejected because the
  result would not be pinned to the engine revision and would duplicate the
  Git-aware discovery policy.
- **Use only path or mtime as identity.** Rejected because same-path and
  restored-mtime writes can otherwise be missed.

## Consequences

- Metadata-only changes may publish a new revision without reparsing source;
  active providers still receive the exact language-scoped invalidation.
- The provider envelope retains a small map of Git-visible project inputs in
  addition to the source catalog. It stores identities, not file bodies.
- Providers may interpret watched-file events differently; Chakra's guarantee
  is that the revision-bound signal is delivered before a result can become
  ready, not that every upstream server reloads every project format.

## Validation / follow-up

- Engine tests prove a metadata-only delta is revision-exact and visible only
  to affected languages.
- Live-index tests edit `Cargo.toml`, publish a newer provider input without a
  source reparse, and observe one changed input.
- A hermetic csharp-ls lifecycle test changes `.csproj`, observes a watched
  event before readiness, and verifies that no text-document notification was
  fabricated. Every shared LSP adapter uses the same input-delta path.
