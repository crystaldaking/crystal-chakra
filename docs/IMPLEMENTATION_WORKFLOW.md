# Chakra Implementation Workflow

This file describes how to use the specification and repository instructions with an autonomous coding agent.

## Documents and authority

- `AGENTS.md` — mandatory repository behavior.
- `docs/SPEC.md` — architectural north star.
- `docs/roadmap/v0.1.md` — v0.1 scope authority.
- `.agents/skills/*` — reusable project workflows.

Do not paste the full SPEC into every task. Keep it in the repository and point the model to the relevant sections.

## Recommended sequence

1. Bootstrap the repository in an empty `crystal-chakra` directory.
2. Establish the v0.1 foundation.
3. Implement the syntax index.
4. Implement live revisions.
5. Add rust-analyzer enrichment.
6. Implement the agent-facing queries.
7. Complete hardening and evaluation.

The user supplies the task for each phase independently. A single long-running agent may continue through phases, but every phase must end at a clear reviewed Git checkpoint.

## Git checkpoints

Each phase should normally end in one or more cohesive commits.

The model must not commit automatically just because a file changed. A commit is appropriate when a coherent slice is implemented and validated.

Before every commit, `AGENTS.md` requires:

1. `$chakra-self-review`
2. `$chakra-architecture-review` when applicable
3. `$chakra-validate`
4. exact staged-patch review
5. `$chakra-commit`

No hooks are needed.

## Scope changes

If implementation uncovers a flaw in SPEC or v0.1 scope:

- do not silently diverge;
- explain the issue;
- update the relevant document and ADR if the change is architectural;
- run architecture review;
- commit the documentation/design change as part of or before the implementation that depends on it.

## Task completion

Every phase closeout should state:

- commits created;
- behavior implemented;
- tests/checks actually run;
- important architecture choices;
- deferred items;
- blockers, if any.
