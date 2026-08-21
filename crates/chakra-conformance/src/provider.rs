//! Controllable `PreciseProvider` double for degradation/restart scenarios.
//!
//! The double never spawns a process and never talks to a real language
//! server. It records one failed start in its crashed (degraded) state;
//! `restart` records a second attempt and flips it to ready with
//! caller-supplied precise relations.
//!
//! Note on provenance: the double labels its precise facts
//! `Provenance::ChakraResolver` because the scenarios assert
//! degradation/restart *behavior* (precision upgrade and explicit
//! fallback), not the identity of a real language server.

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
    start_attempts: u64,
}

/// A `PreciseProvider` that first fails, then restarts on demand.
#[derive(Debug, Default)]
pub struct FlakyProvider {
    state: Mutex<FlakyState>,
}

impl FlakyProvider {
    /// A provider in the crashed state.
    pub fn crashed() -> Self {
        Self {
            state: Mutex::new(FlakyState {
                start_attempts: 1,
                ..FlakyState::default()
            }),
        }
    }

    /// Simulates a bounded restart and marks the provider healthy; subsequent
    /// `enrich` calls return `incoming` for the queried revision.
    pub fn restart(&self, incoming: Vec<PreciseRelation>) {
        let mut state = self.state();
        state.start_attempts = state.start_attempts.saturating_add(1);
        state.healthy = true;
        state.incoming = incoming;
    }

    pub fn start_attempts(&self) -> u64 {
        self.state().start_attempts
    }

    fn state(&self) -> MutexGuard<'_, FlakyState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PreciseProvider for FlakyProvider {
    fn name(&self) -> &'static str {
        "flaky-provider"
    }

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
            fallback_cause: None,
            incoming: state.incoming.clone(),
            outgoing: Vec::new(),
            incoming_truncated: false,
            outgoing_truncated: false,
        }
    }
}
