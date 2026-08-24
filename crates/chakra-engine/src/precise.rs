//! Language-neutral contract for optional, query-time precise enrichment.
//!
//! The engine owns neither an LSP client nor provider-specific protocol
//! types. An adapter receives an immutable syntax workspace, synchronizes its
//! provider, and may return bounded precise relations for that exact
//! revision. A mismatch is treated as catching up by the query layer.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::provenance::Provenance;
use chakra_domain::query::{
    ProviderFallbackCause, ProviderMetrics, ProviderOrchestrationMetrics, ProviderProgress,
};
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;

use crate::{SymbolGraph, WorkspaceSnapshot};

/// One exact source document captured in a published syntax revision.
#[derive(Debug, Clone)]
pub struct ProviderDocument {
    pub path: RepoRelativePath,
    pub source: Arc<str>,
    pub language: Language,
}

/// One non-source workspace input captured in the same published revision as
/// the syntax graph. Providers receive path-level watched-file changes for
/// these inputs without reading mutable filesystem state through the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInput {
    pub path: RepoRelativePath,
    languages: Vec<Language>,
    identity: ProviderInputIdentity,
}

impl ProviderInput {
    /// Captures the exact filesystem identity already validated by the
    /// indexing/reconciliation owner. An empty language set is not a provider
    /// input and is rejected.
    pub fn from_metadata(
        path: RepoRelativePath,
        languages: impl IntoIterator<Item = Language>,
        metadata: &fs::Metadata,
    ) -> Option<Self> {
        let mut languages: Vec<_> = languages.into_iter().collect();
        languages.sort();
        languages.dedup();
        if languages.is_empty() {
            return None;
        }
        Some(Self {
            path,
            languages,
            identity: ProviderInputIdentity::from_metadata(metadata),
        })
    }

    /// Languages affected by this input.
    pub fn languages(&self) -> impl Iterator<Item = Language> + '_ {
        self.languages.iter().copied()
    }
}

/// Strong identity on Unix. Other platforms conservatively report a metadata
/// change on every new revision because their portable metadata does not
/// provide an equivalent change-time/inode proof.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderInputIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

impl ProviderInputIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.mode(),
            #[cfg(unix)]
            modified_seconds: metadata.mtime(),
            #[cfg(unix)]
            modified_nanoseconds: metadata.mtime_nsec(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    fn trustworthy_for_delta(&self) -> bool {
        cfg!(unix)
    }
}

#[derive(Debug, Clone)]
enum ProviderDocuments {
    Snapshot(Arc<SymbolGraph>),
    Owned(Arc<BTreeMap<RepoRelativePath, ProviderDocument>>),
}

/// Immutable input used to synchronize a live provider without giving it
/// access to mutable engine state.
#[derive(Debug, Clone)]
pub struct ProviderWorkspace {
    pub repository_root: PathBuf,
    pub revision: Revision,
    documents: ProviderDocuments,
    inputs: Arc<BTreeMap<RepoRelativePath, ProviderInput>>,
}

impl ProviderWorkspace {
    pub(crate) fn from_snapshot(snapshot: &WorkspaceSnapshot) -> Self {
        Self {
            repository_root: snapshot.identity().root.clone(),
            revision: snapshot.revision(),
            documents: ProviderDocuments::Snapshot(snapshot.graph_arc()),
            inputs: snapshot.provider_inputs_arc(),
        }
    }

    pub(crate) fn from_snapshot_with_context(
        snapshot: &WorkspaceSnapshot,
        operation: &OperationContext,
    ) -> Result<Self, OperationAbort> {
        operation.check()?;
        Ok(Self {
            repository_root: snapshot.identity().root.clone(),
            revision: snapshot.revision(),
            documents: ProviderDocuments::Snapshot(snapshot.graph_arc()),
            inputs: snapshot.provider_inputs_arc(),
        })
    }

    /// Builds a detached provider workspace for adapter contract tests or
    /// callers that do not own a [`WorkspaceSnapshot`]. Production query
    /// paths use the O(1) snapshot-backed constructor above.
    pub fn from_documents(
        repository_root: PathBuf,
        revision: Revision,
        documents: Vec<ProviderDocument>,
    ) -> Self {
        let documents = documents
            .into_iter()
            .map(|document| (document.path.clone(), document))
            .collect();
        Self {
            repository_root,
            revision,
            documents: ProviderDocuments::Owned(Arc::new(documents)),
            inputs: Arc::new(BTreeMap::new()),
        }
    }

    /// Builds a detached provider workspace with non-source inputs for
    /// adapter and freshness contract tests.
    pub fn from_documents_and_inputs(
        repository_root: PathBuf,
        revision: Revision,
        documents: Vec<ProviderDocument>,
        inputs: Vec<ProviderInput>,
    ) -> Self {
        let mut workspace = Self::from_documents(repository_root, revision, documents);
        workspace.inputs = Arc::new(
            inputs
                .into_iter()
                .map(|input| (input.path.clone(), input))
                .collect(),
        );
        workspace
    }

    pub fn document(&self, path: &RepoRelativePath) -> Option<ProviderDocument> {
        match &self.documents {
            ProviderDocuments::Snapshot(graph) => {
                let language = language_from_path(path.as_str())?;
                graph
                    .snapshot_document(path)
                    .map(|source| ProviderDocument {
                        path: path.clone(),
                        source,
                        language,
                    })
            }
            ProviderDocuments::Owned(documents) => documents.get(path).cloned(),
        }
    }

    pub fn document_count(&self, language: Language) -> usize {
        self.document_stats(language).0
    }

    pub fn document_bytes(&self, language: Language) -> u64 {
        self.document_stats(language).1
    }

    pub fn document_stats(&self, language: Language) -> (usize, u64) {
        self.document_stats_with_context(language, &OperationContext::unbounded())
            .unwrap_or((0, 0))
    }

    /// Unbounded [`Self::document_stats_with_context_matching`].
    pub fn document_stats_matching(&self, include: impl Fn(Language) -> bool) -> (usize, u64) {
        self.document_stats_with_context_matching(include, &OperationContext::unbounded())
            .unwrap_or((0, 0))
    }

    /// Unbounded [`Self::document_stats_with_context_matching_documents`].
    pub fn document_stats_matching_documents(
        &self,
        include: impl Fn(Language, &RepoRelativePath) -> bool,
    ) -> (usize, u64) {
        self.document_stats_with_context_matching_documents(include, &OperationContext::unbounded())
            .unwrap_or((0, 0))
    }

    pub fn document_stats_with_context(
        &self,
        language: Language,
        operation: &OperationContext,
    ) -> Result<(usize, u64), OperationAbort> {
        self.document_stats_with_context_matching(|candidate| candidate == language, operation)
    }

    /// Same statistics as [`Self::document_stats_with_context`] for a
    /// provider that natively covers more than one language (vtsls serves
    /// TypeScript and JavaScript through one session).
    pub fn document_stats_with_context_matching(
        &self,
        include: impl Fn(Language) -> bool,
        operation: &OperationContext,
    ) -> Result<(usize, u64), OperationAbort> {
        self.document_stats_with_context_matching_documents(
            |language, _path| include(language),
            operation,
        )
    }

    /// Same statistics as [`Self::document_stats_with_context_matching`],
    /// but the predicate sees each document's path as well (issue #86).
    pub fn document_stats_with_context_matching_documents(
        &self,
        include: impl Fn(Language, &RepoRelativePath) -> bool,
        operation: &OperationContext,
    ) -> Result<(usize, u64), OperationAbort> {
        Ok(self
            .document_catalog(operation)?
            .into_iter()
            .filter(|document| include(document.language, &document.path))
            .fold((0_usize, 0_u64), |(count, bytes), document| {
                (
                    count.saturating_add(1),
                    bytes.saturating_add(document.source.len() as u64),
                )
            }))
    }

    /// Exact revision delta for provider synchronization. Shared source
    /// allocations are the fast unchanged fingerprint. A replaced allocation
    /// is compared once by value to avoid resending an unchanged source after
    /// a conservative full filesystem checkpoint; no hash collision can hide
    /// a real edit.
    pub fn delta_since(
        &self,
        previous: &Self,
        language: Language,
        operation: &OperationContext,
    ) -> Result<ProviderWorkspaceDelta, OperationAbort> {
        self.delta_since_matching(previous, |candidate| candidate == language, operation)
    }

    /// Same delta as [`Self::delta_since`] for a provider that natively
    /// covers more than one language (vtsls serves TypeScript and
    /// JavaScript through one session).
    pub fn delta_since_matching(
        &self,
        previous: &Self,
        include: impl Fn(Language) -> bool,
        operation: &OperationContext,
    ) -> Result<ProviderWorkspaceDelta, OperationAbort> {
        self.delta_since_matching_documents(
            previous,
            |language, _path| include(language),
            operation,
        )
    }

    /// Same delta as [`Self::delta_since_matching`], but the predicate sees
    /// each document's path as well (terraform-ls must not receive Terraform
    /// JSON variants it cannot parse, issue #86).
    pub fn delta_since_matching_documents(
        &self,
        previous: &Self,
        include: impl Fn(Language, &RepoRelativePath) -> bool,
        operation: &OperationContext,
    ) -> Result<ProviderWorkspaceDelta, OperationAbort> {
        if self.shares_document_catalog_with(previous)
            && Arc::ptr_eq(&self.inputs, &previous.inputs)
        {
            operation.check()?;
            return Ok(ProviderWorkspaceDelta::default());
        }
        let mut delta = ProviderWorkspaceDelta::default();
        if !self.shares_document_catalog_with(previous) {
            let current = self.document_catalog(operation)?;
            let previous_documents = previous.document_catalog(operation)?;
            let mut current = current
                .into_iter()
                .filter(|document| include(document.language, &document.path))
                .peekable();
            let mut previous_documents = previous_documents
                .into_iter()
                .filter(|document| include(document.language, &document.path))
                .peekable();
            loop {
                operation.check()?;
                match (current.peek(), previous_documents.peek()) {
                    (Some(now), Some(before)) if now.path < before.path => {
                        if let Some(document) = current.next() {
                            delta.created.push(document);
                        }
                    }
                    (Some(now), Some(before)) if before.path < now.path => {
                        if let Some(document) = previous_documents.next() {
                            delta.deleted.push(document.path);
                        }
                    }
                    (Some(_), Some(_)) => {
                        let Some(now) = current.next() else {
                            break;
                        };
                        let Some(before) = previous_documents.next() else {
                            break;
                        };
                        delta.documents_examined = delta.documents_examined.saturating_add(1);
                        if !Arc::ptr_eq(&now.source, &before.source) {
                            delta.source_body_comparisons =
                                delta.source_body_comparisons.saturating_add(1);
                            if now.source.as_ref() != before.source.as_ref() {
                                delta.changed.push(now);
                            }
                        }
                    }
                    (Some(_), None) => {
                        delta.created.extend(current);
                        break;
                    }
                    (None, Some(_)) => {
                        delta
                            .deleted
                            .extend(previous_documents.map(|document| document.path));
                        break;
                    }
                    (None, None) => break,
                }
            }
            delta.documents_examined = delta
                .documents_examined
                .saturating_add(delta.created.len() as u64)
                .saturating_add(delta.deleted.len() as u64);
        }
        let current_inputs: Vec<_> = self
            .inputs
            .values()
            .filter(|input| {
                input
                    .languages()
                    .any(|language| include(language, &input.path))
            })
            .cloned()
            .collect();
        let previous_inputs: Vec<_> = previous
            .inputs
            .values()
            .filter(|input| {
                input
                    .languages()
                    .any(|language| include(language, &input.path))
            })
            .cloned()
            .collect();
        let mut current_inputs = current_inputs.into_iter().peekable();
        let mut previous_inputs = previous_inputs.into_iter().peekable();
        loop {
            operation.check()?;
            match (current_inputs.peek(), previous_inputs.peek()) {
                (Some(now), Some(before)) if now.path < before.path => {
                    if let Some(input) = current_inputs.next() {
                        delta.inputs_created.push(input.path);
                    }
                }
                (Some(now), Some(before)) if before.path < now.path => {
                    if let Some(input) = previous_inputs.next() {
                        delta.inputs_deleted.push(input.path);
                    }
                }
                (Some(_), Some(_)) => {
                    let Some(now) = current_inputs.next() else {
                        break;
                    };
                    let Some(before) = previous_inputs.next() else {
                        break;
                    };
                    delta.inputs_examined = delta.inputs_examined.saturating_add(1);
                    if !now.identity.trustworthy_for_delta()
                        || !before.identity.trustworthy_for_delta()
                        || now.identity != before.identity
                        || now.languages != before.languages
                    {
                        delta.inputs_changed.push(now.path);
                    }
                }
                (Some(_), None) => {
                    delta
                        .inputs_created
                        .extend(current_inputs.map(|input| input.path));
                    break;
                }
                (None, Some(_)) => {
                    delta
                        .inputs_deleted
                        .extend(previous_inputs.map(|input| input.path));
                    break;
                }
                (None, None) => break,
            }
        }
        delta.inputs_examined = delta
            .inputs_examined
            .saturating_add(delta.inputs_created.len() as u64)
            .saturating_add(delta.inputs_deleted.len() as u64);
        operation.check()?;
        Ok(delta)
    }

    fn shares_document_catalog_with(&self, other: &Self) -> bool {
        match (&self.documents, &other.documents) {
            (ProviderDocuments::Snapshot(left), ProviderDocuments::Snapshot(right)) => {
                Arc::ptr_eq(left, right)
            }
            (ProviderDocuments::Owned(left), ProviderDocuments::Owned(right)) => {
                Arc::ptr_eq(left, right)
            }
            _ => false,
        }
    }

    fn document_catalog(
        &self,
        operation: &OperationContext,
    ) -> Result<Vec<ProviderDocument>, OperationAbort> {
        match &self.documents {
            ProviderDocuments::Snapshot(graph) => Ok(graph
                .snapshot_documents_with_context(operation)?
                .into_iter()
                .filter_map(|(path, source)| {
                    let language = language_from_path(path.as_str())?;
                    Some(ProviderDocument {
                        path,
                        source,
                        language,
                    })
                })
                .collect()),
            ProviderDocuments::Owned(documents) => {
                let mut catalog = Vec::with_capacity(documents.len());
                for (index, document) in documents.values().enumerate() {
                    if index % 256 == 0 {
                        operation.check()?;
                    }
                    catalog.push(document.clone());
                }
                operation.check()?;
                Ok(catalog)
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProviderWorkspaceDelta {
    pub created: Vec<ProviderDocument>,
    pub changed: Vec<ProviderDocument>,
    pub deleted: Vec<RepoRelativePath>,
    pub documents_examined: u64,
    pub source_body_comparisons: u64,
    pub inputs_created: Vec<RepoRelativePath>,
    pub inputs_changed: Vec<RepoRelativePath>,
    pub inputs_deleted: Vec<RepoRelativePath>,
    pub inputs_examined: u64,
}

/// Syntax declaration selected by the caller before precise enrichment.
#[derive(Debug, Clone)]
pub struct ProviderSymbol {
    pub name: String,
    pub declaration: SourceRange,
    pub language: Language,
}

/// The two bounded call-hierarchy directions needed by v0.1 queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallHierarchyDirections {
    pub incoming: bool,
    pub outgoing: bool,
}

/// Adapter-neutral request. No LSP positions, URIs, or protocol enums cross
/// this boundary.
#[derive(Debug, Clone)]
pub struct PreciseQueryRequest {
    pub workspace: ProviderWorkspace,
    pub symbol: ProviderSymbol,
    pub directions: CallHierarchyDirections,
    pub limit: usize,
    pub priority: ProviderRequestPriority,
}

/// Admission priority used by the bounded multi-provider scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ProviderRequestPriority {
    Background,
    #[default]
    Normal,
    Interactive,
}

/// One provider-confirmed relationship endpoint and optional call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreciseRelation {
    pub name: String,
    pub declaration: SourceRange,
    /// Total provider-reported calls represented by this relationship.
    pub occurrence_count: u64,
    /// Bounded representative provider-reported call-site ranges.
    pub call_sites: Vec<SourceRange>,
    pub provenance: Provenance,
}

/// A bounded result explicitly tied to the syntax revision it enriches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreciseQueryResult {
    pub revision: Revision,
    pub state: ProviderState,
    pub fallback_cause: Option<ProviderFallbackCause>,
    pub incoming: Vec<PreciseRelation>,
    pub outgoing: Vec<PreciseRelation>,
    pub incoming_truncated: bool,
    pub outgoing_truncated: bool,
}

impl PreciseQueryResult {
    /// Honest syntax fallback when the provider cannot prove currency.
    pub fn unavailable(revision: Revision, state: ProviderState) -> Self {
        Self {
            revision,
            state,
            fallback_cause: None,
            incoming: Vec::new(),
            outgoing: Vec::new(),
            incoming_truncated: false,
            outgoing_truncated: false,
        }
    }

    pub fn unavailable_because(
        revision: Revision,
        state: ProviderState,
        cause: ProviderFallbackCause,
    ) -> Self {
        Self {
            fallback_cause: Some(cause),
            ..Self::unavailable(revision, state)
        }
    }
}

/// Optional precise-provider adapter installed for one active workspace.
pub trait PreciseProvider: std::fmt::Debug + Send + Sync {
    /// Stable operator-facing adapter name reported by `status` and by
    /// provider query info.
    fn name(&self) -> &'static str;

    /// Whether this adapter can enrich symbols in `language`.
    fn supports(&self, language: Language) -> bool;

    /// State relative to a specific published syntax revision.
    fn state_for(&self, revision: Revision) -> ProviderState;

    /// Bounded operator-facing reason for the current degraded/catching-up
    /// state, when the adapter has one.
    fn last_error(&self) -> Option<String> {
        None
    }

    /// Current provider/lifecycle progress. The source field distinguishes
    /// direct protocol facts from Chakra inference.
    fn progress(&self) -> Option<ProviderProgress> {
        None
    }

    /// Bounded provider-owned cache and synchronization instrumentation.
    /// These counters are strictly provider-local; workspace-global pool
    /// lifecycle/admission counters are reported via `orchestration_metrics`.
    fn metrics(&self) -> Option<ProviderMetrics> {
        None
    }

    /// Workspace-global provider-pool lifecycle and admission counters, when
    /// this adapter belongs to a shared pool. Pooled adapters return the same
    /// shared snapshot; the query layer reports it once (issue #61).
    fn orchestration_metrics(&self) -> Option<ProviderOrchestrationMetrics> {
        None
    }

    /// Maximum time one query is willing to wait for optional precision.
    fn query_wait_budget(&self) -> Option<Duration> {
        None
    }

    /// Idempotently stops provider-owned workers and child processes.
    fn shutdown(&self) -> Result<(), ProviderShutdownError> {
        Ok(())
    }

    /// Lazily enrich one selected symbol. Implementations must bound waiting
    /// and return `CatchingUp`/`Degraded` rather than stale precise facts.
    fn enrich(&self, request: PreciseQueryRequest) -> PreciseQueryResult {
        self.enrich_with_context(request, &OperationContext::unbounded())
    }

    fn enrich_with_context(
        &self,
        request: PreciseQueryRequest,
        operation: &OperationContext,
    ) -> PreciseQueryResult;
}

/// Adapter-neutral provider shutdown failure retained at the orchestration
/// boundary without leaking provider-specific error types.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ProviderShutdownError {
    message: String,
}

impl ProviderShutdownError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

fn language_from_path(path: &str) -> Option<Language> {
    if path.ends_with(".tf") || path.ends_with(".tfvars") || path.ends_with(".hcl") {
        return Some(Language::Hcl);
    }
    match std::path::Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
    {
        Some("rs") => Some(Language::Rust),
        Some("php") => Some(Language::Php),
        Some("ts" | "tsx" | "mts" | "cts") => Some(Language::TypeScript),
        Some("py" | "pyi") => Some(Language::Python),
        Some("js" | "jsx" | "mjs" | "cjs") => Some(Language::JavaScript),
        Some("java") => Some(Language::Java),
        Some("cs") => Some(Language::CSharp),
        Some("sh" | "bash" | "zsh" | "ksh") => Some(Language::Shell),
        Some("go") => Some(Language::Go),
        Some("c" | "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" | "ipp" | "tpp" | "inc") => {
            Some(Language::Cpp)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use chakra_domain::identity::WorkspaceIdentity;

    use super::*;
    use crate::WorkspaceEngine;

    #[test]
    fn snapshot_backed_workspaces_retain_csharp_shell_cpp_hcl_and_go_documents()
    -> Result<(), Box<dyn Error>> {
        let root = std::env::current_dir()?;
        let engine = WorkspaceEngine::new(WorkspaceIdentity::for_primary_worktree(&root)?);
        let mut update = engine.begin_update();
        update.graph_mut().add_file(
            RepoRelativePath::new("src/Program.cs")?,
            "class Program {}\n",
        )?;
        update.graph_mut().add_file(
            RepoRelativePath::new("scripts/release.sh")?,
            "release() { true; }\n",
        )?;
        update.graph_mut().add_file(
            RepoRelativePath::new("src/payment.cpp")?,
            "int payment() { return 1; }\n",
        )?;
        update
            .graph_mut()
            .add_file(RepoRelativePath::new("infra/main.tf")?, "terraform {}\n")?;
        update
            .graph_mut()
            .add_file(RepoRelativePath::new("service.go")?, "package service\n")?;
        engine.publish(update)?;

        let workspace = ProviderWorkspace::from_snapshot(&engine.snapshot());
        assert_eq!(workspace.document_count(Language::CSharp), 1);
        assert_eq!(workspace.document_count(Language::Shell), 1);
        assert_eq!(workspace.document_count(Language::Cpp), 1);
        assert_eq!(workspace.document_count(Language::Hcl), 1);
        assert_eq!(workspace.document_count(Language::Go), 1);
        assert!(
            workspace
                .document(&RepoRelativePath::new("src/Program.cs")?)
                .is_some()
        );
        assert!(
            workspace
                .document(&RepoRelativePath::new("scripts/release.sh")?)
                .is_some()
        );
        assert!(
            workspace
                .document(&RepoRelativePath::new("src/payment.cpp")?)
                .is_some()
        );
        assert!(
            workspace
                .document(&RepoRelativePath::new("service.go")?)
                .is_some()
        );
        assert!(
            workspace
                .document(&RepoRelativePath::new("infra/main.tf")?)
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn metadata_only_delta_is_language_scoped_and_revision_exact() -> Result<(), Box<dyn Error>> {
        let root = std::env::current_dir()?;
        let cargo_toml = fs::metadata(root.join("Cargo.toml"))?;
        let lib_rs = fs::metadata(root.join("src/lib.rs"))?;
        let document = ProviderDocument {
            path: RepoRelativePath::new("src/lib.rs")?,
            source: Arc::<str>::from("pub fn chakra() {}\n"),
            language: Language::Rust,
        };
        let before = ProviderWorkspace::from_documents_and_inputs(
            root.clone(),
            Revision(1),
            vec![document],
            vec![
                ProviderInput::from_metadata(
                    RepoRelativePath::new("Cargo.toml")?,
                    [Language::Rust],
                    &cargo_toml,
                )
                .ok_or("Rust provider input missing")?,
            ],
        );
        let mut after = before.clone();
        after.revision = Revision(2);
        after.inputs = Arc::new(
            vec![
                ProviderInput::from_metadata(
                    RepoRelativePath::new("Cargo.toml")?,
                    [Language::Rust],
                    &lib_rs,
                )
                .ok_or("changed Rust provider input missing")?,
                ProviderInput::from_metadata(
                    RepoRelativePath::new("pyproject.toml")?,
                    [Language::Python],
                    &cargo_toml,
                )
                .ok_or("Python provider input missing")?,
            ]
            .into_iter()
            .map(|input| (input.path.clone(), input))
            .collect(),
        );

        let rust = after.delta_since(&before, Language::Rust, &OperationContext::unbounded())?;
        assert!(rust.created.is_empty());
        assert!(rust.changed.is_empty());
        assert!(rust.deleted.is_empty());
        assert_eq!(rust.documents_examined, 0);
        assert_eq!(rust.source_body_comparisons, 0);
        assert_eq!(rust.inputs_changed, [RepoRelativePath::new("Cargo.toml")?]);
        assert!(rust.inputs_created.is_empty());
        assert!(rust.inputs_deleted.is_empty());

        let python =
            after.delta_since(&before, Language::Python, &OperationContext::unbounded())?;
        assert_eq!(
            python.inputs_created,
            [RepoRelativePath::new("pyproject.toml")?]
        );
        assert!(python.inputs_changed.is_empty());
        Ok(())
    }
}
