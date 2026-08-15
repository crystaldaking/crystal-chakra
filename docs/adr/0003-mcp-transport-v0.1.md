# ADR-003: MCP transport and SDK for v0.1

Status: accepted
Date: 2026-08-15

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
  agents (Codex CLI/Desktop confirmed to support stdio servers via
  `[mcp_servers.*]` config) spawn it as a child process.
- Tools are declared with the `tool`/`tool_router`/`tool_handler` macros
  inside `chakra-mcp`; tool inputs/outputs are the domain contract types
  (serde + JSON Schema), so no MCP protocol types enter `chakra-domain` or
  `chakra-engine`.
- The first real syntax tools are `repo_map`, `search`, and `symbol_search`
  alongside `status`. They only delegate to `QueryService`; indexing and
  Tree-sitter types remain outside the transport crate.
- The adapter holds `Arc<dyn QueryService>`; it is tested against a stub
  service, proving the boundary does not depend on the engine.
- Potentially repository-wide synchronous queries run on Tokio's blocking
  pool behind a two-permit semaphore. MCP runtime workers stay responsive,
  and concurrent CPU work is bounded without leaking async types into the
  query contract.
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
- Protocol upgrades (e.g. MCP 2026-07-28 discovery lifecycle) arrive by
  upgrading `rmcp`, not by rewriting protocol code.
- Adding the HTTP transport later is additive in `chakra-mcp`; the query
  layer is untouched.

## Validation / follow-up

- `crates/chakra-mcp/tests/contract.rs`: in-process clients over a duplex
  transport verify server identity, tool listing, a structured `status`
  call against a domain-only stub, and `repo_map` / `search` /
  `symbol_search` against a real indexed Rust fixture.
- Manual smoke: piped `initialize`/`tools/list` frames into
  `chakra serve --repo .` and received correct responses (2026-08-15).
- External Codex CLI/Desktop connectivity with the real indexed tools remains
  a product-level v0.1 evaluation step; the in-process real-index client
  covers the protocol contract in CI.
