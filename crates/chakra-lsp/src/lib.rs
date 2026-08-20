//! Minimal, generic, reusable LSP stdio client (ADR-0032).
//!
//! This crate is an adapter utility shared by Chakra precise-provider crates.
//! It owns one child process speaking JSON-RPC over stdio: bounded transport
//! framing, request/response routing with `$/cancelRequest`, the
//! initialize/initialized handshake with a bounded deadline, and a
//! shutdown/exit sequence with a process-group kill fallback that leaves no
//! orphan processes.
//!
//! Protocol types stay behind `serde_json::Value`; provider crates deserialize
//! into their own `lsp-types` structures. Nothing here depends on Chakra
//! domain or engine crates, and no LSP type escapes into them either
//! (invariants 5, 6, 10).
//!
//! The client is single-threaded by design: provider workers pump one
//! request at a time and interleave server notifications and server-to-client
//! requests through the event callback.

mod client;
mod transport;

pub use client::{Client, ClientConfig, ClientError, Health, RestartBackoff, ServerEvent};
pub use transport::{TransportConfig, TransportError};
