//! Shared language-neutral scaffolding for optional precise-provider
//! adapters (issue #94).
//!
//! The crate owns the mechanics every LSP-backed provider repeats: the owner
//! thread event loop, session lifecycle with bounded restart/backoff,
//! revision-scoped document synchronization (didOpen/didChange/didClose plus
//! watched-file input deltas), the post-synchronization request barrier,
//! observability publication, cancellation, and cooperative shutdown.
//!
//! Language-specific semantics stay behind typed hooks ([`ProviderHooks`]):
//! the provider's name and provenance, the synchronized language set, LSP
//! language ids, capability verification, and the query strategy. The stock
//! [`CallHierarchyDriver`] covers providers whose precise operations are the
//! LSP call-hierarchy trio; providers with a different precise surface
//! implement the hook directly. No hook erases language-specific readiness or
//! result semantics, and no LSP type crosses the crate's public boundary into
//! domain or query layers (invariants 5, 6, 10).

pub mod convert;
mod error;
mod hooks;
mod provider;
mod state;
mod worker;

pub use error::WorkerError;
pub use hooks::{CallHierarchyDriver, ProviderHooks, QueryChannel, QueryDeadlines, QueryOutcome};
pub use provider::{
    ProviderCommandSpec, ProviderHandle, StartError, WorkerConfig, WorkerShutdownError,
};
pub use state::{ProviderCommand, SharedState};
