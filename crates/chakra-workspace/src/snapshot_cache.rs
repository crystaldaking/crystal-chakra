//! Repository-scoped reuse of complete commit syntax indexes (issue #49).
//!
//! Process-local hits clone persistent graph/index state. Optional disk
//! artifacts use a full compatibility key and a checksummed language-owned
//! payload. Worktree overlays and provider enrichment never enter this cache.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use chakra_domain::composition::{
    CommitSnapshotOrigin, CommitSnapshotRejection, CommitSnapshotReuse,
};
use chakra_domain::identity::RepositoryId;
use chakra_domain::indexing::IndexCancellation;
use chakra_language::{
    CommitIndexProvider, CommitIndexReport, CommitSnapshotCompatibility,
    CommitSnapshotPayloadError, IndexOptions, WorkspaceIndexError, index_commit_with_options,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MANIFEST_FILE: &str = "manifest.json";
const PAYLOAD_FILE: &str = "snapshot.bin";
const ACCESS_FILE: &str = "access";
const ENTRIES_DIR: &str = "entries";
const MANIFEST_READ_LIMIT: u64 = 1024 * 1024;
const IO_CHUNK_BYTES: usize = 64 * 1024;
const WAIT_POLL: Duration = Duration::from_millis(10);

/// Bounds for process-local and optional on-disk commit snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSnapshotCacheConfig {
    pub directory: Option<PathBuf>,
    pub max_memory_entries: usize,
    pub max_disk_artifacts: usize,
    pub max_disk_bytes: u64,
    pub max_artifact_bytes: u64,
}

impl Default for CommitSnapshotCacheConfig {
    fn default() -> Self {
        Self {
            directory: None,
            max_memory_entries: 4,
            max_disk_artifacts: 16,
            max_disk_bytes: 1024 * 1024 * 1024,
            max_artifact_bytes: 512 * 1024 * 1024,
        }
    }
}

impl CommitSnapshotCacheConfig {
    pub fn with_directory(directory: PathBuf) -> Self {
        Self {
            directory: Some(directory),
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), CommitSnapshotCacheError> {
        if self.max_memory_entries == 0 {
            return Err(CommitSnapshotCacheError::InvalidConfig(
                "max_memory_entries must be greater than zero",
            ));
        }
        if self.directory.is_some()
            && (self.max_disk_artifacts == 0
                || self.max_disk_bytes == 0
                || self.max_artifact_bytes == 0
                || self.max_artifact_bytes > self.max_disk_bytes)
        {
            return Err(CommitSnapshotCacheError::InvalidConfig(
                "disk bounds must be non-zero and max_artifact_bytes must not exceed max_disk_bytes",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum CommitSnapshotCacheError {
    #[error("invalid commit snapshot cache configuration: {0}")]
    InvalidConfig(&'static str),
    #[error(transparent)]
    Index(#[from] WorkspaceIndexError),
    #[error("commit snapshot cache operation was cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompatibilityKey {
    repository: String,
    commit: Option<String>,
    compatibility: CommitSnapshotCompatibility,
}

impl CompatibilityKey {
    fn fingerprint(&self) -> Result<String, serde_json::Error> {
        let raw = serde_json::to_vec(self)?;
        Ok(blake3::hash(&raw).to_hex().to_string())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    key: CompatibilityKey,
    payload_bytes: u64,
}

struct DiskLookup<'a> {
    fingerprint: &'a str,
    expected: &'a CompatibilityKey,
    repository_root: &'a Path,
    commit: Option<&'a str>,
    budgets: chakra_domain::indexing::IndexBudgets,
    cancellation: &'a IndexCancellation,
}

#[derive(Debug, Clone)]
struct MemoryEntry {
    report: CommitIndexReport,
    last_used: u64,
}

#[derive(Debug, Default)]
struct CacheState {
    ready: HashMap<String, MemoryEntry>,
    building: HashSet<String>,
    clock: u64,
}

/// Bounded cache shared by every worktree runtime in one registry.
#[derive(Debug)]
pub struct CommitSnapshotCache {
    config: CommitSnapshotCacheConfig,
    state: Mutex<CacheState>,
    changed: Condvar,
}

impl CommitSnapshotCache {
    pub fn new(config: CommitSnapshotCacheConfig) -> Result<Self, CommitSnapshotCacheError> {
        config.validate()?;
        Ok(Self {
            config,
            state: Mutex::new(CacheState::default()),
            changed: Condvar::new(),
        })
    }

    pub fn config(&self) -> &CommitSnapshotCacheConfig {
        &self.config
    }

    /// Returns one compatible immutable commit index. Concurrent requests for
    /// the same key share a single builder; cancelled waiters stop promptly.
    pub fn load_or_build(
        &self,
        repository_root: &Path,
        repository: &RepositoryId,
        commit: Option<&str>,
        options: IndexOptions,
    ) -> Result<CommitIndexReport, CommitSnapshotCacheError> {
        let started = Instant::now();
        check(&options.cancellation)?;
        let budgets = options
            .budgets
            .validate()
            .map_err(WorkspaceIndexError::from)?;
        let key = CompatibilityKey {
            repository: repository.as_str().to_owned(),
            commit: commit.map(str::to_owned),
            compatibility: CommitSnapshotCompatibility::current(budgets),
        };
        let fingerprint = key.fingerprint().map_err(|_| {
            CommitSnapshotCacheError::InvalidConfig("compatibility key encoding failed")
        })?;

        let mut state = lock(&self.state);
        loop {
            let next_clock = state.clock.saturating_add(1);
            if let Some(entry) = state.ready.get_mut(&fingerprint) {
                entry.last_used = next_clock;
                let mut report = entry.report.clone_for_root(repository_root.to_path_buf());
                report.reuse = CommitSnapshotReuse {
                    origin: CommitSnapshotOrigin::MemoryReuse,
                    reused_files: report.source_files,
                    reused_source_bytes: report.source_bytes,
                    elapsed_micros: micros(started.elapsed()),
                    artifact_bytes: None,
                    rejection: None,
                };
                state.clock = next_clock;
                return Ok(report);
            }
            if state.building.insert(fingerprint.clone()) {
                break;
            }
            check(&options.cancellation)?;
            state = match self.changed.wait_timeout(state, WAIT_POLL) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        drop(state);

        let built = self.load_or_build_owner(
            repository_root,
            commit,
            &key,
            &fingerprint,
            options,
            started,
        );
        let mut state = lock(&self.state);
        state.building.remove(&fingerprint);
        if let Ok(report) = &built {
            state.clock = state.clock.saturating_add(1);
            let last_used = state.clock;
            state.ready.insert(
                fingerprint,
                MemoryEntry {
                    report: report.clone(),
                    last_used,
                },
            );
            evict_memory(&mut state, self.config.max_memory_entries);
        }
        self.changed.notify_all();
        built
    }

    fn load_or_build_owner(
        &self,
        repository_root: &Path,
        commit: Option<&str>,
        key: &CompatibilityKey,
        fingerprint: &str,
        options: IndexOptions,
        started: Instant,
    ) -> Result<CommitIndexReport, CommitSnapshotCacheError> {
        let (restored, mut rejection) = match self.config.directory.as_deref() {
            Some(directory) => self.read_disk(
                directory,
                DiskLookup {
                    fingerprint,
                    expected: key,
                    repository_root,
                    commit,
                    budgets: options.budgets,
                    cancellation: &options.cancellation,
                },
            ),
            None => (None, CommitSnapshotRejection::CacheDisabled),
        };
        if let Some((mut report, artifact_bytes)) = restored {
            report.reuse = CommitSnapshotReuse {
                origin: CommitSnapshotOrigin::DiskRestore,
                reused_files: report.source_files,
                reused_source_bytes: report.source_bytes,
                elapsed_micros: micros(started.elapsed()),
                artifact_bytes: Some(artifact_bytes),
                rejection: None,
            };
            return Ok(report);
        }

        check(&options.cancellation)?;
        let mut report = index_commit_with_options(repository_root, commit, options.clone())?;
        if let Some(directory) = self.config.directory.as_deref() {
            match self.write_disk(
                directory,
                fingerprint,
                key,
                &report,
                &options.cancellation,
                matches!(
                    rejection,
                    CommitSnapshotRejection::NotFound
                        | CommitSnapshotRejection::FormatMismatch
                        | CommitSnapshotRejection::CompatibilityMismatch
                        | CommitSnapshotRejection::Corrupt
                        | CommitSnapshotRejection::Oversized
                ),
            ) {
                Ok(artifact_bytes) => {
                    report.reuse.artifact_bytes = Some(artifact_bytes);
                }
                Err(DiskFailure::Cancelled) => return Err(CommitSnapshotCacheError::Cancelled),
                Err(DiskFailure::Rejected(reason)) => rejection = reason,
            }
        }
        report.reuse = CommitSnapshotReuse {
            origin: CommitSnapshotOrigin::ColdBuild,
            reused_files: 0,
            reused_source_bytes: 0,
            elapsed_micros: micros(started.elapsed()),
            artifact_bytes: report.reuse.artifact_bytes,
            rejection: Some(rejection),
        };
        Ok(report)
    }

    fn read_disk(
        &self,
        directory: &Path,
        lookup: DiskLookup<'_>,
    ) -> (Option<(CommitIndexReport, u64)>, CommitSnapshotRejection) {
        let artifact = artifact_path(directory, lookup.fingerprint);
        if !is_real_directory(&artifact) {
            return (None, CommitSnapshotRejection::NotFound);
        }
        let manifest_raw = match read_bounded(
            &artifact.join(MANIFEST_FILE),
            MANIFEST_READ_LIMIT,
            lookup.cancellation,
        ) {
            Ok(raw) => raw,
            Err(DiskFailure::Cancelled) => return (None, CommitSnapshotRejection::IoFailure),
            Err(DiskFailure::Rejected(reason)) => return (None, reason),
        };
        let manifest: Manifest = match serde_json::from_slice(&manifest_raw) {
            Ok(manifest) => manifest,
            Err(_) => return (None, CommitSnapshotRejection::Corrupt),
        };
        if manifest.key.compatibility.format_version != lookup.expected.compatibility.format_version
        {
            return (None, CommitSnapshotRejection::FormatMismatch);
        }
        if &manifest.key != lookup.expected {
            return (None, CommitSnapshotRejection::CompatibilityMismatch);
        }
        if manifest.payload_bytes > self.config.max_artifact_bytes {
            return (None, CommitSnapshotRejection::Oversized);
        }
        let payload = match read_bounded(
            &artifact.join(PAYLOAD_FILE),
            self.config.max_artifact_bytes.saturating_add(1),
            lookup.cancellation,
        ) {
            Ok(payload) => payload,
            Err(DiskFailure::Cancelled) => return (None, CommitSnapshotRejection::IoFailure),
            Err(DiskFailure::Rejected(reason)) => return (None, reason),
        };
        if payload.len() as u64 != manifest.payload_bytes {
            return (None, CommitSnapshotRejection::Corrupt);
        }
        let report = match CommitIndexReport::decode_snapshot(
            lookup.repository_root.to_path_buf(),
            lookup.commit,
            lookup.budgets,
            &payload,
            lookup.cancellation,
        ) {
            Ok(report) => report,
            Err(CommitSnapshotPayloadError::Cancelled) => {
                return (None, CommitSnapshotRejection::IoFailure);
            }
            Err(CommitSnapshotPayloadError::Format { .. }) => {
                return (None, CommitSnapshotRejection::FormatMismatch);
            }
            Err(CommitSnapshotPayloadError::Compatibility(_)) => {
                return (None, CommitSnapshotRejection::CompatibilityMismatch);
            }
            Err(CommitSnapshotPayloadError::Oversized { .. }) => {
                return (None, CommitSnapshotRejection::Oversized);
            }
            Err(_) => return (None, CommitSnapshotRejection::Corrupt),
        };
        touch_access(&artifact);
        (
            Some((report, artifact_bytes(&artifact))),
            CommitSnapshotRejection::NotFound,
        )
    }

    fn write_disk(
        &self,
        directory: &Path,
        fingerprint: &str,
        key: &CompatibilityKey,
        report: &CommitIndexReport,
        cancellation: &IndexCancellation,
        replace_existing: bool,
    ) -> Result<u64, DiskFailure> {
        check_disk(cancellation)?;
        let payload = report
            .encode_snapshot(cancellation)
            .map_err(|error| match error {
                CommitSnapshotPayloadError::Cancelled => DiskFailure::Cancelled,
                CommitSnapshotPayloadError::Oversized { .. } => {
                    DiskFailure::Rejected(CommitSnapshotRejection::Oversized)
                }
                _ => DiskFailure::Rejected(CommitSnapshotRejection::Corrupt),
            })?;
        if payload.len() as u64 > self.config.max_artifact_bytes {
            return Err(DiskFailure::Rejected(CommitSnapshotRejection::Oversized));
        }
        let entries = directory.join(ENTRIES_DIR);
        fs::create_dir_all(&entries)
            .map_err(|_| DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))?;
        let target = entries.join(fingerprint);
        if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(DiskFailure::Rejected(CommitSnapshotRejection::IoFailure));
        }
        if is_real_directory(&target) {
            if !replace_existing {
                return Ok(artifact_bytes(&target));
            }
            fs::remove_dir_all(&target)
                .map_err(|_| DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))?;
        }
        let manifest = Manifest {
            key: key.clone(),
            payload_bytes: payload.len() as u64,
        };
        let manifest_raw = serde_json::to_vec(&manifest)
            .map_err(|_| DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))?;
        let projected_bytes = (payload.len() as u64)
            .saturating_add(manifest_raw.len() as u64)
            .saturating_add(2);
        if projected_bytes > self.config.max_disk_bytes {
            return Err(DiskFailure::Rejected(CommitSnapshotRejection::Oversized));
        }
        let temporary = temporary_path(&entries, fingerprint)?;
        let result = (|| {
            write_synced(&temporary.join(PAYLOAD_FILE), &payload, cancellation)?;
            write_synced(&temporary.join(MANIFEST_FILE), &manifest_raw, cancellation)?;
            write_synced(&temporary.join(ACCESS_FILE), b"1\n", cancellation)?;
            check_disk(cancellation)?;
            match fs::rename(&temporary, &target) {
                Ok(()) => {}
                Err(_) if is_real_directory(&target) => {
                    let _ = fs::remove_dir_all(&temporary);
                }
                Err(_) => {
                    return Err(DiskFailure::Rejected(CommitSnapshotRejection::IoFailure));
                }
            }
            if let Err(error) = self.evict_disk(directory, Some(fingerprint), cancellation) {
                let _ = fs::remove_dir_all(&target);
                return Err(error);
            }
            Ok(artifact_bytes(&target))
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result
    }

    fn evict_disk(
        &self,
        directory: &Path,
        retained: Option<&str>,
        cancellation: &IndexCancellation,
    ) -> Result<(), DiskFailure> {
        let entries = directory.join(ENTRIES_DIR);
        let read_dir = match fs::read_dir(&entries) {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(DiskFailure::Rejected(CommitSnapshotRejection::IoFailure)),
        };
        let mut artifacts = Vec::new();
        for entry in read_dir {
            check_disk(cancellation)?;
            let entry =
                entry.map_err(|_| DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))?;
            let file_type = entry
                .file_type()
                .map_err(|_| DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !file_type.is_dir() || name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let bytes = artifact_bytes(&path);
            let modified = fs::metadata(path.join(ACCESS_FILE))
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            artifacts.push((name, path, bytes, modified));
        }
        artifacts.sort_by_key(|(_, _, _, modified)| *modified);
        let mut total_bytes = artifacts.iter().fold(0_u64, |total, (_, _, bytes, _)| {
            total.saturating_add(*bytes)
        });
        let mut total_count = artifacts.len();
        for (name, path, bytes, _) in artifacts {
            if total_count <= self.config.max_disk_artifacts
                && total_bytes <= self.config.max_disk_bytes
            {
                break;
            }
            if retained.is_some_and(|retained| retained == name) {
                continue;
            }
            if fs::remove_dir_all(&path).is_ok() {
                total_count = total_count.saturating_sub(1);
                total_bytes = total_bytes.saturating_sub(bytes);
            }
        }
        if total_count > self.config.max_disk_artifacts || total_bytes > self.config.max_disk_bytes
        {
            Err(DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))
        } else {
            Ok(())
        }
    }
}

impl CommitIndexProvider for CommitSnapshotCache {
    fn commit_index(
        &self,
        repository_root: &Path,
        repository: &RepositoryId,
        commit: Option<&str>,
        options: IndexOptions,
    ) -> Result<CommitIndexReport, WorkspaceIndexError> {
        self.load_or_build(repository_root, repository, commit, options)
            .map_err(|error| match error {
                CommitSnapshotCacheError::Index(error) => error,
                CommitSnapshotCacheError::Cancelled => WorkspaceIndexError::Cancelled,
                CommitSnapshotCacheError::InvalidConfig(message) => {
                    WorkspaceIndexError::Update(message.to_owned())
                }
            })
    }
}

#[derive(Debug)]
enum DiskFailure {
    Cancelled,
    Rejected(CommitSnapshotRejection),
}

fn check(cancellation: &IndexCancellation) -> Result<(), CommitSnapshotCacheError> {
    if cancellation.is_cancelled() {
        Err(CommitSnapshotCacheError::Cancelled)
    } else {
        Ok(())
    }
}

fn check_disk(cancellation: &IndexCancellation) -> Result<(), DiskFailure> {
    if cancellation.is_cancelled() {
        Err(DiskFailure::Cancelled)
    } else {
        Ok(())
    }
}

fn lock(state: &Mutex<CacheState>) -> MutexGuard<'_, CacheState> {
    match state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn evict_memory(state: &mut CacheState, limit: usize) {
    while state.ready.len() > limit {
        let Some(oldest) = state
            .ready
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        state.ready.remove(&oldest);
    }
}

fn artifact_path(directory: &Path, fingerprint: &str) -> PathBuf {
    directory.join(ENTRIES_DIR).join(fingerprint)
}

fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn temporary_path(entries: &Path, fingerprint: &str) -> Result<PathBuf, DiskFailure> {
    for nonce in 0_u32..128 {
        let path = entries.join(format!(".tmp-{}-{nonce}-{fingerprint}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(DiskFailure::Rejected(CommitSnapshotRejection::IoFailure)),
        }
    }
    Err(DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))
}

fn read_bounded(
    path: &Path,
    limit: u64,
    cancellation: &IndexCancellation,
) -> Result<Vec<u8>, DiskFailure> {
    let file = File::open(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => DiskFailure::Rejected(CommitSnapshotRejection::NotFound),
        _ => DiskFailure::Rejected(CommitSnapshotRejection::IoFailure),
    })?;
    if file
        .metadata()
        .map_err(|_| DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))?
        .len()
        > limit
    {
        return Err(DiskFailure::Rejected(CommitSnapshotRejection::Oversized));
    }
    let mut reader = file.take(limit.saturating_add(1));
    let mut output = Vec::new();
    let mut chunk = [0_u8; IO_CHUNK_BYTES];
    loop {
        check_disk(cancellation)?;
        let read = reader
            .read(&mut chunk)
            .map_err(|_| DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))?;
        if read == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..read]);
        if output.len() as u64 > limit {
            return Err(DiskFailure::Rejected(CommitSnapshotRejection::Oversized));
        }
    }
    Ok(output)
}

fn write_synced(
    path: &Path,
    bytes: &[u8],
    cancellation: &IndexCancellation,
) -> Result<(), DiskFailure> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))?;
    for chunk in bytes.chunks(IO_CHUNK_BYTES) {
        check_disk(cancellation)?;
        file.write_all(chunk)
            .map_err(|_| DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))?;
    }
    file.sync_all()
        .map_err(|_| DiskFailure::Rejected(CommitSnapshotRejection::IoFailure))
}

fn artifact_bytes(path: &Path) -> u64 {
    [MANIFEST_FILE, PAYLOAD_FILE, ACCESS_FILE]
        .into_iter()
        .filter_map(|file| fs::metadata(path.join(file)).ok())
        .fold(0_u64, |total, metadata| {
            total.saturating_add(metadata.len())
        })
}

fn touch_access(artifact: &Path) {
    for nonce in 0_u8..8 {
        let temporary = artifact.join(format!(".access-{}-{nonce}", std::process::id()));
        let Ok(mut file) = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        else {
            continue;
        };
        if file.write_all(b"1\n").is_ok() {
            let _ = file.sync_all();
            let _ = fs::rename(&temporary, artifact.join(ACCESS_FILE));
        }
        let _ = fs::remove_file(temporary);
        break;
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
