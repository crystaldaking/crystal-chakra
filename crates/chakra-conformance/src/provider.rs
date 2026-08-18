//! Controllable `PreciseProvider` double for degradation/recovery scenarios.
//!
//! The double never spawns a process and never talks to a real language
//! server. It starts in a crashed (degraded) state; `recover` flips it to
//! ready with caller-supplied precise relations.
//!
//! Note on provenance: the domain model's only precise-provider provenance
//! variant is `Provenance::RustAnalyzer`. The double borrows it for any
//! language because the scenarios assert degradation/recovery *behavior*
//! (precision upgrade and explicit fallback), not the provider's identity.

use std::sync::{Mutex, MutexGuard, PoisonError};

use chakra_domain::operation::OperationContext;
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{PreciseProvider, PreciseQueryRequest, PreciseQueryResult, PreciseRelation};

#[derive(Debug, Default)]
struct FlakyState {
    healthy: bool,
    incoming: Vec<PreciseRelation>,
}

/// A `PreciseProvider` that first fails, then recovers on demand.
#[derive(Debug, Default)]
pub struct FlakyProvider {
    state: Mutex<FlakyState>,
}

impl FlakyProvider {
    /// A provider in the crashed state.
    pub fn crashed() -> Self {
        Self::default()
    }

    /// Marks the provider healthy; subsequent `enrich` calls return
    /// `incoming` as precise caller relations for the queried revision.
    pub fn recover(&self, incoming: Vec<PreciseRelation>) {
        let mut state = self.state();
        state.healthy = true;
        state.incoming = incoming;
    }

    fn state(&self) -> MutexGuard<'_, FlakyState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PreciseProvider for FlakyProvider {
    fn supports(&self, _language: Language) -> bool {
        true
    }

    fn state_for(&self, _revision: Revision) -> ProviderState {
        if self.state().healthy {
            ProviderState::Ready
        } else {
            ProviderState::Degraded
        }
    }

    fn last_error(&self) -> Option<String> {
        if self.state().healthy {
            None
        } else {
            Some("simulated provider crash (conformance double)".to_owned())
        }
    }

    fn enrich_with_context(
        &self,
        request: PreciseQueryRequest,
        _operation: &OperationContext,
    ) -> PreciseQueryResult {
        let state = self.state();
        if !state.healthy {
            return PreciseQueryResult::unavailable(
                request.workspace.revision,
                ProviderState::Degraded,
            );
        }
        PreciseQueryResult {
            revision: request.workspace.revision,
            state: ProviderState::Ready,
            incoming: state.incoming.clone(),
            outgoing: Vec::new(),
            incoming_truncated: false,
            outgoing_truncated: false,
        }
    }
}
