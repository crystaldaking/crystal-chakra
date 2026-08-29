use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use chakra_domain::operation::OperationContext;

use super::metrics::MetricsState;
use crate::indexer::{WorkspaceIndexError, WorkspaceSourceLoader};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileIdentity {
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

impl FileIdentity {
    pub(super) fn from_metadata(metadata: &fs::Metadata) -> Self {
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

    fn trustworthy_for_reuse(&self) -> bool {
        cfg!(unix)
    }
}

#[derive(Debug, Clone)]
pub(super) struct CachedSource {
    identity: FileIdentity,
    source: Arc<str>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct SourceSnapshotCache {
    pub(super) initialized: bool,
    pub(super) inventory: chakra_git::WorkspaceInventory,
    pub(super) entries: BTreeMap<chakra_domain::location::RepoRelativePath, CachedSource>,
}

pub(super) struct CachedSourceLoader<'a> {
    previous: &'a SourceSnapshotCache,
    pub(super) next: BTreeMap<chakra_domain::location::RepoRelativePath, CachedSource>,
    pub(super) observed: BTreeMap<chakra_domain::location::RepoRelativePath, FileIdentity>,
    pub(super) metadata_paths: BTreeSet<chakra_domain::location::RepoRelativePath>,
    force_full: bool,
    pub(super) files_read: u64,
    metrics: &'a MetricsState,
    operation: &'a OperationContext,
}

impl<'a> CachedSourceLoader<'a> {
    pub(super) fn new(
        previous: &'a SourceSnapshotCache,
        force_full: bool,
        metrics: &'a MetricsState,
        operation: &'a OperationContext,
    ) -> Self {
        Self {
            previous,
            next: BTreeMap::new(),
            observed: BTreeMap::new(),
            metadata_paths: BTreeSet::new(),
            force_full,
            files_read: 0,
            metrics,
            operation,
        }
    }

    fn inspect(&self, metadata: &fs::Metadata) {
        self.metrics.files_inspected.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .source_bytes_inspected
            .fetch_add(metadata.len(), Ordering::Relaxed);
    }
}

impl WorkspaceSourceLoader for CachedSourceLoader<'_> {
    fn observe(
        &mut self,
        path: &chakra_domain::location::RepoRelativePath,
        metadata: &fs::Metadata,
    ) {
        self.inspect(metadata);
        self.observed
            .insert(path.clone(), FileIdentity::from_metadata(metadata));
    }

    fn observe_metadata(
        &mut self,
        path: &chakra_domain::location::RepoRelativePath,
        metadata: &fs::Metadata,
    ) {
        self.metrics
            .metadata_files_inspected
            .fetch_add(1, Ordering::Relaxed);
        self.metrics
            .metadata_bytes_inspected
            .fetch_add(metadata.len(), Ordering::Relaxed);
        self.observed
            .insert(path.clone(), FileIdentity::from_metadata(metadata));
        self.metadata_paths.insert(path.clone());
    }

    fn load(
        &mut self,
        absolute: &Path,
        path: &chakra_domain::location::RepoRelativePath,
        metadata: &fs::Metadata,
        max_bytes: u64,
    ) -> Result<Arc<str>, WorkspaceIndexError> {
        self.operation
            .check()
            .map_err(|_| WorkspaceIndexError::Cancelled)?;
        let before = FileIdentity::from_metadata(metadata);
        let reused = !self.force_full
            && before.trustworthy_for_reuse()
            && self
                .previous
                .entries
                .get(path)
                .is_some_and(|cached| cached.identity == before);
        let source = if reused {
            self.previous
                .entries
                .get(path)
                .map(|cached| cached.source.clone())
                .ok_or_else(|| {
                    WorkspaceIndexError::Update(format!(
                        "source cache entry disappeared for `{path}`"
                    ))
                })?
        } else {
            let file = fs::File::open(absolute).map_err(|source| WorkspaceIndexError::Read {
                path: path.clone(),
                source,
            })?;
            let mut source = String::new();
            file.take(max_bytes.saturating_add(1))
                .read_to_string(&mut source)
                .map_err(|source| WorkspaceIndexError::Read {
                    path: path.clone(),
                    source,
                })?;
            self.files_read = self.files_read.saturating_add(1);
            self.metrics.files_read.fetch_add(1, Ordering::Relaxed);
            self.metrics
                .source_bytes_read
                .fetch_add(source.len() as u64, Ordering::Relaxed);
            Arc::<str>::from(source)
        };
        self.operation
            .check()
            .map_err(|_| WorkspaceIndexError::Cancelled)?;
        let after = if reused {
            before.clone()
        } else {
            let after_metadata =
                fs::metadata(absolute).map_err(|source| WorkspaceIndexError::Read {
                    path: path.clone(),
                    source,
                })?;
            self.inspect(&after_metadata);
            FileIdentity::from_metadata(&after_metadata)
        };
        if before != after {
            return Err(WorkspaceIndexError::Update(format!(
                "source `{path}` changed while its freshness snapshot was read"
            )));
        }
        self.next.insert(
            path.clone(),
            CachedSource {
                identity: after,
                source: source.clone(),
            },
        );
        Ok(source)
    }
}
