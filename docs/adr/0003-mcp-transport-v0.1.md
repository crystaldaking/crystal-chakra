# ADR-003: MCP transport and SDK for v0.1

Status: accepted
Date: 2026-08-15
Last reviewed: 2026-08-18

## Context

SPEC §30: MCP is a thin transport adapter over the query layer; prefer the
current maintained Rust MCP SDK over implementing the protocol manually;
v0.1 chooses the minimum transport needed for real agent validation.
Roadmap §13 requires verifying current Codex MCP transport support and
picking one path — not building both stdio and HTTP because the north star
mentions both.

## Decision

- SDK: `rmcp` 3.x (official Model Context Protocol Rust SDK), with default
  features off and only `server`, `macros`, `schemars`, `transport-io`
  enabled.
- Transport: stdio only. `chakra serve --repo <path>` runs the server;
  Codex CLI and the ChatGPT desktop app support local stdio servers and share
  host configuration. Registration can use
  `codex mcp add chakra -- <chakra command>` or `[mcp_servers.chakra]` config.
- Tools are declared with the `tool`/`tool_router`/`tool_handler` macros
  inside `chakra-mcp`; tool inputs/outputs are the domain contract types
  (serde + JSON Schema), so no MCP protocol types enter `chakra-domain` or
  `chakra-engine`.
- The final v0.1 surface is exactly `status`, `repo_map`, `search`,
  `symbol_search`, `context`, `callers`, and `diff_context`. They only
  delegate to `QueryService`; Git, indexing, Tree-sitter, and LSP types remain
  outside the transport crate.
- The adapter holds `Arc<dyn QueryService>`; it is tested against a stub
  service, proving the boundary does not depend on the engine.
- Potentially repository-wide synchronous queries run on Tokio's blocking
  pool behind a two-permit semaphore. MCP runtime workers stay responsive,
  and concurrent CPU work is bounded without leaking async types into the
  query contract.
- Cancellation while a request is waiting for a permit prevents dispatch.
  ADR-012 adds a domain-owned operation context and drop guard so cancellation
  after dispatch cooperatively unwinds freshness, graph, Git, and provider work
  while retaining the permit until cleanup completes. Queue and execution
  deadlines are distinct. Provider requests have their own deadlines and
  active LSP cancellation.
- Under ADR-024, serialize each typed query envelope once into rmcp's protocol
  `Value`, compute its exact compact JSON length by walking that value, and
  return a ready structured tool result. An oversized envelope is rejected
  without constructing a full encoded buffer. This 1 MiB total guard is in
  addition to per-section item and byte truncation; rmcp owns final transport
  encoding because its supported API has no pre-encoded structured-result
  path.
- Stdout is owned by the protocol stream; logging goes to stderr only.

## Alternatives considered

- Hand-rolled JSON-RPC over stdio: explicitly rejected by SPEC §30.
- Streamable HTTP daemon: needed later for multi-client/daemon scenarios,
  but it is a second transport to secure, test, and document; roadmap §13
  says pick one. Stdio is sufficient for Codex and every major local agent
  client.
- Child-process bridge to a long-running daemon: adds lifecycle and
  discovery complexity (SPEC §31) with no v0.1 consumer.

## Consequences

- `tokio` enters the workspace (rmcp runs on it). The CLI performs blocking
  Git discovery and CPU-heavy initial parsing through an owned
  `spawn_blocking` task before serving requests, keeping it off runtime
  worker paths.
- `serde` and `serde_json` are direct adapter dependencies because the MCP
  boundary itself enforces the serialized response budget.
- Protocol upgrades arrive by upgrading `rmcp`, not by rewriting protocol
  framing in Chakra.
- Adding the HTTP transport later is additive in `chakra-mcp`; the query
  layer is untouched.

## Validation / follow-up

- `crates/chakra-mcp/tests/contract.rs`: in-process clients over a duplex
  transport verify server identity, all seven tools, a structured `status`
  call against a domain-only stub, and every high-level query against a real
  indexed Rust and PHP fixtures.
- Unit regressions exhaust both blocking-query permits, prove a cancelled queued
  request is never dispatched, then cancel two already-running requests and
  prove a third starts without waiting for their original deadline.
- Unit regressions reject an envelope whose serialized representation exceeds
  the transport budget, compare exact size accounting with serde_json escaping,
  and prove the typed payload is serialized once at the budget boundary.
- All seven tools advertise read-only, non-destructive, idempotent, closed-world
  MCP annotations. A contract regression verifies these hints so
  non-interactive clients need not treat code-intelligence reads as writes.
- Manual smoke: piped `initialize`/`tools/list` frames into
  `chakra serve --repo .` and received correct responses (2026-08-15).
- The documented Codex CLI command/config shape was rechecked against the
  current CLI and official OpenAI MCP documentation on 2026-08-16.
- External Codex CLI 0.146.0 connectivity was executed on 2026-08-16 with
  ephemeral config overrides: a real agent completed `status` and
  `symbol_search` against the indexed Chakra repository under the default
  approval policy. The in-process real-index client continues to cover the
  deterministic protocol contract in CI.
