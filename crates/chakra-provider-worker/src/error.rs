//! Provider error surface shared by all worker-backed adapters.

use chakra_domain::state::ProviderState;
use chakra_lsp::ClientError;

/// Bounded worker failure. Transport/timeout/cancellation semantics are
/// language-neutral; the provider name is attached at the raise site so
/// operator messages stay specific without per-crate error enums.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("{0}")]
    Transport(String),
    #[error("timed out waiting for the provider response")]
    Timeout,
    #[error("the provider request was cancelled by its caller")]
    Cancelled,
    #[error("provider request failed ({code}): {message}")]
    Server { code: i32, message: String },
    #[error("invalid provider response: {0}")]
    InvalidResponse(#[from] serde_json::Error),
    #[error("provider does not advertise the required capability: {0}")]
    Unsupported(String),
    #[error("invalid file URI for {0}")]
    InvalidUri(String),
    #[error("provider position is outside captured source")]
    InvalidPosition,
}

impl WorkerError {
    pub(crate) fn transport(provider: &str, error: impl std::fmt::Display) -> Self {
        Self::Transport(format!("{provider} transport failed: {error}"))
    }

    pub(crate) fn from_client(provider: &str, error: ClientError) -> Self {
        match error {
            ClientError::Timeout { .. } => Self::Timeout,
            ClientError::Cancelled { .. } => Self::Cancelled,
            ClientError::Server { code, message, .. } => Self::Server { code, message },
            other => Self::transport(provider, other),
        }
    }

    pub(crate) fn is_transport_failure(&self) -> bool {
        matches!(self, Self::Transport(_))
    }

    pub(crate) fn fallback_state(&self) -> ProviderState {
        match self {
            Self::Timeout | Self::Cancelled => ProviderState::CatchingUp,
            _ => ProviderState::Degraded,
        }
    }
}
