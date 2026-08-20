use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::symbol::Language;
use chakra_engine::{PreciseProvider, ProviderWorkspace};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct ProviderPoolConfig {
    pub max_active_providers: usize,
    pub max_reserved_memory_bytes: u64,
    pub max_concurrent_queries: usize,
    pub max_queued_queries: usize,
    pub query_queue_timeout: Duration,
    pub idle_timeout: Duration,
    pub idle_poll_interval: Duration,
    pub activation_backoff_base: Duration,
    pub activation_backoff_max: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for ProviderPoolConfig {
    fn default() -> Self {
        Self {
            max_active_providers: 3,
            max_reserved_memory_bytes: 2 * 1024 * 1024 * 1024,
            max_concurrent_queries: 4,
            max_queued_queries: 16,
            query_queue_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(5 * 60),
            idle_poll_interval: Duration::from_secs(5),
            activation_backoff_base: Duration::from_millis(250),
            activation_backoff_max: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderPoolConfigError {
    #[error("provider-pool limits and timeouts must be non-zero")]
    ZeroBound,
    #[error("activation backoff maximum must be at least its base")]
    InvalidBackoff,
    #[error("provider registration {provider} must declare at least one language")]
    NoLanguages { provider: String },
    #[error("provider registration {provider} must reserve non-zero memory")]
    ZeroReservation { provider: String },
    #[error(
        "provider registration {provider} reserves {reserved} bytes, above the pool maximum {maximum}"
    )]
    ReservationExceedsPool {
        provider: String,
        reserved: u64,
        maximum: u64,
    },
    #[error("language {language:?} is registered for both {first} and {second}")]
    LanguageConflict {
        language: Language,
        first: String,
        second: String,
    },
    #[error("provider {provider} is registered more than once")]
    DuplicateProvider { provider: String },
    #[error("provider {provider} registers language {language:?} more than once")]
    DuplicateLanguage {
        provider: String,
        language: Language,
    },
    #[error("failed to spawn provider-pool reaper: {0}")]
    ThreadSpawn(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderStartError {
    #[error("provider activation was cancelled")]
    Cancelled,
    #[error("provider activation exceeded its deadline")]
    DeadlineExceeded,
    #[error("{message}")]
    Failed { message: String },
}

impl ProviderStartError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }

    pub(crate) fn abort(&self) -> Option<OperationAbort> {
        match self {
            Self::Cancelled => Some(OperationAbort::Cancelled),
            Self::DeadlineExceeded => Some(OperationAbort::DeadlineExceeded),
            Self::Failed { .. } => None,
        }
    }
}

impl From<OperationAbort> for ProviderStartError {
    fn from(abort: OperationAbort) -> Self {
        match abort {
            OperationAbort::Cancelled => Self::Cancelled,
            OperationAbort::DeadlineExceeded => Self::DeadlineExceeded,
        }
    }
}

type ProviderFactory = dyn Fn(ProviderWorkspace, &OperationContext) -> Result<Arc<dyn PreciseProvider>, ProviderStartError>
    + Send
    + Sync;

pub struct ProviderRegistration {
    pub(crate) name: &'static str,
    pub(crate) languages: Vec<Language>,
    pub(crate) reserved_memory_bytes: u64,
    pub(crate) additional_wait_budget: Duration,
    pub(crate) factory: Arc<ProviderFactory>,
}

impl ProviderRegistration {
    pub fn new(
        name: &'static str,
        languages: Vec<Language>,
        reserved_memory_bytes: u64,
        factory: impl Fn(
            ProviderWorkspace,
            &OperationContext,
        ) -> Result<Arc<dyn PreciseProvider>, ProviderStartError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            name,
            languages,
            reserved_memory_bytes,
            additional_wait_budget: Duration::ZERO,
            factory: Arc::new(factory),
        }
    }

    /// Adds a declared activation/provider wait bound to the pool admission
    /// budget reported through the provider contract.
    pub fn with_additional_wait_budget(mut self, budget: Duration) -> Self {
        self.additional_wait_budget = budget;
        self
    }
}

impl fmt::Debug for ProviderRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRegistration")
            .field("name", &self.name)
            .field("languages", &self.languages)
            .field("reserved_memory_bytes", &self.reserved_memory_bytes)
            .field("additional_wait_budget", &self.additional_wait_budget)
            .finish_non_exhaustive()
    }
}
