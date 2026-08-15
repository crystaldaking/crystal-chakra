//! Git-aware Rust syntax indexing adapter.
//!
//! File discovery delegates repository semantics to Git, while syntax facts
//! come from the official Tree-sitter Rust grammar. The crate points inward:
//! it builds Chakra domain/engine types, and neither core crate depends on a
//! language-specific parser.

mod discovery;
mod indexer;
mod live;
mod parser;

pub use discovery::{DiscoveryError, discover_rust_files, resolve_repository_root};
pub use indexer::{IndexMetrics, IndexReport, RustIndexError, RustSyntaxIndex, index_repository};
pub use live::{LiveIndexError, LiveIndexMetrics, LiveRustIndex, start_live_rust_index};
