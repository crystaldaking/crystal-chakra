//! Versioned codec for complete materialization-independent commit indexes.
//!
//! Unlike the rejected per-file cache from issue #39, this payload retains
//! each adapter's already materialized graph as well as the private facts
//! needed for future one-file reconciliation. Restore therefore performs no
//! Tree-sitter parse and no graph rebuild. Repository identity, commit, and
//! configuration remain outside the payload in the cache compatibility key;
//! all inputs are repeated and checked here as defense in depth.

use std::path::PathBuf;

use chakra_domain::indexing::{IndexBudgets, IndexCancellation};
use chakra_domain::project::ProjectModel;
use chakra_domain::symbol::Language;
use chakra_engine::SymbolGraph;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{CommitIndexReport, WorkspaceAdapterState, WorkspaceSyntaxIndex, check_cancelled};
use crate::adapter::{
    AdapterFrameworkMetrics, decode_snapshot_value, default_adapters, encode_snapshot_value,
};

pub const COMMIT_SNAPSHOT_FORMAT_VERSION: u32 = 1;
pub const COMMIT_SNAPSHOT_GRAPH_MODEL_VERSION: u32 = 1;

const MAGIC: [u8; 4] = *b"CKS1";
const CHECKSUM_BYTES: usize = 16;
const HEADER_BYTES: usize = MAGIC.len() + 4 + 8 + CHECKSUM_BYTES;
const MAX_SNAPSHOT_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSnapshotCompatibility {
    pub format_version: u32,
    pub graph_model_version: u32,
    pub chakra_language_version: String,
    pub budgets: IndexBudgets,
    pub adapters: Vec<(Language, String)>,
}

impl CommitSnapshotCompatibility {
    /// Compatibility expected by the current binary for one budget set,
    /// before any repository sources are read.
    pub fn current(budgets: IndexBudgets) -> Self {
        compatibility_from_defaults(budgets)
    }
}

#[derive(Debug, Error)]
pub enum CommitSnapshotPayloadError {
    #[error("commit snapshot operation was cancelled")]
    Cancelled,
    #[error("commit snapshot payload exceeds the {limit}-byte bound")]
    Oversized { limit: u64 },
    #[error("commit snapshot payload has an invalid envelope")]
    Envelope,
    #[error("commit snapshot payload checksum mismatch")]
    Checksum,
    #[error("commit snapshot format {found} does not match {expected}")]
    Format { expected: u32, found: u32 },
    #[error("commit snapshot compatibility mismatch: {0}")]
    Compatibility(&'static str),
    #[error("commit snapshot adapter set is invalid: {0}")]
    Adapters(String),
    #[error("commit snapshot codec failed: {0}")]
    Codec(String),
    #[error("commit snapshot graph is inconsistent: {0}")]
    Consistency(String),
}

#[derive(Serialize, Deserialize)]
struct AdapterPayload {
    language: Language,
    version: String,
    limits: chakra_engine::GraphBuildLimits,
    framework: AdapterFrameworkMetrics,
    payload: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotPayload {
    compatibility: CommitSnapshotCompatibility,
    commit: Option<String>,
    source_files: u64,
    source_bytes: u64,
    indexing: chakra_domain::indexing::IndexingStatus,
    project_model: ProjectModel,
    adapters: Vec<AdapterPayload>,
}

pub(super) fn compatibility(index: &WorkspaceSyntaxIndex) -> CommitSnapshotCompatibility {
    CommitSnapshotCompatibility {
        format_version: COMMIT_SNAPSHOT_FORMAT_VERSION,
        graph_model_version: COMMIT_SNAPSHOT_GRAPH_MODEL_VERSION,
        chakra_language_version: env!("CARGO_PKG_VERSION").to_owned(),
        budgets: index.budgets,
        adapters: index
            .adapters
            .iter()
            .map(|state| {
                (
                    state.adapter.language(),
                    state.adapter.snapshot_version().to_owned(),
                )
            })
            .collect(),
    }
}

pub(super) fn encode(
    report: &CommitIndexReport,
    cancellation: &IndexCancellation,
) -> Result<Vec<u8>, CommitSnapshotPayloadError> {
    check(cancellation)?;
    let mut adapters = Vec::with_capacity(report.syntax_index.adapters.len());
    for state in &report.syntax_index.adapters {
        check(cancellation)?;
        adapters.push(AdapterPayload {
            language: state.adapter.language(),
            version: state.adapter.snapshot_version().to_owned(),
            limits: state.limits,
            framework: state.framework,
            payload: state
                .adapter
                .encode_snapshot(cancellation)
                .map_err(|error| CommitSnapshotPayloadError::Codec(error.to_string()))?,
        });
    }
    let payload = SnapshotPayload {
        compatibility: compatibility(&report.syntax_index),
        commit: report.commit.clone(),
        source_files: report.source_files,
        source_bytes: report.source_bytes,
        indexing: report.indexing.clone(),
        project_model: report.syntax_index.project_model.clone(),
        adapters,
    };
    let body = encode_snapshot_value(&payload, cancellation)
        .map_err(|error| map_workspace_codec_error(error, cancellation))?;
    if !snapshot_size_within_bound(body.len()) {
        return Err(CommitSnapshotPayloadError::Oversized {
            limit: MAX_SNAPSHOT_BYTES as u64,
        });
    }
    check(cancellation)?;
    let checksum = blake3::hash(&body);
    let mut encoded = Vec::with_capacity(HEADER_BYTES.saturating_add(body.len()));
    encoded.extend_from_slice(&MAGIC);
    encoded.extend_from_slice(&COMMIT_SNAPSHOT_FORMAT_VERSION.to_le_bytes());
    encoded.extend_from_slice(&(body.len() as u64).to_le_bytes());
    encoded.extend_from_slice(&checksum.as_bytes()[..CHECKSUM_BYTES]);
    encoded.extend_from_slice(&body);
    Ok(encoded)
}

pub(super) fn decode(
    repository_root: PathBuf,
    expected_commit: Option<&str>,
    expected_budgets: IndexBudgets,
    encoded: &[u8],
    cancellation: &IndexCancellation,
) -> Result<CommitIndexReport, CommitSnapshotPayloadError> {
    check(cancellation)?;
    if encoded.len() < HEADER_BYTES || encoded.len() > MAX_SNAPSHOT_BYTES {
        return Err(CommitSnapshotPayloadError::Oversized {
            limit: MAX_SNAPSHOT_BYTES as u64,
        });
    }
    if encoded[..MAGIC.len()] != MAGIC {
        return Err(CommitSnapshotPayloadError::Envelope);
    }
    let version = u32::from_le_bytes(
        encoded[MAGIC.len()..MAGIC.len() + 4]
            .try_into()
            .map_err(|_| CommitSnapshotPayloadError::Envelope)?,
    );
    if version != COMMIT_SNAPSHOT_FORMAT_VERSION {
        return Err(CommitSnapshotPayloadError::Format {
            expected: COMMIT_SNAPSHOT_FORMAT_VERSION,
            found: version,
        });
    }
    let length_start = MAGIC.len() + 4;
    let body_len = u64::from_le_bytes(
        encoded[length_start..length_start + 8]
            .try_into()
            .map_err(|_| CommitSnapshotPayloadError::Envelope)?,
    );
    let body = encoded
        .get(HEADER_BYTES..)
        .ok_or(CommitSnapshotPayloadError::Envelope)?;
    if body_len != body.len() as u64 {
        return Err(CommitSnapshotPayloadError::Envelope);
    }
    let checksum_start = length_start + 8;
    if encoded[checksum_start..checksum_start + CHECKSUM_BYTES]
        != blake3::hash(body).as_bytes()[..CHECKSUM_BYTES]
    {
        return Err(CommitSnapshotPayloadError::Checksum);
    }
    let payload: SnapshotPayload = decode_snapshot_value(body, cancellation)
        .map_err(|error| map_workspace_codec_error(error, cancellation))?;
    let expected = compatibility_from_defaults(expected_budgets);
    validate_compatibility(&expected, &payload.compatibility)?;
    if payload.commit.as_deref() != expected_commit {
        return Err(CommitSnapshotPayloadError::Compatibility("commit"));
    }
    if payload.adapters.len() != expected.adapters.len() {
        return Err(CommitSnapshotPayloadError::Adapters(
            "adapter count mismatch".to_owned(),
        ));
    }

    let prototypes = default_adapters();
    let mut states = Vec::with_capacity(prototypes.len());
    for (prototype, stored) in prototypes.iter().zip(payload.adapters) {
        check(cancellation)?;
        if prototype.language() != stored.language || prototype.snapshot_version() != stored.version
        {
            return Err(CommitSnapshotPayloadError::Adapters(format!(
                "expected {:?}/{} but found {:?}/{}",
                prototype.language(),
                prototype.snapshot_version(),
                stored.language,
                stored.version
            )));
        }
        let adapter = prototype
            .decode_snapshot(&stored.payload, cancellation)
            .map_err(|error| map_workspace_codec_error(error, cancellation))?;
        states.push(WorkspaceAdapterState {
            adapter,
            limits: stored.limits,
            framework: stored.framework,
        });
    }
    check(cancellation)?;
    let graph = SymbolGraph::merge(states.iter().map(|state| state.adapter.graph().clone()))
        .map_err(|error| CommitSnapshotPayloadError::Consistency(error.to_string()))?;
    graph
        .audit_consistency()
        .map_err(|error| CommitSnapshotPayloadError::Consistency(error.to_string()))?;
    let syntax_index = WorkspaceSyntaxIndex {
        adapters: states,
        budgets: expected_budgets,
        indexing: payload.indexing.clone(),
        provider_inputs: Vec::new(),
        project_model: payload.project_model,
    };
    Ok(CommitIndexReport {
        commit: payload.commit,
        graph,
        indexing: payload.indexing,
        source_files: payload.source_files,
        source_bytes: payload.source_bytes,
        reuse: Default::default(),
        syntax_index,
        repository_root,
    })
}

fn compatibility_from_defaults(budgets: IndexBudgets) -> CommitSnapshotCompatibility {
    CommitSnapshotCompatibility {
        format_version: COMMIT_SNAPSHOT_FORMAT_VERSION,
        graph_model_version: COMMIT_SNAPSHOT_GRAPH_MODEL_VERSION,
        chakra_language_version: env!("CARGO_PKG_VERSION").to_owned(),
        budgets,
        adapters: default_adapters()
            .into_iter()
            .map(|adapter| (adapter.language(), adapter.snapshot_version().to_owned()))
            .collect(),
    }
}

fn snapshot_size_within_bound(body_len: usize) -> bool {
    HEADER_BYTES.saturating_add(body_len) <= MAX_SNAPSHOT_BYTES
}

fn validate_compatibility(
    expected: &CommitSnapshotCompatibility,
    actual: &CommitSnapshotCompatibility,
) -> Result<(), CommitSnapshotPayloadError> {
    if actual.format_version != expected.format_version {
        return Err(CommitSnapshotPayloadError::Compatibility("format_version"));
    }
    if actual.graph_model_version != expected.graph_model_version {
        return Err(CommitSnapshotPayloadError::Compatibility(
            "graph_model_version",
        ));
    }
    if actual.chakra_language_version != expected.chakra_language_version {
        return Err(CommitSnapshotPayloadError::Compatibility(
            "chakra_language_version",
        ));
    }
    if actual.budgets != expected.budgets {
        return Err(CommitSnapshotPayloadError::Compatibility("budgets"));
    }
    if actual.adapters != expected.adapters {
        return Err(CommitSnapshotPayloadError::Compatibility("adapters"));
    }
    Ok(())
}

fn check(cancellation: &IndexCancellation) -> Result<(), CommitSnapshotPayloadError> {
    check_cancelled(cancellation).map_err(|_| CommitSnapshotPayloadError::Cancelled)
}

fn map_workspace_codec_error(
    error: super::WorkspaceIndexError,
    cancellation: &IndexCancellation,
) -> CommitSnapshotPayloadError {
    if cancellation.is_cancelled() || matches!(error, super::WorkspaceIndexError::Cancelled) {
        CommitSnapshotPayloadError::Cancelled
    } else {
        CommitSnapshotPayloadError::Codec(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_covers_every_registered_adapter() {
        let compatibility = compatibility_from_defaults(IndexBudgets::default());
        assert_eq!(compatibility.adapters.len(), Language::ALL.len());
        assert!(
            compatibility
                .adapters
                .iter()
                .all(|(_, version)| version.ends_with(":s1"))
        );
    }

    #[test]
    fn snapshot_size_bound_includes_the_envelope_header() {
        assert!(snapshot_size_within_bound(
            MAX_SNAPSHOT_BYTES - HEADER_BYTES
        ));
        assert!(!snapshot_size_within_bound(
            MAX_SNAPSHOT_BYTES - HEADER_BYTES + 1
        ));
    }
}
