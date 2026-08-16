//! Live, optional rust-analyzer adapter.
//!
//! Only the v0.1 call-hierarchy operations cross this adapter internally.
//! Public contracts are Chakra-native, so LSP URIs, UTF-16 positions, and
//! protocol lifecycle details remain confined to this crate.

mod convert;
mod protocol;
mod worker;

use std::ffi::OsString;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_engine::{PreciseProvider, PreciseQueryRequest, PreciseQueryResult, ProviderWorkspace};
use crossbeam_channel::{Sender, bounded};
use thiserror::Error;

use crate::worker::Worker;

const DEFAULT_COMMAND_CAPACITY: usize = 8;
const DEFAULT_CACHE_CAPACITY: usize = 128;

/// Process and bounded-wait settings for the optional provider.
#[derive(Debug, Clone)]
pub struct RustAnalyzerConfig {
    pub executable: OsString,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub barrier_timeout: Duration,
    pub command_capacity: usize,
    pub cache_capacity: usize,
}

impl Default for RustAnalyzerConfig {
    fn default() -> Self {
        Self {
            executable: OsString::from("rust-analyzer"),
            startup_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(5),
            barrier_timeout: Duration::from_millis(750),
            command_capacity: DEFAULT_COMMAND_CAPACITY,
            cache_capacity: DEFAULT_CACHE_CAPACITY,
        }
    }
}

#[derive(Debug, Error)]
pub enum StartError {
    #[error("provider command and cache capacities must be non-zero")]
    InvalidCapacity,
    #[error("provider startup, request, and barrier timeouts must be non-zero")]
    InvalidTimeout,
    #[error("failed to spawn rust-analyzer owner thread: {0}")]
    ThreadSpawn(#[source] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ShutdownError {
    #[error("rust-analyzer owner thread panicked")]
    WorkerPanicked,
    #[error("rust-analyzer owner lock is poisoned")]
    LockPoisoned,
}

#[derive(Debug)]
struct SharedState {
    state: ProviderState,
    synced_revision: Option<Revision>,
    provider_epoch: u64,
    last_error: Option<String>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            state: ProviderState::Initializing,
            synced_revision: None,
            provider_epoch: 0,
            last_error: None,
        }
    }
}

enum Command {
    Enrich {
        request: Box<PreciseQueryRequest>,
        response: Sender<PreciseQueryResult>,
    },
}

/// Owned rust-analyzer process and worker lifecycle.
pub struct RustAnalyzerProvider {
    commands: Sender<Command>,
    shared: Arc<Mutex<SharedState>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    stopped: AtomicBool,
    force_stop: Arc<AtomicBool>,
    config: RustAnalyzerConfig,
}

impl fmt::Debug for RustAnalyzerProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RustAnalyzerProvider")
            .field("state", &self.state_snapshot())
            .field("stopped", &self.stopped.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl RustAnalyzerProvider {
    /// Starts the owner thread. A missing or failing executable does not fail
    /// Chakra startup: the handle transitions to `Degraded` and later queries
    /// retain syntax results.
    pub fn start(
        initial_workspace: ProviderWorkspace,
        config: RustAnalyzerConfig,
    ) -> Result<Arc<Self>, StartError> {
        if config.command_capacity == 0 || config.cache_capacity == 0 {
            return Err(StartError::InvalidCapacity);
        }
        if config.startup_timeout.is_zero()
            || config.request_timeout.is_zero()
            || config.barrier_timeout.is_zero()
        {
            return Err(StartError::InvalidTimeout);
        }
        let (commands, receiver) = bounded(config.command_capacity);
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let force_stop = Arc::new(AtomicBool::new(false));
        let worker_shared = shared.clone();
        let worker_stop = force_stop.clone();
        let worker_config = config.clone();
        let worker = thread::Builder::new()
            .name("chakra-rust-analyzer".to_owned())
            .spawn(move || {
                Worker::new(
                    receiver,
                    worker_shared,
                    worker_stop,
                    worker_config,
                    initial_workspace,
                )
                .run();
            })
            .map_err(StartError::ThreadSpawn)?;
        Ok(Arc::new(Self {
            commands,
            shared,
            worker: Mutex::new(Some(worker)),
            stopped: AtomicBool::new(false),
            force_stop,
            config,
        }))
    }

    pub fn last_error(&self) -> Option<String> {
        self.shared
            .lock()
            .ok()
            .and_then(|state| state.last_error.clone())
    }

    /// Idempotent cooperative shutdown followed by joining the owned worker.
    pub fn shutdown(&self) -> Result<(), ShutdownError> {
        self.stopped.store(true, Ordering::Release);
        self.force_stop.store(true, Ordering::Release);
        let worker = self
            .worker
            .lock()
            .map_err(|_| ShutdownError::LockPoisoned)?
            .take();
        if let Some(worker) = worker {
            worker.join().map_err(|_| ShutdownError::WorkerPanicked)?;
        }
        Ok(())
    }

    fn state_snapshot(&self) -> (ProviderState, Option<Revision>, u64) {
        self.shared
            .lock()
            .map_or((ProviderState::Degraded, None, 0), |state| {
                (state.state, state.synced_revision, state.provider_epoch)
            })
    }
}

impl PreciseProvider for RustAnalyzerProvider {
    fn state_for(&self, revision: Revision) -> ProviderState {
        if self.stopped.load(Ordering::Acquire) {
            return ProviderState::Degraded;
        }
        let (state, synced_revision, _) = self.state_snapshot();
        match state {
            ProviderState::Ready if synced_revision == Some(revision) => ProviderState::Ready,
            ProviderState::Ready | ProviderState::CatchingUp => ProviderState::CatchingUp,
            other => other,
        }
    }

    fn last_error(&self) -> Option<String> {
        RustAnalyzerProvider::last_error(self)
    }

    fn enrich(&self, request: PreciseQueryRequest) -> PreciseQueryResult {
        let revision = request.workspace.revision;
        if self.stopped.load(Ordering::Acquire) {
            return PreciseQueryResult::unavailable(revision, ProviderState::Degraded);
        }
        let (sender, receiver) = bounded(1);
        if self
            .commands
            .send_timeout(
                Command::Enrich {
                    request: Box::new(request),
                    response: sender,
                },
                self.config.barrier_timeout,
            )
            .is_err()
        {
            return PreciseQueryResult::unavailable(revision, ProviderState::CatchingUp);
        }
        // Cover initial startup, a lazy start, one transport restart, both
        // query attempts, and bounded cancellation/shutdown acknowledgements.
        let wait = self
            .config
            .startup_timeout
            .saturating_mul(3)
            .saturating_add(self.config.request_timeout.saturating_mul(2))
            .saturating_add(self.config.barrier_timeout.saturating_mul(4));
        receiver.recv_timeout(wait).unwrap_or_else(|_| {
            PreciseQueryResult::unavailable(revision, ProviderState::CatchingUp)
        })
    }
}

impl Drop for RustAnalyzerProvider {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
    use chakra_engine::{CallHierarchyDirections, ProviderDocument};
    use crossbeam_channel::bounded;
    use lsp_server::Notification;
    use serde_json::json;

    use crate::convert::{
        chakra_to_lsp_position, lsp_to_chakra_position, path_to_uri, uri_to_path,
    };
    use crate::worker::Worker;

    #[test]
    fn unicode_positions_round_trip_through_lsp_utf16() -> Result<(), Box<dyn std::error::Error>> {
        let source = "fn café🦀() {}\n";
        let chakra = TextPosition::new(1, 9)?;
        let lsp = chakra_to_lsp_position(source, chakra).ok_or("position conversion failed")?;
        assert_eq!(lsp.character, 9);
        assert_eq!(lsp_to_chakra_position(source, lsp), Some(chakra));
        Ok(())
    }

    #[test]
    fn path_uri_round_trip_is_repository_scoped() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let path = RepoRelativePath::new("src/lib.rs")?;
        let uri = path_to_uri(root.path(), &path)?;
        assert_eq!(uri_to_path(root.path(), &uri), Some(path));
        Ok(())
    }

    #[test]
    fn server_status_updates_revision_relative_lifecycle_without_sleeping()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let (_sender, commands) = bounded(1);
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let mut worker = Worker::new(
            commands,
            shared.clone(),
            Arc::new(AtomicBool::new(false)),
            RustAnalyzerConfig::default(),
            ProviderWorkspace {
                repository_root: root.path().to_path_buf(),
                revision: Revision(7),
                documents: Vec::new(),
            },
        );
        worker.handle_notification(Notification {
            method: "experimental/serverStatus".to_owned(),
            params: json!({ "health": "ok", "quiescent": true, "message": null }),
        });
        let state = shared.lock().map_err(|_| "shared state lock poisoned")?;
        assert_eq!(state.state, ProviderState::Ready);
        assert_eq!(state.synced_revision, Some(Revision(7)));
        Ok(())
    }

    #[test]
    fn quiescent_status_does_not_claim_unopened_documents_are_current()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let (_sender, commands) = bounded(1);
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let mut worker = Worker::new(
            commands,
            shared.clone(),
            Arc::new(AtomicBool::new(false)),
            RustAnalyzerConfig::default(),
            ProviderWorkspace {
                repository_root: root.path().to_path_buf(),
                revision: Revision(7),
                documents: vec![ProviderDocument {
                    path: RepoRelativePath::new("src/lib.rs")?,
                    source: Arc::from("fn target() {}\n"),
                }],
            },
        );
        worker.handle_notification(Notification {
            method: "experimental/serverStatus".to_owned(),
            params: json!({ "health": "ok", "quiescent": true, "message": null }),
        });
        let state = shared.lock().map_err(|_| "shared state lock poisoned")?;
        assert_eq!(state.state, ProviderState::CatchingUp);
        assert_eq!(state.synced_revision, None);
        Ok(())
    }

    #[test]
    fn missing_executable_degrades_without_global_provider()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let workspace = ProviderWorkspace {
            repository_root: root.path().to_path_buf(),
            revision: Revision(1),
            documents: vec![ProviderDocument {
                path: RepoRelativePath::new("src/lib.rs")?,
                source: Arc::from("fn target() {}\n"),
            }],
        };
        let provider = RustAnalyzerProvider::start(
            workspace.clone(),
            RustAnalyzerConfig {
                executable: OsString::from("chakra-definitely-missing-rust-analyzer"),
                request_timeout: Duration::from_millis(100),
                barrier_timeout: Duration::from_millis(50),
                ..RustAnalyzerConfig::default()
            },
        )?;
        let position = TextPosition::new(1, 1)?;
        let result = provider.enrich(PreciseQueryRequest {
            workspace,
            symbol: chakra_engine::ProviderSymbol {
                name: "target".to_owned(),
                declaration: SourceRange::new(
                    RepoRelativePath::new("src/lib.rs")?,
                    position,
                    TextPosition::new(1, 15)?,
                )?,
            },
            directions: CallHierarchyDirections {
                incoming: true,
                outgoing: false,
            },
            limit: 20,
        });
        assert_eq!(result.state, ProviderState::Degraded);
        provider.shutdown()?;
        Ok(())
    }
}
