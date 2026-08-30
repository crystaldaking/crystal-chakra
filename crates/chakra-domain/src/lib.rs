//! Core domain types and query contracts for Chakra.
//!
//! This crate is the bottom of the dependency graph: it must not depend on
//! MCP protocol types, LSP types, storage engines, or any other adapter.
//! `serde`/`schemars` derives exist because the query envelope is defined as
//! versioned JSON (SPEC §28); they carry no transport semantics.

pub mod diagnostic;
pub mod envelope;
pub mod identity;
pub mod indexing;
pub mod location;
pub mod operation;
pub mod provenance;
pub mod query;
pub mod revision;
pub mod scheduling;
pub mod source;
pub mod state;
pub mod symbol;
