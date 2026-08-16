//! MCP adapter for Chakra (SPEC §30, ADR-0003).
//!
//! Thin stdio transport over the domain [`chakra_domain::query::QueryService`] contract. MCP
//! protocol types stay inside this crate; domain and engine never see them.

mod server;

pub use server::{ChakraMcpServer, ServeError, serve_stdio};
