# ADR-009: Git-aware repository and worktree identity

Status: accepted
Date: 2026-08-16

## Context

SPEC §11 requires repository identity not to rely only on an absolute local
path. It must account for ordinary path moves, linked worktrees, repositories
without remotes, remote URL changes, and local-only repositories. The initial
implementation used the canonical worktree root as `RepositoryId`, so moving
a checkout changed both repository and workspace identity.

Repository identity belongs at the Git adapter boundary. The domain layer
must not discover Git administrative layout, and Chakra must never assume that
administration lives in `<worktree>/.git`.

## Decision

- Resolve production workspace identity through `chakra-git`. The adapter
  asks Git for the worktree root and root commit object ids and constructs a
  deterministic `git-roots:` repository key from the sorted unique roots.
  The key is independent of remote configuration and absolute checkout path.
- For an unborn repository, where no Git object can distinguish the
  repository, ask Git for the common administrative directory and use its
  filesystem object identity (device/inode on Unix and volume/file index on
  Windows). This distinguishes local-only unborn repositories and remains
  stable across an ordinary rename on the same filesystem.
- Keep `RepositoryId` construction typed in `chakra-domain`, but pass the
  adapter-established stable key into it. Domain code does not execute Git or
  inspect administrative paths.
- Derive `WorkspaceId` from `RepositoryId` plus the canonical materialized
  worktree root. Linked worktrees therefore share repository identity but have
  distinct workspace identities.
- Retain a clearly named standalone path fallback for isolated engine tests or
  non-Git embeddings. The production CLI does not use it.

## Alternatives considered

- Canonical worktree path only: rejected because a repository rename changes
  identity and linked worktrees collapse repository/workspace concepts.
- Remote URL: rejected because local-only repositories have no remote and a
  configured URL can change without changing the repository.
- Git common-directory path: rejected as the primary key because it remains
  path-dependent and differs between clones.
- Write a generated UUID into local Git config: rejected for v0.1 because
  identity discovery should not mutate repository configuration.

## Consequences

- Committed clones with the same root history share repository identity;
  distinct linked worktrees remain separately addressable.
- Adding or removing a genuinely unrelated root history under repository refs
  can change the root-set identity. This is an explicit, rare limitation of
  the v0.1 identity model and is preferable to mutable remote/path identity.
- Moving an unborn repository across filesystems can change its fallback
  identity. Once it has a commit, identity is object-derived.
- Git identity commands use the existing bounded subprocess implementation;
  no production dependency was added.

## Validation / follow-up

- Integration tests cover path moves, remote URL changes, linked worktrees,
  local-only committed repositories, deterministic unborn identity, and
  separation of distinct unborn repositories.
- Persistent snapshots, lineage, and cross-repository identity reconciliation
  remain outside v0.1.
