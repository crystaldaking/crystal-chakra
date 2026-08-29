//! On-disk syntax fact cache: manifest, per-file fact payloads, atomic
//! publication, and bounded reads (issue #39).
//!
//! Layout inside the configured directory:
//!
//! - `manifest.bin` — sealed compatibility key plus one `(path, content
//!   hash, fact file)` entry per cached source file;
//! - `facts/<blake3(path)>.bin` — one sealed [`FileSyntaxFacts`] payload per
//!   source file.
//!
//! Publication is atomic: fact payloads are written to temporary files and
//! renamed, and the manifest — the commit point — is renamed last. A crash
//! anywhere leaves either the previous manifest (fully usable) or orphaned
//! fact files (never referenced, garbage-collected on the next publish).
//! Corruption of one fact file invalidates only that file (a reparse miss);
//! corruption of the manifest invalidates the whole cache (a deterministic
//! rebuild). The cache is always an optimization: every failure mode falls
//! back to deterministic parsing.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use chakra_domain::indexing::IndexBudgets;
use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;
use chakra_domain::symbol::Language;
use thiserror::Error;

use super::codec::{self, CodecError, GRAPH_MODEL_VERSION, fact_file_name};
use super::facts::FileSyntaxFacts;

/// Manifest file name inside the cache directory.
const MANIFEST_FILE: &str = "manifest.bin";
/// Fact payloads live in this subdirectory.
const FACTS_DIR: &str = "facts";
/// Upper bound on the manifest read: a fixed envelope plus a bounded
/// per-entry allowance.
const MANIFEST_ENTRY_ALLOWANCE_BYTES: u64 = 320;
const MANIFEST_BASE_BYTES: u64 = 1024 * 1024;

/// Default restore/write gate (budget B1): below roughly 1,000 indexed
/// files a deterministic rebuild is already fast and strictly simpler, so
/// the cache stays off.
pub const DEFAULT_MIN_INDEXED_FILES: u64 = 1_000;
/// Default bounds for the on-disk cache.
pub const DEFAULT_MAX_ENTRIES: u64 = 200_000;
pub const DEFAULT_MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024;
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// Typed configuration of the per-file syntax fact cache. The cache is
/// opt-in and bounded; below `min_indexed_files` it is neither read nor
/// written (budget B1 gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxCacheConfig {
    /// Directory holding the manifest and fact payloads. One directory per
    /// worktree; the caller chooses the location (the Git-aware
    /// administrative directory or an explicit tool path).
    pub directory: PathBuf,
    /// Cache is active only above this many indexed files.
    pub min_indexed_files: u64,
    /// Maximum number of cached files per manifest.
    pub max_entries: u64,
    /// Maximum encoded bytes of one fact payload; larger files are skipped
    /// (they become permanent misses, never a failure).
    pub max_entry_bytes: u64,
    /// Maximum total fact payload bytes per published manifest.
    pub max_total_bytes: u64,
}

impl SyntaxCacheConfig {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            min_indexed_files: DEFAULT_MIN_INDEXED_FILES,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_entry_bytes: DEFAULT_MAX_ENTRY_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

/// Cache mode of one indexing run. `Disabled` is the default: indexing
/// never touches the disk unless the caller explicitly enables the cache.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SyntaxCacheMode {
    #[default]
    Disabled,
    Enabled(SyntaxCacheConfig),
}

impl SyntaxCacheMode {
    pub fn config(&self) -> Option<&SyntaxCacheConfig> {
        match self {
            Self::Disabled => None,
            Self::Enabled(config) => Some(config),
        }
    }
}

/// Everything a cache must match before its facts are trusted (SPEC §14):
/// repository identity, commit, index format version (payload header),
/// graph model version, extractor versions per language, the Chakra
/// version, and the indexing configuration fingerprint. Per-file content
/// hashes live in the manifest entries and gate freshness per file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibilityKey {
    pub graph_model_version: u32,
    pub repository: String,
    /// HEAD commit at index time; empty for an unborn repository.
    pub head_sha: String,
    /// Version of the `chakra-language` crate that wrote the cache.
    pub chakra_version: String,
    pub config_fingerprint: [u8; 16],
    /// Per-language extractor versions, sorted by language tag.
    pub extractors: Vec<(Language, String)>,
}

impl CompatibilityKey {
    /// Resolves the key for the current worktree and configuration.
    pub fn resolve(
        repository_root: &Path,
        budgets: &IndexBudgets,
        extractors: Vec<(Language, String)>,
    ) -> Result<Self, CacheError> {
        let identity = chakra_git::resolve_workspace_identity(repository_root)
            .map_err(|error| CacheError::Git(error.to_string()))?;
        let operation = OperationContext::unbounded();
        let head_sha = chakra_git::resolve_head_commit_with_context(repository_root, &operation)
            .map_err(|error| CacheError::Git(error.to_string()))?
            .unwrap_or_default();
        let mut extractors = extractors;
        extractors.sort_by_key(|(language, _)| codec::language_tag(*language));
        Ok(Self {
            graph_model_version: GRAPH_MODEL_VERSION,
            repository: identity.repository.as_str().to_owned(),
            head_sha,
            chakra_version: env!("CARGO_PKG_VERSION").to_owned(),
            config_fingerprint: config_fingerprint(budgets),
            extractors,
        })
    }

    /// The first mismatching input, if any. A mismatch is never an error:
    /// it means the cache belongs to a different world and the caller runs
    /// a deterministic rebuild.
    pub fn mismatch(&self, other: &Self) -> Option<&'static str> {
        if self.graph_model_version != other.graph_model_version {
            return Some("graph_model_version");
        }
        if self.repository != other.repository {
            return Some("repository");
        }
        if self.head_sha != other.head_sha {
            return Some("head_sha");
        }
        if self.chakra_version != other.chakra_version {
            return Some("chakra_version");
        }
        if self.config_fingerprint != other.config_fingerprint {
            return Some("config_fingerprint");
        }
        if self.extractors != other.extractors {
            return Some("extractors");
        }
        None
    }
}

/// Fingerprint of the indexing configuration that affects graph contents.
pub fn config_fingerprint(budgets: &IndexBudgets) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    for value in [
        budgets.max_files,
        budgets.max_source_file_bytes,
        budgets.max_workspace_source_bytes,
        budgets.max_symbols,
        budgets.max_edges,
        budgets.max_call_sites,
    ] {
        hasher.update(&value.to_le_bytes());
    }
    let digest = hasher.finalize();
    let mut fingerprint = [0_u8; 16];
    fingerprint.copy_from_slice(&digest.as_bytes()[..16]);
    fingerprint
}

/// Content hash of one retained source (128-bit BLAKE3). The fixed-key
/// SipHash of the issue #38 benchmark model is deliberately not reused:
/// cache hits must be collision-safe because a wrong hit would silently
/// substitute facts of a different file.
pub fn content_hash(source: &str) -> [u8; 16] {
    let digest = blake3::hash(source.as_bytes());
    let mut hash = [0_u8; 16];
    hash.copy_from_slice(&digest.as_bytes()[..16]);
    hash
}

/// One manifest entry: the freshness key of one cached file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub path: RepoRelativePath,
    pub content_hash: [u8; 16],
    pub byte_len: u64,
    pub fact_file: String,
}

/// Why a cache operation failed. Every variant maps to a safe fallback
/// (per-file reparse or deterministic rebuild); none aborts indexing.
#[derive(Debug, Error)]
pub enum CacheError {
    #[error("cache I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("cache payload is corrupt: {0}")]
    Decode(#[from] CodecError),
    #[error("cache compatibility key mismatch: {0}")]
    KeyMismatch(&'static str),
    #[error("cache identity resolution failed: {0}")]
    Git(String),
}

/// One file's facts ready for publication.
#[derive(Debug)]
pub struct FactsToStore {
    pub language: Language,
    pub facts: FileSyntaxFacts,
    pub content_hash: [u8; 16],
}

/// Outcome of one cache publication.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheWriteOutcome {
    pub published: bool,
    /// Why the manifest was not published (bounded cache policy).
    pub rejection: Option<&'static str>,
    pub entries: u64,
    pub entries_written: u64,
    pub entries_reused: u64,
    /// Files skipped because their encoded payload exceeded
    /// `max_entry_bytes`; they become permanent misses.
    pub skipped_entries: u64,
    pub bytes_written: u64,
    /// Total on-disk bytes of the published cache (manifest + payloads).
    pub total_bytes: u64,
}

/// On-disk cache bound to one configuration.
#[derive(Debug, Clone)]
pub struct CacheStore {
    config: SyntaxCacheConfig,
}

impl CacheStore {
    pub fn new(config: SyntaxCacheConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &SyntaxCacheConfig {
        &self.config
    }

    fn facts_dir(&self) -> PathBuf {
        self.config.directory.join(FACTS_DIR)
    }

    /// Reads and validates the manifest and checks the compatibility key.
    /// `Ok(None)` means no usable cache exists (missing, corrupt, oversized,
    /// or incompatible); the reason is reported for observability.
    pub fn read_compatible_manifest(
        &self,
        expected: &CompatibilityKey,
    ) -> (Option<(Vec<ManifestEntry>, u64)>, Option<String>) {
        let manifest_path = self.config.directory.join(MANIFEST_FILE);
        let limit = MANIFEST_BASE_BYTES
            .saturating_add(
                self.config
                    .max_entries
                    .saturating_mul(MANIFEST_ENTRY_ALLOWANCE_BYTES),
            )
            .saturating_add(1);
        let raw = match read_bounded(&manifest_path, limit) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return (None, Some("missing manifest".to_owned()));
            }
            Err(error) => return (None, Some(format!("manifest unreadable: {error}"))),
        };
        let bytes_read = raw.len() as u64;
        match codec::decode_manifest(&raw) {
            Ok((key, entries)) => {
                if let Some(field) = expected.mismatch(&key) {
                    return (None, Some(format!("compatibility key mismatch: {field}")));
                }
                (Some((entries, bytes_read)), None)
            }
            Err(error) => (None, Some(format!("corrupt manifest: {error}"))),
        }
    }

    /// Reads and decodes one entry's fact payload, bounded by
    /// `max_entry_bytes`. Any failure is a per-file miss for the caller.
    pub fn read_facts(
        &self,
        entry: &ManifestEntry,
        language: Language,
    ) -> Result<(FileSyntaxFacts, u64), CacheError> {
        let path = self.facts_dir().join(&entry.fact_file);
        let raw = read_bounded(&path, self.config.max_entry_bytes.saturating_add(1))?;
        let bytes_read = raw.len() as u64;
        let facts = codec::decode_file_facts(&raw, &entry.path, language)?;
        Ok((facts, bytes_read))
    }

    /// Publishes a new cache revision: unchanged payloads are reused, new
    /// or changed payloads are written atomically, the manifest is renamed
    /// last, and unreferenced payloads are garbage-collected. Bounds are
    /// enforced before the manifest is touched; an over-budget cache simply
    /// is not published and the previous revision stays intact.
    pub fn write(
        &self,
        key: &CompatibilityKey,
        files: &[FactsToStore],
    ) -> Result<CacheWriteOutcome, CacheError> {
        let mut outcome = CacheWriteOutcome::default();
        if files.len() as u64 > self.config.max_entries {
            outcome.rejection = Some("too many entries");
            return Ok(outcome);
        }
        let previous: HashMap<RepoRelativePath, ([u8; 16], String)> =
            match read_bounded(&self.config.directory.join(MANIFEST_FILE), u64::MAX) {
                Ok(raw) => match codec::decode_manifest(&raw) {
                    Ok((previous_key, entries)) if previous_key == *key => entries
                        .into_iter()
                        .map(|entry| (entry.path, (entry.content_hash, entry.fact_file)))
                        .collect(),
                    _ => HashMap::new(),
                },
                Err(_) => HashMap::new(),
            };
        fs::create_dir_all(self.facts_dir())?;
        let mut entries = Vec::with_capacity(files.len());
        let mut payload_bytes = 0_u64;
        for file in files {
            let raw = codec::encode_file_facts(&file.facts);
            let raw_len = raw.len() as u64;
            if raw_len > self.config.max_entry_bytes {
                outcome.skipped_entries = outcome.skipped_entries.saturating_add(1);
                continue;
            }
            let fact_file = fact_file_name(&file.facts.path);
            let reused = previous.get(&file.facts.path).is_some_and(|(hash, name)| {
                *hash == file.content_hash
                    && *name == fact_file
                    && self.facts_dir().join(name).is_file()
            });
            if reused {
                outcome.entries_reused = outcome.entries_reused.saturating_add(1);
            } else {
                write_atomic(&self.facts_dir().join(&fact_file), &raw)?;
                outcome.entries_written = outcome.entries_written.saturating_add(1);
                outcome.bytes_written = outcome.bytes_written.saturating_add(raw_len);
            }
            payload_bytes = payload_bytes.saturating_add(raw_len);
            entries.push(ManifestEntry {
                path: file.facts.path.clone(),
                content_hash: file.content_hash,
                byte_len: file.facts.byte_len,
                fact_file,
            });
        }
        if payload_bytes > self.config.max_total_bytes {
            outcome.rejection = Some("cache bytes budget exceeded");
            return Ok(outcome);
        }
        let manifest = codec::encode_manifest(key, &entries);
        write_atomic(&self.config.directory.join(MANIFEST_FILE), &manifest)?;
        outcome.bytes_written = outcome.bytes_written.saturating_add(manifest.len() as u64);
        outcome.total_bytes = payload_bytes.saturating_add(manifest.len() as u64);
        outcome.entries = entries.len() as u64;
        outcome.published = true;
        self.garbage_collect(&entries);
        Ok(outcome)
    }

    /// Removes fact payloads no manifest entry references anymore. Best
    /// effort: a stale payload is harmless (never read), so failures are
    /// ignored.
    fn garbage_collect(&self, entries: &[ManifestEntry]) {
        let referenced: std::collections::HashSet<&str> = entries
            .iter()
            .map(|entry| entry.fact_file.as_str())
            .collect();
        let Ok(read_dir) = fs::read_dir(self.facts_dir()) else {
            return;
        };
        for entry in read_dir.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if codec::is_fact_file_name(name) && !referenced.contains(name) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Reads at most `limit` bytes of a file; exceeding the limit is an error,
/// never a silent truncation.
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, io::Error> {
    let file = fs::File::open(path)?;
    let mut raw = Vec::new();
    file.take(limit).read_to_end(&mut raw)?;
    if raw.len() as u64 >= limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds the bounded read limit", path.display()),
        ));
    }
    Ok(raw)
}

/// Writes `raw` to a sibling temporary file and renames it over `path`, so
/// concurrent readers only ever see a complete previous or next revision.
fn write_atomic(path: &Path, raw: &[u8]) -> Result<(), io::Error> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, raw)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::facts::SymbolFact;
    use chakra_domain::location::{SourceRange, TextPosition};
    use chakra_domain::symbol::SymbolKind;

    type TestCheck<T> = Result<T, Box<dyn std::error::Error>>;

    fn sample_store() -> TestCheck<(tempfile::TempDir, CacheStore)> {
        let dir = tempfile::TempDir::new()?;
        let store = CacheStore::new(SyntaxCacheConfig::new(dir.path().to_path_buf()));
        Ok((dir, store))
    }

    fn sample_key() -> CompatibilityKey {
        CompatibilityKey {
            graph_model_version: GRAPH_MODEL_VERSION,
            repository: "repo".to_owned(),
            head_sha: "abc".to_owned(),
            chakra_version: "0.1.3".to_owned(),
            config_fingerprint: [1; 16],
            extractors: vec![(Language::Rust, "rust:f1".to_owned())],
        }
    }

    fn sample_file(path: &str, source: &str) -> TestCheck<FactsToStore> {
        let path = RepoRelativePath::new(path)?;
        let location = SourceRange::new(
            path.clone(),
            TextPosition::new(1, 1)?,
            TextPosition::new(1, 5)?,
        )?;
        Ok(FactsToStore {
            language: Language::Rust,
            facts: FileSyntaxFacts {
                path,
                byte_len: source.len() as u64,
                module_path: Vec::new(),
                extension_scopes: Vec::new(),
                symbols: vec![SymbolFact {
                    qualified_name: "alpha".to_owned(),
                    container: None,
                    kind: SymbolKind::Function,
                    location,
                    signature: None,
                    parent: None,
                    is_extension_method: false,
                }],
                calls: Vec::new(),
                named_relations: Vec::new(),
                type_relations: Vec::new(),
                implementations: Vec::new(),
                has_errors: false,
                diagnostics: Vec::new(),
                diagnostic_count: 0,
            },
            content_hash: content_hash(source),
        })
    }

    #[test]
    fn write_restore_and_key_mismatch() -> TestCheck<()> {
        let (_dir, store) = sample_store()?;
        let key = sample_key();
        let files = vec![sample_file("src/lib.rs", "pub fn alpha() {}\n")?];
        let written = store.write(&key, &files)?;
        assert!(written.published);
        assert_eq!(written.entries, 1);

        let (manifest, rejection) = store.read_compatible_manifest(&key);
        assert!(rejection.is_none(), "rejection: {rejection:?}");
        let (entries, bytes_read) = manifest.ok_or("manifest missing")?;
        assert!(bytes_read > 0);
        assert_eq!(entries.len(), 1);
        let (facts, _) = store.read_facts(&entries[0], Language::Rust)?;
        assert_eq!(facts.symbols.len(), 1);

        let mut other = key.clone();
        other.head_sha = "def".to_owned();
        let (manifest, rejection) = store.read_compatible_manifest(&other);
        assert!(manifest.is_none());
        assert_eq!(
            rejection.as_deref(),
            Some("compatibility key mismatch: head_sha")
        );
        Ok(())
    }

    #[test]
    fn corrupt_fact_file_is_an_isolated_miss() -> TestCheck<()> {
        let (_dir, store) = sample_store()?;
        let key = sample_key();
        let files = vec![
            sample_file("src/a.rs", "pub fn a() {}\n")?,
            sample_file("src/b.rs", "pub fn b() {}\n")?,
        ];
        store.write(&key, &files)?;
        let (manifest, _) = store.read_compatible_manifest(&key);
        let (entries, _) = manifest.ok_or("manifest missing")?;
        // Corrupt one payload: the other entry still decodes.
        let target = store.facts_dir().join(&entries[0].fact_file);
        fs::write(&target, b"garbage")?;
        assert!(store.read_facts(&entries[0], Language::Rust).is_err());
        assert!(store.read_facts(&entries[1], Language::Rust).is_ok());
        Ok(())
    }

    #[test]
    fn bounds_prevent_publication() -> TestCheck<()> {
        let (_dir, mut store) = sample_store()?;
        store.config.max_total_bytes = 8;
        let key = sample_key();
        let files = vec![sample_file("src/lib.rs", "pub fn alpha() {}\n")?];
        let written = store.write(&key, &files)?;
        assert!(!written.published);
        assert_eq!(written.rejection, Some("cache bytes budget exceeded"));
        let (manifest, rejection) = store.read_compatible_manifest(&key);
        assert!(manifest.is_none());
        assert_eq!(rejection.as_deref(), Some("missing manifest"));
        Ok(())
    }

    #[test]
    fn incremental_write_reuses_unchanged_payloads() -> TestCheck<()> {
        let (_dir, store) = sample_store()?;
        let key = sample_key();
        let files = vec![
            sample_file("src/a.rs", "pub fn a() {}\n")?,
            sample_file("src/b.rs", "pub fn b() {}\n")?,
        ];
        let first = store.write(&key, &files)?;
        assert_eq!(first.entries_written, 2);
        let second = store.write(&key, &files)?;
        assert_eq!(second.entries_written, 0);
        assert_eq!(second.entries_reused, 2);
        Ok(())
    }
}
