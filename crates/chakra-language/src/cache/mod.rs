//! Versioned per-file syntax fact cache (issue #39).
//!
//! The cache persists only materialization-independent per-file facts —
//! declarations, call candidates, relation drafts, diagnostics, content
//! hashes — in a compact binary encoding (budget B4). A restore validates
//! the SPEC §14 compatibility key, checks every file's content hash against
//! the current worktree, rebuilds each language partition from the facts
//! through the same bounded pipeline a cold build uses, and publishes the
//! result through the same atomic revision path. Restored revisions are
//! equivalent to deterministic rebuilds; provenance tiers are recomputed
//! during materialization, never stored as upgraded truth.
//!
//! Every failure mode falls back safely: a missing, corrupt, oversized, or
//! version-incompatible manifest means a deterministic rebuild; a corrupt,
//! truncated, or stale fact file means a reparse of exactly that file.
//! Precise live-provider facts are never stored. The cache is opt-in,
//! bounded, and disabled below the B1 size gate
//! ([`DEFAULT_MIN_INDEXED_FILES`]).

mod codec;
mod facts;
mod store;

pub(crate) mod convert;

pub use codec::{GRAPH_MODEL_VERSION, INDEX_FORMAT_VERSION};
pub use facts::{
    CallFact, FileSyntaxFacts, ImplFact, NamedRelationFact, SymbolFact, TypeRelationFact,
    TypeRelationKindFact,
};
pub use store::{
    CacheError, CacheStore, CacheWriteOutcome, CompatibilityKey, DEFAULT_MAX_ENTRIES,
    DEFAULT_MAX_ENTRY_BYTES, DEFAULT_MAX_TOTAL_BYTES, DEFAULT_MIN_INDEXED_FILES, FactsToStore,
    ManifestEntry, SyntaxCacheConfig, SyntaxCacheMode, content_hash,
};

/// How the syntax cache participated in one indexing run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheRestoreOutcome {
    /// Cache not configured.
    Disabled,
    /// Below the B1 size gate: the deterministic rebuild is the default
    /// path and the cache is neither read nor written.
    BelowGate { indexed_files: u64, gate: u64 },
    /// The index was restored from cached facts; `misses` files were
    /// reparsed because their facts were missing, stale, or corrupt.
    Restored { hits: u64, misses: u64 },
    /// The cache could not be used; the index is a deterministic rebuild.
    Fallback { reason: String },
}

/// Cache participation report of one indexing run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxCacheReport {
    pub restore: CacheRestoreOutcome,
    /// Publication after the build (`None` when nothing needed writing:
    /// disabled cache, below-gate repository, or a 100%-hit restore).
    pub write: Option<CacheWriteOutcome>,
    /// Bytes read from the cache during restore (manifest + payloads).
    pub bytes_read: u64,
}

impl Default for SyntaxCacheReport {
    fn default() -> Self {
        Self {
            restore: CacheRestoreOutcome::Disabled,
            write: None,
            bytes_read: 0,
        }
    }
}
