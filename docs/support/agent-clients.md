# Agent-client compatibility matrix (issue #205, ADR-0055)

Verified against official documentation on 2026-09-13. Mechanism coverage is
hermetic: format, idempotence, preservation, conflict, removal, dry-run, and
worktree-resolution tests run in `chakra-cli`. Real-session evidence per
client (fresh session invokes Chakra tools without prompting) is a release
gate tracked in #195 and is **not yet recorded** — do not claim full client
support from this matrix alone.

| Client | Pinned docs generation | MCP config (project scope) | Format | Server entry | Instruction file |
| --- | --- | --- | --- | --- | --- |
| Codex CLI | docs 2026-09 (`~/.codex/config.toml`, `.codex/config.toml` project scope) | `.codex/config.toml` | TOML | `[mcp_servers.chakra]` with `command`, `args`, `enabled` | `AGENTS.md` |
| Claude Code | docs 2026-09 (project scope `.mcp.json`) | `.mcp.json` | JSON | `mcpServers.chakra` with `command`, `args` | `CLAUDE.md` |
| Cursor | docs 2026-09 (project `.cursor/mcp.json`) | `.cursor/mcp.json` | JSON | `mcpServers.chakra` with `command`, `args` | `AGENTS.md` |
| OpenCode | V2 docs 2026-09 (`mcp` top-level key) | `opencode.json` | JSON | `mcp.chakra` with `type = "local"`, `command = [...]`, `enabled` | `AGENTS.md` |

## Notes and limits

- Every registration is project scope:
  `chakra serve --repo <absolute worktree root>`, so a registration never
  redirects an unrelated project and machine paths stay out of portable
  shared configuration.
- Codex applies project configuration only for trusted projects; the user
  must trust the project in Codex for `.codex/config.toml` to take effect.
- OpenCode V2 does not resolve the `instructions` config array; `AGENTS.md`
  is the active instruction route. A legacy `mcpServers` OpenCode layout is
  rejected with an actionable message instead of a guessed migration.
- JSON configurations are re-emitted pretty-printed; JSONC comments in
  `opencode.json` are refused rather than silently dropped. Codex TOML is
  edited comment-preserving.
- The Chakra MCP server also sends workflow guidance in
  `initialize.instructions`; whether each client injects that field into
  model context is verified per client in the real-session evidence, and
  persistent project instructions remain the primary route.
- `chakra doctor --agent <client>` distinguishes executable availability,
  effective registration, instruction-block discovery, and project-config
  health; a successful MCP handshake alone is not evidence the model
  follows the workflow.
