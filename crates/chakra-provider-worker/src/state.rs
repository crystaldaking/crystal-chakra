//! Shared command channel and published observability state.

use chakra_domain::operation::OperationContext;
use chakra_domain::query::{ProviderMetrics, ProviderProgress};
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_engine::{PreciseQueryRequest, PreciseQueryResult};
use crossbeam_channel::Sender;

/// One unit of work for the provider owner thread.
#[derive(Debug)]
pub enum ProviderCommand {
    Enrich {
        request: Box<PreciseQueryRequest>,
        operation: OperationContext,
        response: Sender<PreciseQueryResult>,
    },
}

/// Worker-published observability snapshot read by the adapter handle.
#[derive(Debug)]
pub struct SharedState {
    pub state: ProviderState,
    pub synced_revision: Option<Revision>,
    pub provider_epoch: u64,
    pub last_error: Option<String>,
    pub progress: Option<ProviderProgress>,
    pub metrics: ProviderMetrics,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            state: ProviderState::Initializing,
            synced_revision: None,
            provider_epoch: 0,
            last_error: None,
            progress: None,
            metrics: ProviderMetrics::default(),
        }
    }
}
