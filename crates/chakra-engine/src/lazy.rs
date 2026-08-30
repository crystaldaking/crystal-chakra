//! Lazy revision-scoped file facts (issue #42; SPEC §3 enrichment layer).
//!
//! Some per-file facts are too expensive or too rarely useful to index
//! eagerly into every published revision. A typed [`LazyFactProducer`]
//! declares its inputs, format version, cost budget, and provenance; a
//! per-producer [`LazyFactStore`] computes facts on demand and retains them
//! keyed by workspace, path, compatible file content, and the pinned
//! workspace revision.
//!
//! This is a `WorkspaceEnrichment`-style overlay, not commit-snapshot state:
//! facts never enter the published graph, and the store never mutates or
//! gates revision publication. Every returned fact carries the producer's
//! explicit provenance and precision so callers cannot mistake a heuristic
//! digest for a precise-provider result.
//!
//! The store is intentionally *not* a general-purpose cache: each instance
//! is bound to one concrete producer type, so keys, values, budgets, and
//! eviction are all typed. Revision or content changes invalidate entries
//! naturally through the key; stale entries are never served and are evicted
//! by the count/byte bounds.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use chakra_domain::identity::WorkspaceId;
use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::revision::Revision;
use thiserror::Error;

use crate::engine::WorkspaceSnapshot;
use crate::graph::SymbolGraph;

/// Poll cap for coalesced waits: a waiter re-checks its own cancellation and
/// deadline at least this often instead of sleeping unboundedly.
const WAIT_POLL_CAP: Duration = Duration::from_millis(5);

/// Maximum rendered outline lines per file digest. A file exceeding the cap
/// reports `truncated` rather than growing the retained fact without bound.
const MAX_OUTLINE_LINES: usize = 2_048;

/// Cache-only identity of one exact file content (FNV-1a, 64-bit).
///
/// Deterministic and process-local: it keys an in-memory cache and is never
/// persisted or compared across processes, so a cryptographic hash is not
/// warranted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentHash(u64);

impl ContentHash {
    pub fn of(source: &str) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in source.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self(hash)
    }
}

/// Versioned identity of one producer implementation.
///
/// `format_version` must bump whenever the fact format or semantics change in
/// a way that makes earlier facts incompatible. The identity is fixed per
/// store instance, so a version bump silently reading stale entries is
/// impossible: a new producer gets its own store and its own key space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProducerIdentity {
    pub id: &'static str,
    pub format_version: u32,
}

/// Declared cost/budget contract of one producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactBudget {
    /// Wall-time bound enforced on top of the caller's own deadline; the
    /// effective compute deadline is the earlier of the two.
    pub max_wall_time: Duration,
    /// Facts larger than this are computed and returned but never retained,
    /// so one huge file cannot displace the whole store.
    pub max_fact_bytes: usize,
}

/// Retention bounds of one typed store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactStoreBounds {
    pub max_entries: usize,
    pub max_total_bytes: usize,
}

impl Default for FactStoreBounds {
    fn default() -> Self {
        Self {
            max_entries: 1_024,
            max_total_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Declared inputs of a lazy fact: the exact file content, the pinned
/// workspace/config revision, and read access to that revision's graph.
///
/// The store is bound to `workspace`; the producer declares its typed
/// invalidation key from `path`, `source`, and `revision`. `graph` gives the
/// producer read-only working context from that pinned revision.
#[derive(Debug)]
pub struct FileFactInput<'a> {
    workspace: &'a WorkspaceId,
    path: &'a RepoRelativePath,
    revision: Revision,
    source: &'a str,
    graph: &'a SymbolGraph,
}

impl<'a> FileFactInput<'a> {
    /// Pins all input components to one immutable published snapshot. A
    /// caller cannot mix a revision label, graph, and source from different
    /// publications through the public API.
    pub fn from_snapshot(
        snapshot: &'a WorkspaceSnapshot,
        path: &'a RepoRelativePath,
    ) -> Result<Self, LazyFactError> {
        let source =
            snapshot
                .graph()
                .file_source(path)
                .ok_or_else(|| LazyFactError::FileNotInSnapshot {
                    path: path.clone(),
                    revision: snapshot.revision(),
                })?;
        Ok(Self {
            workspace: &snapshot.identity().workspace,
            path,
            revision: snapshot.revision(),
            source,
            graph: snapshot.graph(),
        })
    }

    pub fn workspace(&self) -> &WorkspaceId {
        self.workspace
    }

    pub fn path(&self) -> &RepoRelativePath {
        self.path
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn source(&self) -> &str {
        self.source
    }

    pub fn graph(&self) -> &SymbolGraph {
        self.graph
    }
}

/// Producer-declared invalidation key for one exact file fact.
///
/// The path is required even when two files have identical bytes because a
/// producer may derive path- or graph-dependent facts. The store adds its
/// bound workspace identity and producer identity around this key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileFactInvalidation {
    pub path: RepoRelativePath,
    pub revision: Revision,
    pub content: ContentHash,
}

impl FileFactInvalidation {
    pub fn exact(input: &FileFactInput<'_>) -> Self {
        Self {
            path: input.path.clone(),
            revision: input.revision,
            content: ContentHash::of(input.source),
        }
    }
}

/// A typed lazy fact with an honest retained-size estimate for byte bounds.
pub trait LazyFact: Send + Sync {
    /// Estimated retained bytes, including string/vector payloads.
    fn retained_bytes(&self) -> usize;
}

/// Why a lazy fact could not be produced.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LazyFactError {
    #[error(transparent)]
    Aborted(#[from] OperationAbort),
    #[error("lazy fact producer {producer} v{format_version} failed: {message}")]
    Producer {
        producer: &'static str,
        format_version: u32,
        message: String,
    },
    #[error("lazy fact store reached its {max_entries}-entry in-flight/cache bound")]
    StoreSaturated { max_entries: usize },
    #[error("lazy fact input workspace {input} does not match store workspace {store}")]
    WorkspaceMismatch {
        store: WorkspaceId,
        input: WorkspaceId,
    },
    #[error("file {path} is not retained in published revision {revision}")]
    FileNotInSnapshot {
        path: RepoRelativePath,
        revision: Revision,
    },
}

/// A typed producer of one kind of lazy per-file fact.
///
/// Implementations must be deterministic over their declared inputs and must
/// honor `operation` cooperatively: a cancelled or over-budget computation
/// aborts instead of publishing a partial result. Producers never claim more
/// precision than their derivation supports.
pub trait LazyFactProducer: std::fmt::Debug + Send + Sync {
    /// The typed fact this producer computes.
    type Fact: LazyFact;

    /// Versioned producer identity (invalidation key component).
    fn identity(&self) -> ProducerIdentity;

    /// Declared cost/budget for one computation.
    fn budget(&self) -> FactBudget;

    /// Provenance every produced fact carries.
    fn provenance(&self) -> Provenance;

    /// Precision every produced fact carries.
    fn precision(&self) -> Precision;

    /// Typed invalidation key for the declared inputs. Exact-file producers
    /// normally return [`FileFactInvalidation::exact`]; any broader
    /// compatibility is an explicit producer decision.
    fn invalidation_key(&self, input: &FileFactInput<'_>) -> FileFactInvalidation;

    /// Computes the fact from the declared inputs. The store enforces
    /// [`FactBudget::max_wall_time`] on top of the caller's operation; the
    /// producer must still poll `operation` inside expensive loops.
    fn compute(
        &self,
        input: &FileFactInput<'_>,
        operation: &OperationContext,
    ) -> Result<Self::Fact, LazyFactError>;
}

/// How one returned fact was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactOrigin {
    /// Served from the store without computation.
    Cached,
    /// Computed by this request (the coalescing owner).
    Computed,
    /// Another in-flight request computed it; this request waited.
    Coalesced,
}

/// One lazy fact result with explicit provenance and precision.
#[derive(Debug, Clone)]
pub struct LazyFactOutcome<F> {
    pub fact: Arc<F>,
    pub origin: FactOrigin,
    pub provenance: Provenance,
    pub precision: Precision,
    pub revision: Revision,
}

/// Diagnostic counters of one store (future diagnostics surface).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LazyFactStats {
    pub hits: u64,
    pub misses: u64,
    /// Requests that found an in-flight computation and waited for it.
    pub coalesced_joins: u64,
    pub evictions: u64,
    pub failures: u64,
    /// Facts computed but not retained because they exceed the byte budget.
    pub oversize_drops: u64,
    /// Unique requests refused because every bounded entry was in flight.
    pub saturated: u64,
    pub entries: u64,
    pub retained_bytes: u64,
}

/// Failure shared with coalesced waiters but never retained in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SharedFailure {
    Aborted(OperationAbort),
    Failed {
        producer: &'static str,
        format_version: u32,
        message: String,
    },
    StoreSaturated {
        max_entries: usize,
    },
    WorkspaceMismatch {
        store: WorkspaceId,
        input: WorkspaceId,
    },
    FileNotInSnapshot {
        path: RepoRelativePath,
        revision: Revision,
    },
}

impl SharedFailure {
    fn into_error(self) -> LazyFactError {
        match self {
            Self::Aborted(abort) => LazyFactError::Aborted(abort),
            Self::Failed {
                producer,
                format_version,
                message,
            } => LazyFactError::Producer {
                producer,
                format_version,
                message,
            },
            Self::StoreSaturated { max_entries } => LazyFactError::StoreSaturated { max_entries },
            Self::WorkspaceMismatch { store, input } => {
                LazyFactError::WorkspaceMismatch { store, input }
            }
            Self::FileNotInSnapshot { path, revision } => {
                LazyFactError::FileNotInSnapshot { path, revision }
            }
        }
    }
}

impl LazyFactError {
    fn shared(&self) -> SharedFailure {
        match self {
            Self::Aborted(abort) => SharedFailure::Aborted(*abort),
            Self::Producer {
                producer,
                format_version,
                message,
            } => SharedFailure::Failed {
                producer,
                format_version: *format_version,
                message: message.clone(),
            },
            Self::StoreSaturated { max_entries } => SharedFailure::StoreSaturated {
                max_entries: *max_entries,
            },
            Self::WorkspaceMismatch { store, input } => SharedFailure::WorkspaceMismatch {
                store: store.clone(),
                input: input.clone(),
            },
            Self::FileNotInSnapshot { path, revision } => SharedFailure::FileNotInSnapshot {
                path: path.clone(),
                revision: *revision,
            },
        }
    }
}

enum SlotState<F> {
    Pending,
    Finished(Result<Arc<F>, SharedFailure>),
}

/// Completion rendezvous for one in-flight computation.
#[derive(Debug)]
struct InFlightSlot<F> {
    state: Mutex<SlotState<F>>,
    ready: Condvar,
}

impl<F> InFlightSlot<F> {
    fn new() -> Self {
        Self {
            state: Mutex::new(SlotState::Pending),
            ready: Condvar::new(),
        }
    }

    fn finish(&self, outcome: Result<Arc<F>, SharedFailure>) {
        *lock(&self.state) = SlotState::Finished(outcome);
        self.ready.notify_all();
    }
}

impl<F: std::fmt::Debug> std::fmt::Debug for SlotState<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => f.write_str("Pending"),
            Self::Finished(_) => f.write_str("Finished"),
        }
    }
}

#[derive(Debug)]
enum EntryState<F> {
    Ready(Arc<F>),
    InFlight(Arc<InFlightSlot<F>>),
}

#[derive(Debug)]
struct Entry<F> {
    state: EntryState<F>,
    /// Last-use tick; LRU eviction order among `Ready` entries.
    tick: u64,
    bytes: usize,
}

/// Invalidation key: compatible file content plus pinned revision. The
/// producer identity is the third component — it is fixed per store
/// instance, so it is a type-level key component rather than a runtime
/// string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FactKey {
    invalidation: FileFactInvalidation,
}

#[derive(Debug)]
struct StoreInner<F> {
    entries: HashMap<FactKey, Entry<F>>,
    total_bytes: usize,
    clock: u64,
    stats: LazyFactStats,
}

impl<F> StoreInner<F> {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            total_bytes: 0,
            clock: 0,
            stats: LazyFactStats::default(),
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }

    fn refresh_stats(&mut self) {
        self.stats.entries = self.entries.len() as u64;
        self.stats.retained_bytes = self.total_bytes as u64;
    }
}

/// What the key lookup decided for one request.
enum Decision<F> {
    Hit(Arc<F>),
    Join(Arc<InFlightSlot<F>>),
    Own(Arc<InFlightSlot<F>>),
}

/// Workspace-bound, bounded, typed, coalescing store for the facts of one
/// producer.
///
/// Concurrent requests for the same key run the producer exactly once;
/// duplicate requesters wait within their own operation budget and receive
/// the shared result or the shared failure. A failed or cancelled
/// computation is never retained, so cancellation cannot poison the store.
pub struct LazyFactStore<P: LazyFactProducer> {
    workspace: WorkspaceId,
    producer: Arc<P>,
    producer_identity: ProducerIdentity,
    producer_budget: FactBudget,
    provenance: Provenance,
    precision: Precision,
    bounds: FactStoreBounds,
    inner: Mutex<StoreInner<P::Fact>>,
}

impl<P: LazyFactProducer> std::fmt::Debug for LazyFactStore<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyFactStore")
            .field("workspace", &self.workspace)
            .field("producer", &self.producer_identity)
            .field("bounds", &self.bounds)
            .finish_non_exhaustive()
    }
}

impl<P: LazyFactProducer> LazyFactStore<P> {
    pub fn new(workspace: WorkspaceId, producer: Arc<P>, bounds: FactStoreBounds) -> Self {
        let producer_identity = producer.identity();
        let producer_budget = producer.budget();
        let provenance = producer.provenance();
        let precision = producer.precision();
        Self {
            workspace,
            producer,
            producer_identity,
            producer_budget,
            provenance,
            precision,
            bounds,
            inner: Mutex::new(StoreInner::new()),
        }
    }

    pub fn producer(&self) -> &Arc<P> {
        &self.producer
    }

    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }

    /// Returns the fact for `input`, computing it on first request.
    ///
    /// The caller's `operation` bounds both its own compute (when this
    /// request is the owner) and its wait (when coalesced behind an
    /// in-flight request). Cancellation while waiting returns an honest
    /// [`OperationAbort`] to this caller only; the shared computation is
    /// unaffected and its success is still retained for others.
    pub fn get_or_compute(
        &self,
        input: &FileFactInput<'_>,
        operation: &OperationContext,
    ) -> Result<LazyFactOutcome<P::Fact>, LazyFactError> {
        operation.check()?;
        if input.workspace != &self.workspace {
            return Err(LazyFactError::WorkspaceMismatch {
                store: self.workspace.clone(),
                input: input.workspace.clone(),
            });
        }
        let key = FactKey {
            invalidation: self.producer.invalidation_key(input),
        };
        // The lookup guard must drop before any compute or wait: the owner
        // path re-locks the store to publish the completed fact.
        let decision = {
            let inner = &mut *lock(&self.inner);
            let tick = inner.next_tick();
            match inner.entries.get_mut(&key) {
                Some(entry) => {
                    entry.tick = tick;
                    match &entry.state {
                        EntryState::Ready(fact) => {
                            inner.stats.hits = inner.stats.hits.saturating_add(1);
                            Decision::Hit(fact.clone())
                        }
                        EntryState::InFlight(slot) => {
                            inner.stats.coalesced_joins =
                                inner.stats.coalesced_joins.saturating_add(1);
                            Decision::Join(slot.clone())
                        }
                    }
                }
                None => {
                    if !self.make_room_for_entry(inner) {
                        inner.stats.saturated = inner.stats.saturated.saturating_add(1);
                        inner.refresh_stats();
                        return Err(LazyFactError::StoreSaturated {
                            max_entries: self.bounds.max_entries,
                        });
                    }
                    let slot = Arc::new(InFlightSlot::new());
                    inner.entries.insert(
                        key.clone(),
                        Entry {
                            state: EntryState::InFlight(slot.clone()),
                            tick,
                            bytes: 0,
                        },
                    );
                    inner.stats.misses = inner.stats.misses.saturating_add(1);
                    inner.refresh_stats();
                    Decision::Own(slot)
                }
            }
        };
        match decision {
            Decision::Hit(fact) => Ok(self.outcome(fact, FactOrigin::Cached, input.revision)),
            Decision::Join(slot) => self.wait_coalesced(slot, input.revision, operation),
            Decision::Own(slot) => self.compute_as_owner(key, slot, input, operation),
        }
    }

    /// Diagnostic counters snapshot.
    pub fn stats(&self) -> LazyFactStats {
        lock(&self.inner).stats
    }

    fn outcome(
        &self,
        fact: Arc<P::Fact>,
        origin: FactOrigin,
        revision: Revision,
    ) -> LazyFactOutcome<P::Fact> {
        LazyFactOutcome {
            fact,
            origin,
            provenance: self.provenance,
            precision: self.precision,
            revision,
        }
    }

    fn compute_as_owner(
        &self,
        key: FactKey,
        slot: Arc<InFlightSlot<P::Fact>>,
        input: &FileFactInput<'_>,
        operation: &OperationContext,
    ) -> Result<LazyFactOutcome<P::Fact>, LazyFactError> {
        let bounded = operation.bounded_by(self.producer_budget.max_wall_time);
        let computed = self.producer.compute(input, &bounded).and_then(|fact| {
            // Cooperative polling bounds the producer while it runs; the
            // final check prevents a late result from being published.
            bounded.check()?;
            Ok(fact)
        });
        match computed {
            Ok(fact) => {
                let fact = Arc::new(fact);
                let mut slot_finished = false;
                {
                    let inner = &mut *lock(&self.inner);
                    let bytes = fact.retained_bytes();
                    if bytes <= self.producer_budget.max_fact_bytes {
                        let tick = inner.next_tick();
                        if let Some(entry) = inner.entries.get_mut(&key) {
                            entry.state = EntryState::Ready(fact.clone());
                            entry.tick = tick;
                            entry.bytes = bytes;
                            inner.total_bytes = inner.total_bytes.saturating_add(bytes);
                        }
                        self.evict_within_bounds(inner);
                    } else {
                        // Publish to current waiters before removing the slot,
                        // so a new identical request cannot start another
                        // computation during the completion window.
                        slot.finish(Ok(fact.clone()));
                        slot_finished = true;
                        inner.entries.remove(&key);
                        inner.stats.oversize_drops = inner.stats.oversize_drops.saturating_add(1);
                    }
                    inner.refresh_stats();
                }
                if !slot_finished {
                    slot.finish(Ok(fact.clone()));
                }
                Ok(self.outcome(fact, FactOrigin::Computed, input.revision))
            }
            Err(error) => {
                // Never retain failures or cancellations: remove the slot so
                // the next request recomputes from scratch. Waiters receive
                // the shared failure once; nothing is cached.
                let shared = error.shared();
                {
                    let inner = &mut *lock(&self.inner);
                    inner.stats.failures = inner.stats.failures.saturating_add(1);
                    // Finish while the map still names this slot. New
                    // duplicate requests either joined it already or wait on
                    // the store lock until removal; none can overlap a retry
                    // with the completing owner.
                    slot.finish(Err(shared));
                    inner.entries.remove(&key);
                    inner.refresh_stats();
                }
                Err(error)
            }
        }
    }

    fn wait_coalesced(
        &self,
        slot: Arc<InFlightSlot<P::Fact>>,
        revision: Revision,
        operation: &OperationContext,
    ) -> Result<LazyFactOutcome<P::Fact>, LazyFactError> {
        let mut state = lock(&slot.state);
        loop {
            if let SlotState::Finished(outcome) = &*state {
                return match outcome.clone() {
                    Ok(fact) => Ok(self.outcome(fact, FactOrigin::Coalesced, revision)),
                    Err(failure) => Err(failure.into_error()),
                };
            }
            // Bounded wait: the caller's cancellation/deadline is re-checked
            // every poll even when the owner makes no progress.
            let wait = operation.poll_timeout(WAIT_POLL_CAP)?;
            let (next, _) = match slot.ready.wait_timeout(state, wait) {
                Ok(waited) => waited,
                Err(poisoned) => poisoned.into_inner(),
            };
            state = next;
        }
    }

    /// Evicts least-recently-used `Ready` entries until both bounds hold.
    /// In-flight entries are never evicted.
    fn evict_within_bounds(&self, inner: &mut StoreInner<P::Fact>) {
        while inner.entries.len() > self.bounds.max_entries
            || inner.total_bytes > self.bounds.max_total_bytes
        {
            let oldest = inner
                .entries
                .iter()
                .filter(|(_, entry)| matches!(entry.state, EntryState::Ready(_)))
                .min_by_key(|(_, entry)| entry.tick)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    if let Some(entry) = inner.entries.remove(&key) {
                        inner.total_bytes = inner.total_bytes.saturating_sub(entry.bytes);
                        inner.stats.evictions = inner.stats.evictions.saturating_add(1);
                    }
                }
                // Only in-flight entries remain; bounds apply at completion.
                None => break,
            }
        }
    }

    /// Reserves one strict count-bounded slot for a new unique computation.
    /// Ready LRU entries may be evicted; if every slot is in flight, reject
    /// with typed backpressure instead of growing unboundedly.
    fn make_room_for_entry(&self, inner: &mut StoreInner<P::Fact>) -> bool {
        while inner.entries.len() >= self.bounds.max_entries {
            let oldest = inner
                .entries
                .iter()
                .filter(|(_, entry)| matches!(entry.state, EntryState::Ready(_)))
                .min_by_key(|(_, entry)| entry.tick)
                .map(|(key, _)| key.clone());
            let Some(key) = oldest else {
                return false;
            };
            if let Some(entry) = inner.entries.remove(&key) {
                inner.total_bytes = inner.total_bytes.saturating_sub(entry.bytes);
                inner.stats.evictions = inner.stats.evictions.saturating_add(1);
            }
        }
        true
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Rendered per-file outline digest: symbol kind, name, first signature
/// line, and start position for every symbol in one file.
///
/// Derived entirely from the syntax-indexed graph revision, so the fact is
/// `Provenance::TreeSitter` / `Precision::Syntax` — never precise-tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutlineDigest {
    pub path: RepoRelativePath,
    pub symbol_count: usize,
    pub truncated: bool,
    pub digest: String,
    retained_bytes: usize,
}

impl LazyFact for FileOutlineDigest {
    fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

/// Example producer: a rendered outline/signature digest of one file.
///
/// Why lazy: the digest is a rendered string per file — retaining one for
/// every indexed file eagerly multiplies memory and formats thousands of
/// files an agent will never ask about, while the typical `context`-style
/// workload touches a small fraction of files. The graph already holds the
/// structured symbols; rendering is deferred until a file is actually
/// requested, then cached against (content, revision).
#[derive(Debug, Default)]
pub struct FileOutlineDigestProducer;

impl FileOutlineDigestProducer {
    pub fn new() -> Self {
        Self
    }
}

impl LazyFactProducer for FileOutlineDigestProducer {
    type Fact = FileOutlineDigest;

    fn identity(&self) -> ProducerIdentity {
        ProducerIdentity {
            id: "file_outline_digest",
            format_version: 1,
        }
    }

    fn budget(&self) -> FactBudget {
        FactBudget {
            max_wall_time: Duration::from_millis(250),
            max_fact_bytes: 256 * 1024,
        }
    }

    fn provenance(&self) -> Provenance {
        Provenance::TreeSitter
    }

    fn precision(&self) -> Precision {
        Precision::Syntax
    }

    fn invalidation_key(&self, input: &FileFactInput<'_>) -> FileFactInvalidation {
        FileFactInvalidation::exact(input)
    }

    fn compute(
        &self,
        input: &FileFactInput<'_>,
        operation: &OperationContext,
    ) -> Result<Self::Fact, LazyFactError> {
        let mut symbols = Vec::with_capacity(MAX_OUTLINE_LINES);
        let mut symbol_count = 0_usize;
        let mut truncated = false;
        for symbol in input.graph().symbols_in_file(input.path()) {
            if symbol_count.is_multiple_of(64) {
                operation.check()?;
            }
            symbol_count = symbol_count.saturating_add(1);
            if symbols.len() < MAX_OUTLINE_LINES {
                symbols.push(symbol);
            } else {
                truncated = true;
            }
        }
        operation.check()?;
        symbols.sort_by_key(|symbol| {
            (
                symbol.location.start().line(),
                symbol.location.start().column(),
            )
        });

        let mut digest = String::new();
        for (index, symbol) in symbols.iter().enumerate() {
            // Cooperative cancellation inside the bounded render loop.
            if index.is_multiple_of(64) {
                operation.check()?;
            }
            let signature = symbol
                .signature
                .as_deref()
                .and_then(|signature| signature.lines().next())
                .unwrap_or_default();
            let _ = writeln!(
                digest,
                "{:?} {} @ {}:{} {}",
                symbol.key.kind,
                symbol.name(),
                symbol.location.start().line(),
                symbol.location.start().column(),
                signature,
            );
        }

        let retained_bytes = std::mem::size_of::<FileOutlineDigest>()
            .saturating_add(input.path().as_str().len())
            .saturating_add(digest.len());
        Ok(FileOutlineDigest {
            path: input.path().clone(),
            symbol_count,
            truncated,
            digest,
            retained_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::thread::{self, JoinHandle};

    use chakra_domain::identity::WorkspaceIdentity;
    use chakra_domain::location::{SourceRange, TextPosition};
    use chakra_domain::operation::CancellationToken;
    use chakra_domain::symbol::{Language, SymbolKey, SymbolKind};

    use crate::engine::WorkspaceEngine;
    use crate::graph::{BoundedGraphBuilder, GraphBuildLimits};

    fn path(relative: &str) -> Result<RepoRelativePath, Box<dyn std::error::Error>> {
        Ok(RepoRelativePath::new(relative)?)
    }

    fn input<'a>(
        workspace: &'a WorkspaceId,
        path: &'a RepoRelativePath,
        revision: Revision,
        source: &'a str,
        graph: &'a SymbolGraph,
    ) -> FileFactInput<'a> {
        FileFactInput {
            workspace,
            path,
            revision,
            source,
            graph,
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct CountFact {
        sequence: u64,
        bytes: usize,
    }

    impl LazyFact for CountFact {
        fn retained_bytes(&self) -> usize {
            self.bytes
        }
    }

    /// Counting producer whose fact is its own call sequence number.
    #[derive(Debug, Default)]
    struct CountingProducer {
        calls: AtomicU64,
    }

    impl CountingProducer {
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::Acquire)
        }
    }

    impl LazyFactProducer for CountingProducer {
        type Fact = CountFact;

        fn identity(&self) -> ProducerIdentity {
            ProducerIdentity {
                id: "counting",
                format_version: 1,
            }
        }

        fn budget(&self) -> FactBudget {
            FactBudget {
                max_wall_time: Duration::from_secs(60),
                max_fact_bytes: 1_024,
            }
        }

        fn provenance(&self) -> Provenance {
            Provenance::Heuristic
        }

        fn precision(&self) -> Precision {
            Precision::Heuristic
        }

        fn invalidation_key(&self, input: &FileFactInput<'_>) -> FileFactInvalidation {
            FileFactInvalidation::exact(input)
        }

        fn compute(
            &self,
            _input: &FileFactInput<'_>,
            operation: &OperationContext,
        ) -> Result<Self::Fact, LazyFactError> {
            operation.check()?;
            let sequence = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
            Ok(CountFact {
                sequence,
                bytes: 64,
            })
        }
    }

    /// Producer that signals when it entered compute, then blocks until the
    /// test releases it. Deterministic coalescing control without sleeps.
    #[derive(Debug)]
    struct GatedProducer {
        calls: AtomicU64,
        entered: Sender<()>,
        release: Mutex<Option<Receiver<()>>>,
    }

    impl GatedProducer {
        fn new() -> (Self, Receiver<()>, Sender<()>) {
            let (entered_tx, entered_rx) = channel();
            let (release_tx, release_rx) = channel();
            (
                Self {
                    calls: AtomicU64::new(0),
                    entered: entered_tx,
                    release: Mutex::new(Some(release_rx)),
                },
                entered_rx,
                release_tx,
            )
        }

        fn calls(&self) -> u64 {
            self.calls.load(Ordering::Acquire)
        }
    }

    impl LazyFactProducer for GatedProducer {
        type Fact = CountFact;

        fn identity(&self) -> ProducerIdentity {
            ProducerIdentity {
                id: "gated",
                format_version: 1,
            }
        }

        fn budget(&self) -> FactBudget {
            FactBudget {
                max_wall_time: Duration::from_secs(60),
                max_fact_bytes: 1_024,
            }
        }

        fn provenance(&self) -> Provenance {
            Provenance::Heuristic
        }

        fn precision(&self) -> Precision {
            Precision::Heuristic
        }

        fn invalidation_key(&self, input: &FileFactInput<'_>) -> FileFactInvalidation {
            FileFactInvalidation::exact(input)
        }

        fn compute(
            &self,
            _input: &FileFactInput<'_>,
            _operation: &OperationContext,
        ) -> Result<Self::Fact, LazyFactError> {
            let sequence = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
            let _ = self.entered.send(());
            let release = lock(&self.release).take();
            if let Some(release) = release {
                // Blocking recv: the test decides when compute finishes.
                if release.recv().is_err() {
                    return Err(LazyFactError::Producer {
                        producer: "gated",
                        format_version: 1,
                        message: "release channel closed".to_owned(),
                    });
                }
            }
            Ok(CountFact {
                sequence,
                bytes: 64,
            })
        }
    }

    /// Producer whose first computation parks cooperatively until the caller
    /// aborts it; later computations succeed immediately. Deterministic
    /// cancellation control without sleeps.
    #[derive(Debug)]
    struct CancelOnceProducer {
        calls: AtomicU64,
        entered: Sender<()>,
    }

    impl CancelOnceProducer {
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::Acquire)
        }
    }

    impl LazyFactProducer for CancelOnceProducer {
        type Fact = CountFact;

        fn identity(&self) -> ProducerIdentity {
            ProducerIdentity {
                id: "cancel-once",
                format_version: 1,
            }
        }

        fn budget(&self) -> FactBudget {
            FactBudget {
                max_wall_time: Duration::from_secs(60),
                max_fact_bytes: 1_024,
            }
        }

        fn provenance(&self) -> Provenance {
            Provenance::Heuristic
        }

        fn precision(&self) -> Precision {
            Precision::Heuristic
        }

        fn invalidation_key(&self, input: &FileFactInput<'_>) -> FileFactInvalidation {
            FileFactInvalidation::exact(input)
        }

        fn compute(
            &self,
            _input: &FileFactInput<'_>,
            operation: &OperationContext,
        ) -> Result<Self::Fact, LazyFactError> {
            let sequence = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
            if sequence == 1 {
                let _ = self.entered.send(());
                // Diverges until the operation aborts: cancellation is the
                // only way out of the first computation.
                loop {
                    operation.check()?;
                    thread::yield_now();
                }
            }
            Ok(CountFact {
                sequence,
                bytes: 64,
            })
        }
    }

    /// Producer that fails on demand with a typed producer error.
    #[derive(Debug)]
    struct FlakyProducer {
        calls: AtomicU64,
    }

    impl FlakyProducer {
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::Acquire)
        }
    }

    impl LazyFactProducer for FlakyProducer {
        type Fact = CountFact;

        fn identity(&self) -> ProducerIdentity {
            ProducerIdentity {
                id: "flaky",
                format_version: 1,
            }
        }

        fn budget(&self) -> FactBudget {
            FactBudget {
                max_wall_time: Duration::from_secs(60),
                max_fact_bytes: 1_024,
            }
        }

        fn provenance(&self) -> Provenance {
            Provenance::Heuristic
        }

        fn precision(&self) -> Precision {
            Precision::Heuristic
        }

        fn invalidation_key(&self, input: &FileFactInput<'_>) -> FileFactInvalidation {
            FileFactInvalidation::exact(input)
        }

        fn compute(
            &self,
            _input: &FileFactInput<'_>,
            _operation: &OperationContext,
        ) -> Result<Self::Fact, LazyFactError> {
            let sequence = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
            if sequence == 1 {
                return Err(LazyFactError::Producer {
                    producer: "flaky",
                    format_version: 1,
                    message: "transient failure".to_owned(),
                });
            }
            Ok(CountFact {
                sequence,
                bytes: 64,
            })
        }
    }

    fn store<P: LazyFactProducer>(
        producer: Arc<P>,
        bounds: FactStoreBounds,
    ) -> Result<Arc<LazyFactStore<P>>, Box<dyn std::error::Error>> {
        let root = std::env::current_dir()?;
        let workspace = WorkspaceIdentity::for_primary_worktree(&root)?.workspace;
        Ok(Arc::new(LazyFactStore::new(workspace, producer, bounds)))
    }

    /// Bounded spin on an eventually-true condition (no sleeps, no flakes).
    fn wait_until(
        condition: impl Fn() -> bool,
        what: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !condition() {
            if std::time::Instant::now() > deadline {
                return Err(format!("timed out waiting for {what}").into());
            }
            thread::yield_now();
        }
        Ok(())
    }

    #[test]
    fn second_identical_request_hits_without_recomputing() -> Result<(), Box<dyn std::error::Error>>
    {
        let producer = Arc::new(CountingProducer::default());
        let store = store(producer.clone(), FactStoreBounds::default())?;
        let graph = SymbolGraph::new();
        let path = path("src/lib.rs")?;
        let operation = OperationContext::unbounded();

        let first = store.get_or_compute(
            &input(store.workspace(), &path, Revision(1), "fn a() {}", &graph),
            &operation,
        )?;
        let second = store.get_or_compute(
            &input(store.workspace(), &path, Revision(1), "fn a() {}", &graph),
            &operation,
        )?;

        assert_eq!(producer.calls(), 1);
        assert_eq!(first.origin, FactOrigin::Computed);
        assert_eq!(second.origin, FactOrigin::Cached);
        assert!(Arc::ptr_eq(&first.fact, &second.fact));
        assert_eq!(store.stats().hits, 1);
        assert_eq!(store.stats().misses, 1);
        Ok(())
    }

    #[test]
    fn identical_content_in_different_paths_never_aliases() -> Result<(), Box<dyn std::error::Error>>
    {
        let producer = Arc::new(CountingProducer::default());
        let store = store(producer.clone(), FactStoreBounds::default())?;
        let graph = SymbolGraph::new();
        let first_path = path("src/first.rs")?;
        let second_path = path("src/second.rs")?;
        let operation = OperationContext::unbounded();
        let source = "fn same() {}";

        let first = store.get_or_compute(
            &input(store.workspace(), &first_path, Revision(1), source, &graph),
            &operation,
        )?;
        let second = store.get_or_compute(
            &input(store.workspace(), &second_path, Revision(1), source, &graph),
            &operation,
        )?;

        assert_eq!(first.origin, FactOrigin::Computed);
        assert_eq!(second.origin, FactOrigin::Computed);
        assert_eq!(producer.calls(), 2);
        assert_eq!(store.stats().entries, 2);
        Ok(())
    }

    #[test]
    fn input_from_another_workspace_is_rejected_before_lookup()
    -> Result<(), Box<dyn std::error::Error>> {
        let producer = Arc::new(CountingProducer::default());
        let store = store(producer.clone(), FactStoreBounds::default())?;
        let foreign_root = std::env::current_dir()?.join("../../fixtures");
        let foreign = WorkspaceIdentity::for_primary_worktree(&foreign_root)?.workspace;
        assert_ne!(store.workspace(), &foreign);
        let graph = SymbolGraph::new();
        let path = path("src/lib.rs")?;

        let result = store.get_or_compute(
            &input(&foreign, &path, Revision(1), "fn a() {}", &graph),
            &OperationContext::unbounded(),
        );

        assert!(matches!(
            result,
            Err(LazyFactError::WorkspaceMismatch { .. })
        ));
        assert_eq!(producer.calls(), 0);
        assert_eq!(store.stats(), LazyFactStats::default());
        Ok(())
    }

    #[test]
    fn public_input_constructor_rejects_a_file_absent_from_the_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::current_dir()?;
        let identity = WorkspaceIdentity::for_primary_worktree(&root)?;
        let engine = WorkspaceEngine::new(identity);
        let snapshot = engine.snapshot();
        let missing = path("src/missing.rs")?;

        let result = FileFactInput::from_snapshot(&snapshot, &missing);

        assert!(matches!(
            result,
            Err(LazyFactError::FileNotInSnapshot {
                path,
                revision: Revision(0),
            }) if path == missing
        ));
        Ok(())
    }

    #[test]
    fn concurrent_duplicate_requests_run_one_computation() -> Result<(), Box<dyn std::error::Error>>
    {
        let (producer, entered, release) = GatedProducer::new();
        let producer = Arc::new(producer);
        let store = store(producer.clone(), FactStoreBounds::default())?;
        let path = path("src/lib.rs")?;
        let source = "fn shared() {}";

        // Owner request on its own thread: it blocks inside compute.
        let owner = {
            let store = store.clone();
            let path = path.clone();
            thread::spawn(move || {
                let graph = SymbolGraph::new();
                store.get_or_compute(
                    &input(store.workspace(), &path, Revision(1), source, &graph),
                    &OperationContext::unbounded(),
                )
            })
        };
        entered.recv()?;

        // Two duplicate requesters join the in-flight computation.
        let waiters: Vec<JoinHandle<_>> = (0..2)
            .map(|_| {
                let store = store.clone();
                let path = path.clone();
                thread::spawn(move || {
                    let graph = SymbolGraph::new();
                    store.get_or_compute(
                        &input(store.workspace(), &path, Revision(1), source, &graph),
                        &OperationContext::unbounded(),
                    )
                })
            })
            .collect();
        wait_until(
            || store.stats().coalesced_joins == 2,
            "duplicate requests to join the in-flight slot",
        )?;
        release.send(())?;

        let owner_outcome = owner.join().map_err(|_| "owner thread panicked")??;
        assert_eq!(owner_outcome.origin, FactOrigin::Computed);
        for waiter in waiters {
            let outcome = waiter.join().map_err(|_| "waiter thread panicked")??;
            assert_eq!(outcome.origin, FactOrigin::Coalesced);
            assert!(Arc::ptr_eq(&owner_outcome.fact, &outcome.fact));
        }
        assert_eq!(producer.calls(), 1);
        assert_eq!(store.stats().coalesced_joins, 2);
        Ok(())
    }

    #[test]
    fn unique_in_flight_work_is_strictly_count_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let (producer, entered, release) = GatedProducer::new();
        let producer = Arc::new(producer);
        let store = store(
            producer.clone(),
            FactStoreBounds {
                max_entries: 1,
                max_total_bytes: usize::MAX,
            },
        )?;
        let first_path = path("src/first.rs")?;
        let second_path = path("src/second.rs")?;
        let source = "fn shared_bytes() {}";

        let owner = {
            let store = store.clone();
            let first_path = first_path.clone();
            thread::spawn(move || {
                let graph = SymbolGraph::new();
                store.get_or_compute(
                    &input(store.workspace(), &first_path, Revision(1), source, &graph),
                    &OperationContext::unbounded(),
                )
            })
        };
        entered.recv()?;

        let graph = SymbolGraph::new();
        let rejected = store.get_or_compute(
            &input(store.workspace(), &second_path, Revision(1), source, &graph),
            &OperationContext::unbounded(),
        );
        assert!(matches!(
            rejected,
            Err(LazyFactError::StoreSaturated { max_entries: 1 })
        ));
        assert_eq!(store.stats().entries, 1);
        assert_eq!(store.stats().saturated, 1);
        assert_eq!(producer.calls(), 1);

        release.send(())?;
        owner.join().map_err(|_| "owner thread panicked")??;
        Ok(())
    }

    #[test]
    fn owner_cancellation_never_poisons_and_retry_recomputes()
    -> Result<(), Box<dyn std::error::Error>> {
        let (entered_tx, entered) = channel();
        let producer = Arc::new(CancelOnceProducer {
            calls: AtomicU64::new(0),
            entered: entered_tx,
        });
        let store = store(producer.clone(), FactStoreBounds::default())?;
        let path = path("src/lib.rs")?;
        let source = "fn cancelled() {}";

        let cancellation = CancellationToken::default();
        let owner_operation = OperationContext::from_cancellation(cancellation.clone());
        let owner = {
            let store = store.clone();
            let path = path.clone();
            thread::spawn(move || {
                let graph = SymbolGraph::new();
                store.get_or_compute(
                    &input(store.workspace(), &path, Revision(1), source, &graph),
                    &owner_operation,
                )
            })
        };
        entered.recv()?;
        cancellation.cancel();

        let outcome = owner.join().map_err(|_| "owner thread panicked")?;
        assert!(matches!(
            outcome,
            Err(LazyFactError::Aborted(OperationAbort::Cancelled))
        ));
        assert_eq!(store.stats().entries, 0);
        assert_eq!(store.stats().failures, 1);

        // The cancelled computation left nothing behind: the same key
        // recomputes from scratch and is retained on success.
        let graph = SymbolGraph::new();
        let operation = OperationContext::unbounded();
        let retried = store.get_or_compute(
            &input(store.workspace(), &path, Revision(1), source, &graph),
            &operation,
        )?;
        assert_eq!(retried.origin, FactOrigin::Computed);
        assert_eq!(retried.fact.sequence, 2);
        assert_eq!(producer.calls(), 2);
        assert_eq!(store.stats().entries, 1);

        let cached = store.get_or_compute(
            &input(store.workspace(), &path, Revision(1), source, &graph),
            &operation,
        )?;
        assert_eq!(cached.origin, FactOrigin::Cached);
        assert_eq!(producer.calls(), 2);
        Ok(())
    }

    #[test]
    fn failure_is_never_cached_and_retry_recomputes() -> Result<(), Box<dyn std::error::Error>> {
        let producer = Arc::new(FlakyProducer {
            calls: AtomicU64::new(0),
        });
        let store = store(producer.clone(), FactStoreBounds::default())?;
        let graph = SymbolGraph::new();
        let path = path("src/lib.rs")?;
        let source = "fn flaky() {}";
        let operation = OperationContext::unbounded();

        let failed = store.get_or_compute(
            &input(store.workspace(), &path, Revision(1), source, &graph),
            &operation,
        );
        assert!(matches!(
            failed,
            Err(LazyFactError::Producer {
                producer: "flaky",
                format_version: 1,
                ..
            })
        ));
        assert_eq!(store.stats().entries, 0);
        assert_eq!(store.stats().failures, 1);

        let retried = store.get_or_compute(
            &input(store.workspace(), &path, Revision(1), source, &graph),
            &operation,
        )?;
        assert_eq!(retried.fact.sequence, 2);
        assert_eq!(producer.calls(), 2);
        assert_eq!(store.stats().entries, 1);
        Ok(())
    }

    #[test]
    fn coalesced_waiter_deadline_leaves_owner_unaffected() -> Result<(), Box<dyn std::error::Error>>
    {
        let (producer, entered, release) = GatedProducer::new();
        let producer = Arc::new(producer);
        let store = store(producer.clone(), FactStoreBounds::default())?;
        let path = path("src/lib.rs")?;
        let source = "fn gated() {}";

        let owner = {
            let store = store.clone();
            let path = path.clone();
            thread::spawn(move || {
                let graph = SymbolGraph::new();
                store.get_or_compute(
                    &input(store.workspace(), &path, Revision(1), source, &graph),
                    &OperationContext::unbounded(),
                )
            })
        };
        entered.recv()?;

        // Waiter with a tight deadline: the owner never finishes in time.
        let waiter = {
            let store = store.clone();
            let path = path.clone();
            thread::spawn(move || {
                let graph = SymbolGraph::new();
                store.get_or_compute(
                    &input(store.workspace(), &path, Revision(1), source, &graph),
                    &OperationContext::with_timeout(Duration::from_millis(50)),
                )
            })
        };
        wait_until(
            || store.stats().coalesced_joins == 1,
            "waiter to join the in-flight slot",
        )?;
        let waiter_outcome = waiter.join().map_err(|_| "waiter thread panicked")?;
        assert!(matches!(
            waiter_outcome,
            Err(LazyFactError::Aborted(OperationAbort::DeadlineExceeded))
        ));

        // The owner is still computing and completes normally; the waiter's
        // abort neither removed nor poisoned anything.
        release.send(())?;
        let owner_outcome = owner.join().map_err(|_| "owner thread panicked")??;
        assert_eq!(owner_outcome.origin, FactOrigin::Computed);
        assert_eq!(store.stats().entries, 1);
        assert_eq!(producer.calls(), 1);

        let graph = SymbolGraph::new();
        let cached = store.get_or_compute(
            &input(store.workspace(), &path, Revision(1), source, &graph),
            &OperationContext::unbounded(),
        )?;
        assert_eq!(cached.origin, FactOrigin::Cached);
        assert_eq!(producer.calls(), 1);
        Ok(())
    }

    #[test]
    fn count_bound_evicts_least_recently_used() -> Result<(), Box<dyn std::error::Error>> {
        let producer = Arc::new(CountingProducer::default());
        let bounds = FactStoreBounds {
            max_entries: 2,
            max_total_bytes: usize::MAX,
        };
        let store = store(producer.clone(), bounds)?;
        let graph = SymbolGraph::new();
        let path = path("src/lib.rs")?;
        let operation = OperationContext::unbounded();
        let revision = Revision(1);

        store.get_or_compute(
            &input(store.workspace(), &path, revision, "fn one() {}", &graph),
            &operation,
        )?;
        store.get_or_compute(
            &input(store.workspace(), &path, revision, "fn two() {}", &graph),
            &operation,
        )?;
        // Touch the first so the second becomes LRU.
        store.get_or_compute(
            &input(store.workspace(), &path, revision, "fn one() {}", &graph),
            &operation,
        )?;
        store.get_or_compute(
            &input(store.workspace(), &path, revision, "fn three() {}", &graph),
            &operation,
        )?;

        let stats = store.stats();
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.evictions, 1);
        assert_eq!(producer.calls(), 3);

        // "two" was evicted (LRU); "one" survived because it was re-touched.
        let one = store.get_or_compute(
            &input(store.workspace(), &path, revision, "fn one() {}", &graph),
            &operation,
        )?;
        assert_eq!(one.origin, FactOrigin::Cached);
        let two = store.get_or_compute(
            &input(store.workspace(), &path, revision, "fn two() {}", &graph),
            &operation,
        )?;
        assert_eq!(two.origin, FactOrigin::Computed);
        Ok(())
    }

    #[test]
    fn byte_bound_evicts_until_within_budget() -> Result<(), Box<dyn std::error::Error>> {
        let producer = Arc::new(CountingProducer::default());
        // Each fact reports 64 bytes; room for exactly two.
        let bounds = FactStoreBounds {
            max_entries: usize::MAX,
            max_total_bytes: 128,
        };
        let store = store(producer.clone(), bounds)?;
        let graph = SymbolGraph::new();
        let path = path("src/lib.rs")?;
        let operation = OperationContext::unbounded();
        let revision = Revision(1);

        for source in ["fn a() {}", "fn b() {}", "fn c() {}"] {
            store.get_or_compute(
                &input(store.workspace(), &path, revision, source, &graph),
                &operation,
            )?;
        }
        let stats = store.stats();
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.retained_bytes, 128);
        assert_eq!(stats.evictions, 1);
        Ok(())
    }

    #[test]
    fn oversize_fact_is_returned_but_never_retained() -> Result<(), Box<dyn std::error::Error>> {
        #[derive(Debug)]
        struct HugeProducer;
        impl LazyFactProducer for HugeProducer {
            type Fact = CountFact;
            fn identity(&self) -> ProducerIdentity {
                ProducerIdentity {
                    id: "huge",
                    format_version: 1,
                }
            }
            fn budget(&self) -> FactBudget {
                FactBudget {
                    max_wall_time: Duration::from_secs(60),
                    max_fact_bytes: 16,
                }
            }
            fn provenance(&self) -> Provenance {
                Provenance::Heuristic
            }
            fn precision(&self) -> Precision {
                Precision::Heuristic
            }
            fn invalidation_key(&self, input: &FileFactInput<'_>) -> FileFactInvalidation {
                FileFactInvalidation::exact(input)
            }
            fn compute(
                &self,
                _input: &FileFactInput<'_>,
                _operation: &OperationContext,
            ) -> Result<Self::Fact, LazyFactError> {
                Ok(CountFact {
                    sequence: 1,
                    bytes: 64,
                })
            }
        }

        let store = store(Arc::new(HugeProducer), FactStoreBounds::default())?;
        let graph = SymbolGraph::new();
        let path = path("src/big.rs")?;
        let operation = OperationContext::unbounded();

        let outcome = store.get_or_compute(
            &input(
                store.workspace(),
                &path,
                Revision(1),
                "fn huge() {}",
                &graph,
            ),
            &operation,
        )?;
        assert_eq!(outcome.origin, FactOrigin::Computed);
        let stats = store.stats();
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.oversize_drops, 1);
        // Never retained: an identical request recomputes.
        let again = store.get_or_compute(
            &input(
                store.workspace(),
                &path,
                Revision(1),
                "fn huge() {}",
                &graph,
            ),
            &operation,
        )?;
        assert_eq!(again.origin, FactOrigin::Computed);
        assert_eq!(store.stats().oversize_drops, 2);
        Ok(())
    }

    #[test]
    fn result_returned_after_the_declared_deadline_is_not_published()
    -> Result<(), Box<dyn std::error::Error>> {
        #[derive(Debug)]
        struct LateProducer;
        impl LazyFactProducer for LateProducer {
            type Fact = CountFact;
            fn identity(&self) -> ProducerIdentity {
                ProducerIdentity {
                    id: "late",
                    format_version: 1,
                }
            }
            fn budget(&self) -> FactBudget {
                FactBudget {
                    max_wall_time: Duration::ZERO,
                    max_fact_bytes: 1_024,
                }
            }
            fn provenance(&self) -> Provenance {
                Provenance::Heuristic
            }
            fn precision(&self) -> Precision {
                Precision::Heuristic
            }
            fn invalidation_key(&self, input: &FileFactInput<'_>) -> FileFactInvalidation {
                FileFactInvalidation::exact(input)
            }
            fn compute(
                &self,
                _input: &FileFactInput<'_>,
                _operation: &OperationContext,
            ) -> Result<Self::Fact, LazyFactError> {
                // Deliberately ignore the operation: the store's final budget
                // check must still prevent this late result from publishing.
                Ok(CountFact {
                    sequence: 1,
                    bytes: 64,
                })
            }
        }

        let store = store(Arc::new(LateProducer), FactStoreBounds::default())?;
        let graph = SymbolGraph::new();
        let path = path("src/late.rs")?;
        let result = store.get_or_compute(
            &input(
                store.workspace(),
                &path,
                Revision(1),
                "fn late() {}",
                &graph,
            ),
            &OperationContext::unbounded(),
        );

        assert!(matches!(
            result,
            Err(LazyFactError::Aborted(OperationAbort::DeadlineExceeded))
        ));
        assert_eq!(store.stats().entries, 0);
        assert_eq!(store.stats().failures, 1);
        Ok(())
    }

    #[test]
    fn revision_change_invalidates_through_the_key() -> Result<(), Box<dyn std::error::Error>> {
        let producer = Arc::new(CountingProducer::default());
        let store = store(producer.clone(), FactStoreBounds::default())?;
        let graph = SymbolGraph::new();
        let path = path("src/lib.rs")?;
        let source = "fn stable() {}";
        let operation = OperationContext::unbounded();

        let r1 = store.get_or_compute(
            &input(store.workspace(), &path, Revision(1), source, &graph),
            &operation,
        )?;
        let r2 = store.get_or_compute(
            &input(store.workspace(), &path, Revision(2), source, &graph),
            &operation,
        )?;

        // Identical content, new revision: a miss, never a stale serve.
        assert_eq!(producer.calls(), 2);
        assert_eq!(r1.origin, FactOrigin::Computed);
        assert_eq!(r2.origin, FactOrigin::Computed);
        assert!(!Arc::ptr_eq(&r1.fact, &r2.fact));
        assert_eq!(r2.revision, Revision(2));

        // Content change within one revision also invalidates.
        let edited = store.get_or_compute(
            &input(
                store.workspace(),
                &path,
                Revision(2),
                "fn edited() {}",
                &graph,
            ),
            &operation,
        )?;
        assert_eq!(edited.origin, FactOrigin::Computed);
        assert_eq!(producer.calls(), 3);
        Ok(())
    }

    #[test]
    fn outcome_carries_the_producers_declared_provenance() -> Result<(), Box<dyn std::error::Error>>
    {
        let store = store(
            Arc::new(FileOutlineDigestProducer::new()),
            FactStoreBounds::default(),
        )?;
        let graph = SymbolGraph::new();
        let path = path("src/lib.rs")?;
        let outcome = store.get_or_compute(
            &input(
                store.workspace(),
                &path,
                Revision(1),
                "fn outlined() {}",
                &graph,
            ),
            &OperationContext::unbounded(),
        )?;
        assert_eq!(outcome.provenance, Provenance::TreeSitter);
        assert_eq!(outcome.precision, Precision::Syntax);
        Ok(())
    }

    #[test]
    fn outline_digest_renders_sorted_symbols_with_positions()
    -> Result<(), Box<dyn std::error::Error>> {
        let file = path("src/lib.rs")?;
        let source = "fn second() {}\nfn first() {}\n";
        let mut builder = BoundedGraphBuilder::new(GraphBuildLimits::UNLIMITED);
        builder.add_file(file.clone(), source)?;
        // Insert out of source order to prove position sorting.
        for (name, line, signature) in [
            ("second", 1_u32, "fn second()"),
            ("first", 2_u32, "fn first()"),
        ] {
            let start = TextPosition::new(line, 1)?;
            let end = TextPosition::new(line, 14)?;
            let range = SourceRange::new(file.clone(), start, end)?;
            builder.add_symbol(
                SymbolKey {
                    language: Language::Rust,
                    qualified_name: name.to_owned(),
                    container: None,
                    kind: SymbolKind::Function,
                    path: file.clone(),
                },
                range,
                Some(signature.to_owned()),
                Provenance::TreeSitter,
                Precision::Syntax,
            )?;
        }
        let (graph, _) = builder.finish();

        let store = store(
            Arc::new(FileOutlineDigestProducer::new()),
            FactStoreBounds::default(),
        )?;
        let outcome = store.get_or_compute(
            &input(store.workspace(), &file, Revision(1), source, &graph),
            &OperationContext::unbounded(),
        )?;

        let fact = outcome.fact.as_ref();
        assert_eq!(fact.symbol_count, 2);
        assert!(!fact.truncated);
        let lines: Vec<&str> = fact.digest.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Function second @ 1:1 fn second()"));
        assert!(lines[1].contains("Function first @ 2:1 fn first()"));
        assert!(fact.retained_bytes() >= fact.digest.len());
        Ok(())
    }

    #[test]
    fn outline_digest_counts_but_does_not_retain_symbols_beyond_its_line_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let file = path("src/large.rs")?;
        let mut builder = BoundedGraphBuilder::new(GraphBuildLimits::UNLIMITED);
        builder.add_file(file.clone(), "")?;
        for index in 0..=MAX_OUTLINE_LINES {
            let line = u32::try_from(index)?.saturating_add(1);
            let start = TextPosition::new(line, 1)?;
            let end = TextPosition::new(line, 2)?;
            builder.add_symbol(
                SymbolKey {
                    language: Language::Rust,
                    qualified_name: format!("symbol_{index}"),
                    container: None,
                    kind: SymbolKind::Function,
                    path: file.clone(),
                },
                SourceRange::new(file.clone(), start, end)?,
                None,
                Provenance::TreeSitter,
                Precision::Syntax,
            )?;
        }
        let (graph, _) = builder.finish();
        let store = store(
            Arc::new(FileOutlineDigestProducer::new()),
            FactStoreBounds::default(),
        )?;

        let outcome = store.get_or_compute(
            &input(store.workspace(), &file, Revision(1), "", &graph),
            &OperationContext::unbounded(),
        )?;

        assert_eq!(outcome.fact.symbol_count, MAX_OUTLINE_LINES + 1);
        assert!(outcome.fact.truncated);
        assert_eq!(outcome.fact.digest.lines().count(), MAX_OUTLINE_LINES);
        Ok(())
    }
}
