# Crystal Chakra — Implementation Pack

This pack is intended to be copied into the root of a new `crystal-chakra` repository before implementation starts.

## Contents

```text
AGENTS.md
.agents/skills/
  chakra-self-review/SKILL.md
  chakra-architecture-review/SKILL.md
  chakra-validate/SKILL.md
  chakra-commit/SKILL.md

docs/
  SPEC.md
  IMPLEMENTATION_WORKFLOW.md
  roadmap/v0.1.md
  adr/README.md
  evaluation/v0.1-template.md
```

## Recommended start

1. Create/open an empty directory named `crystal-chakra`.
2. Copy this pack's contents into its root.
3. Give the coding agent a focused repository-bootstrap task grounded in `AGENTS.md` and the relevant project documentation.
4. Continue one implementation phase at a time, supplying each task independently.

`AGENTS.md` requires the model to use the repo-scoped review/validation/commit skills before every Git commit. This is intentionally agent-driven and does not rely on Git hooks.

## Document hierarchy

- `docs/SPEC.md`: long-term architecture.
- `docs/roadmap/v0.1.md`: what must actually be built now.
- `AGENTS.md`: permanent operational rules for agents.
- `.agents/skills`: reusable workflows that the agent should invoke autonomously.
