# ADR-0055: Agent-client setup ownership, worktree resolution, and instruction blocks

Status: accepted
Date: 2026-09-13

## Context

Issue #205 (milestone v0.4.0): after a one-time project setup, each supported
coding-agent client — Codex, Claude Code, Cursor, OpenCode — must start
Chakra through local stdio MCP and carry concise guidance for using its
tools. Setup must be idempotent, must preserve user work, and must never
happen implicitly: `chakra serve` never edits agent configuration.

## Decision

### Command surface

- `chakra init --agent <codex|claude|cursor|opencode>` (repeatable) performs
  setup for exactly the named clients. `--repo PATH` (default `.`) selects
  the project; the registered worktree root is resolved through
  `chakra-git` (never a hand-rolled `.git` walk). `--dry-run` prints every
  planned write without touching disk; `--remove` reverts Chakra-owned
  setup. There is no "all detected clients" mode: selecting more than one
  client requires naming each one.
- `chakra doctor --agent <client>` diagnoses registration (extended by
  #207/#208 under the same command).

### Ownership model

Chakra owns exactly three artifacts per project, and nothing else:

1. **One MCP server entry named `chakra`** in the client project-scope
   configuration. An existing `chakra` entry with identical command/args is
   a no-op (idempotent), preserving optional fields such as environment,
   timeouts, and disabled state byte-for-byte. Remote registrations and
   entries with different command/args or malformed shapes are a *conflict*:
   setup stops with an actionable message and changes nothing unless the
   user first removes it or runs `--remove`.
2. **One delimited instruction block** bounded by
   `<!-- chakra:begin -->` / `<!-- chakra:end -->` in the client's
   instruction file. Content inside the markers is Chakra-managed and
   replaced wholesale on re-run; everything outside is byte-preserved. A
   malformed block (one marker, nested markers) is an error, never a
   rewrite.
3. **A minimal `chakra.toml`** created only when none exists, from the
   documented example (ADR-0053). Existing configuration is never
   overwritten.

### Client adapters and pinned formats (verified 2026-09-13)

| Client | MCP config (project scope) | Format | Instruction file |
| --- | --- | --- | --- |
| Codex | `.codex/config.toml` → `[mcp_servers.chakra]` with `command`, `args`, `enabled` | TOML | `AGENTS.md` |
| Claude Code | `.mcp.json` → `mcpServers.chakra` with `command`, `args` | JSON | `CLAUDE.md` |
| Cursor | `.cursor/mcp.json` → `mcpServers.chakra` | JSON | `AGENTS.md` |
| OpenCode | `opencode.json` → `mcp.chakra` with `type = "local"`, `command = [...]`, `enabled` | JSON(C) | `AGENTS.md` |

- Every registration is `chakra serve --repo <absolute worktree root>` with
  the resolved executable from `std::env::current_exe`. Project scope keeps
  machine paths out of portable shared configuration and prevents a global
  registration from silently redirecting an unrelated project.
- Codex TOML is edited with `toml_edit` so existing comments and formatting
  survive. JSON files are edited semantically and re-emitted pretty-printed
  (formatting normalizes; unrelated keys are preserved). An OpenCode
  configuration containing JSONC comments is refused with an actionable
  message instead of silently dropping them.
- A client configuration whose shape does not match the pinned format (for
  example an OpenCode `mcpServers` layout instead of `mcp`) is *unsupported*:
  setup stops and names the expected format rather than guessing.
- The MCP server already advertises workflow guidance through
  `initialize.instructions`; persistent project instructions remain the
  primary route because not every client injects that field into model
  context.

### Instruction template

One shared concise template teaches: verify the worktree and freshness with
`status`, prefer Chakra tools for discovery/symbols/relationships/changes,
respect provenance/precision labels, and fall back to ordinary search when
Chakra is unavailable. It never requires every tool on every task and never
overrides existing project policy outside its markers.

### Writes and removal

- All writes go through a temporary file + atomic rename; an interrupted
  setup leaves no partial file. A `--dry-run` run prints the exact planned
  contents.
- `--remove` deletes only the Chakra MCP entry and the delimited block,
  pruning files it created only when they become empty of other content;
  `chakra.toml` is left in place (it is project configuration, not
  Chakra-owned runtime state).
- A shared instruction block remains while any unremoved supported client
  using that instruction file still has a `chakra` registration, including
  a disabled one. The last registration's removal deletes the block.
  Unreadable sibling configurations or invalid TOML/JSON stop removal before writes
  because ownership cannot be established safely.

## Alternatives considered

- **User/global MCP registration.** Rejected as the default: a global entry
  cannot name the right worktree per project and would leak absolute paths
  into user-global shared configuration.
- **Invoking each client's CLI (`claude mcp add`, `codex mcp add`).**
  Rejected: it requires every client installed at setup time, gives no
  dry-run/remove ownership, and varies more across versions than the file
  formats themselves.
- **JSONC comment-preserving edits for OpenCode.** Deferred: no maintained
  Rust JSONC editing crate meets the bar; refusing commented files with a
  clear message is safer than silently dropping comments.

## Consequences

- `chakra-cli` gains setup/doctor modules and a `toml_edit` dependency
  (workspace-managed); no domain, query, engine, or provider crate changes.
- The compatibility matrix in `docs/support/agent-clients.md` records pinned
  client versions, formats, and evidence. Real-session evidence per client
  is a release-gate (#195) item tracked in #205; hermetic format tests are
  the pre-evidence gate.
- Adding a fifth client is one adapter + matrix row, not a redesign.

## Validation / follow-up

- Hermetic tests per adapter: initial write, idempotent re-run, preservation
  of unrelated content, conflicting registration, malformed blocks,
  removal, dry-run purity, unsupported shapes, nested-directory and
  linked-worktree resolution, spaces/non-ASCII paths.
- #207 extends `chakra doctor` with provider/analysis findings; #208 adds
  report export; this ADR's registration diagnostics stay with #205.
