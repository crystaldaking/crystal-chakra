//! Synthetic persistence cache model (issue #38).
//!
//! Honestly a **model**: it measures what a per-file syntax-fact cache would
//! cost without implementing graph restoration (explicitly out of scope for
//! issue #38). The layout mirrors a plausible production design:
//!
//! - `manifest.json` — compatibility key plus one `(path, content_hash,
//!   fact_file)` entry per indexed source file;
//! - `facts/<path-hash>.json` — the serialized [`FileFacts`] of one file.
//!
//! Phases measured:
//!
//! - **write** — serialize every [`FileFacts`] plus the manifest to a temp
//!   directory (bytes written, wall, CPU);
//! - **warm restore** — read the manifest, check the compatibility key, then
//!   for every entry hash the current worktree source, and read + deserialize
//!   the fact file of every hit (bytes read, wall, CPU, hit ratio);
//! - **validation only** — read the manifest and hash the worktree sources
//!   without touching fact files (the cache-validation overhead budget).
//!
//! Hash validation reads every source file from disk — the conservative,
//! honest model; a production cache might trust metadata first, which would
//! only make restore look cheaper than measured here.
//!
//! A compatibility-key mismatch is not an error: restore reports
//! `compatible: false` and the caller falls back to a deterministic rebuild,
//! which is exactly the production fallback policy.

use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use chakra_domain::indexing::IndexBudgets;

use super::projection::{FileFacts, MODEL_FORMAT_VERSION, model_hash};
use crate::{Check, ensure, failure};

/// Cache manifest file name inside the model cache directory.
const MANIFEST_FILE: &str = "manifest.json";
/// Fact files live in this subdirectory, named by the model hash of the
/// repository-relative path (path strings are stored inside for validation).
const FACTS_DIR: &str = "facts";

/// Everything a model cache must match before its facts are trusted
/// (SPEC §14): repository identity, commit, index format version, and the
/// indexing configuration fingerprint. Parser/provider versions are out of
/// scope for the syntax-only benchmark model and are called out in the
/// evaluation document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityKey {
    pub model_format_version: u32,
    pub repository: String,
    pub sha: String,
    pub config_fingerprint: String,
    pub content_fingerprint: String,
}

impl CompatibilityKey {
    /// Builds the key for one measured target.
    pub fn new(
        repository: &str,
        sha: &str,
        budgets: &IndexBudgets,
        files: &[FileFacts],
    ) -> Check<Self> {
        let config = model_hash(serde_json::to_string(budgets)?.as_bytes());
        let mut content = std::collections::hash_map::DefaultHasher::new();
        use std::hash::Hasher;
        for file in files {
            content.write(file.path.as_bytes());
            content.write(file.content_hash.as_bytes());
        }
        Ok(Self {
            model_format_version: MODEL_FORMAT_VERSION,
            repository: repository.to_owned(),
            sha: sha.to_owned(),
            config_fingerprint: config,
            content_fingerprint: format!("{:016x}", content.finish()),
        })
    }

    /// Single-token rendering for artifacts and quick comparisons.
    pub fn fingerprint(&self) -> String {
        model_hash(
            format!(
                "{}|{}|{}|{}|{}",
                self.model_format_version,
                self.repository,
                self.sha,
                self.config_fingerprint,
                self.content_fingerprint
            )
            .as_bytes(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheManifestEntry {
    path: String,
    content_hash: String,
    fact_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheManifest {
    key: CompatibilityKey,
    files: Vec<CacheManifestEntry>,
}

/// Wall/CPU/RSS/bytes evidence of one measured phase.
#[derive(Debug, Clone, Default)]
pub struct PhaseMeasurement {
    pub wall_micros: u64,
    /// Process CPU consumed during the phase (user + system), where the
    /// platform exposes it.
    pub cpu_micros: Option<u64>,
    /// Process high-water RSS at the end of the phase. Monotonic within one
    /// process: a phase after the cold rebuild reports at least the cold
    /// rebuild's peak. Compare RSS only within one phase family.
    pub end_peak_rss_bytes: Option<u64>,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

/// Measures wall and process CPU of one phase.
pub struct PhaseTimer {
    started: Instant,
    cpu_start: Option<u64>,
}

impl PhaseTimer {
    pub fn start() -> Self {
        Self {
            started: Instant::now(),
            cpu_start: process_cpu_micros(),
        }
    }

    pub fn finish(self) -> PhaseMeasurement {
        let wall_micros = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let cpu_micros = match (self.cpu_start, process_cpu_micros()) {
            (Some(start), Some(end)) => Some(end.saturating_sub(start)),
            _ => None,
        };
        PhaseMeasurement {
            wall_micros,
            cpu_micros,
            end_peak_rss_bytes: process_peak_rss_bytes(),
            bytes_read: 0,
            bytes_written: 0,
        }
    }
}

#[cfg(unix)]
fn process_cpu_micros() -> Option<u64> {
    use nix::sys::resource::{UsageWho, getrusage};
    use nix::sys::time::TimeValLike;
    let usage = getrusage(UsageWho::RUSAGE_SELF).ok()?;
    let total = usage
        .user_time()
        .num_microseconds()
        .checked_add(usage.system_time().num_microseconds())?;
    u64::try_from(total).ok()
}

#[cfg(not(unix))]
fn process_cpu_micros() -> Option<u64> {
    None
}

#[cfg(unix)]
fn process_peak_rss_bytes() -> Option<u64> {
    use nix::sys::resource::{UsageWho, getrusage};
    let rss = u64::try_from(getrusage(UsageWho::RUSAGE_SELF).ok()?.max_rss()).ok()?;
    #[cfg(target_vendor = "apple")]
    {
        Some(rss)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        rss.checked_mul(1024)
    }
}

#[cfg(not(unix))]
fn process_peak_rss_bytes() -> Option<u64> {
    None
}

/// Outcome of writing the model cache.
pub struct WriteOutcome {
    pub measurement: PhaseMeasurement,
}

/// Outcome of a restore or validation pass.
pub struct RestoreOutcome {
    pub measurement: PhaseMeasurement,
    /// `false` on a compatibility-key mismatch: the caller falls back to a
    /// deterministic rebuild. Hits/misses stay zero in that case.
    pub compatible: bool,
    pub hits: u64,
    pub misses: u64,
}

impl RestoreOutcome {
    /// Hit ratio in per-mille (integer arithmetic keeps artifacts exact).
    pub fn hit_ratio_per_mille(&self) -> u64 {
        let total = self.hits.saturating_add(self.misses);
        if total == 0 {
            return 0;
        }
        self.hits.saturating_mul(1_000) / total
    }
}

fn fact_file_name(path: &str) -> String {
    format!("{}.json", model_hash(path.as_bytes()))
}

fn read_manifest(cache_dir: &Path) -> Check<(CacheManifest, u64)> {
    let raw = fs::read(cache_dir.join(MANIFEST_FILE))?;
    let bytes = u64::try_from(raw.len()).unwrap_or(u64::MAX);
    let manifest: CacheManifest = serde_json::from_slice(&raw)?;
    Ok((manifest, bytes))
}

/// Writes the model cache: one fact file per source file plus the manifest.
/// The timer covers serialization and I/O; projection building is measured
/// separately by the caller.
pub fn write_cache(
    cache_dir: &Path,
    key: &CompatibilityKey,
    files: &[FileFacts],
) -> Check<WriteOutcome> {
    let timer = PhaseTimer::start();
    let facts_dir = cache_dir.join(FACTS_DIR);
    fs::create_dir_all(&facts_dir)?;
    let mut bytes_written = 0_u64;
    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let fact_file = fact_file_name(&file.path);
        let raw = serde_json::to_vec(file)?;
        bytes_written = bytes_written.saturating_add(u64::try_from(raw.len()).unwrap_or(u64::MAX));
        fs::write(facts_dir.join(&fact_file), raw)?;
        entries.push(CacheManifestEntry {
            path: file.path.clone(),
            content_hash: file.content_hash.clone(),
            fact_file,
        });
    }
    let manifest = CacheManifest {
        key: key.clone(),
        files: entries,
    };
    let raw = serde_json::to_vec_pretty(&manifest)?;
    bytes_written = bytes_written.saturating_add(u64::try_from(raw.len()).unwrap_or(u64::MAX));
    fs::write(cache_dir.join(MANIFEST_FILE), raw)?;
    let mut measurement = timer.finish();
    measurement.bytes_written = bytes_written;
    Ok(WriteOutcome { measurement })
}

/// Warm restore: compatibility-key check, source-hash validation, and
/// read + deserialize of every hit's fact file. Misses are not read — a real
/// restore would reparse them instead (measured separately as refresh).
pub fn restore_cache(
    cache_dir: &Path,
    expected: &CompatibilityKey,
    worktree: &Path,
) -> Check<RestoreOutcome> {
    restore_or_validate(cache_dir, expected, worktree, true)
}

/// Validation only: compatibility-key check and source-hash validation
/// without reading any fact file.
pub fn validate_cache(
    cache_dir: &Path,
    expected: &CompatibilityKey,
    worktree: &Path,
) -> Check<RestoreOutcome> {
    restore_or_validate(cache_dir, expected, worktree, false)
}

fn restore_or_validate(
    cache_dir: &Path,
    expected: &CompatibilityKey,
    worktree: &Path,
    read_facts: bool,
) -> Check<RestoreOutcome> {
    let timer = PhaseTimer::start();
    let (manifest, manifest_bytes) = read_manifest(cache_dir)?;
    let mut bytes_read = manifest_bytes;
    if manifest.key != *expected {
        let mut measurement = timer.finish();
        measurement.bytes_read = bytes_read;
        return Ok(RestoreOutcome {
            measurement,
            compatible: false,
            hits: 0,
            misses: 0,
        });
    }
    let mut hits = 0_u64;
    let mut misses = 0_u64;
    for entry in &manifest.files {
        let source = fs::read(worktree.join(&entry.path))
            .map_err(|error| failure(format!("cannot hash `{}`: {error}", entry.path)))?;
        bytes_read = bytes_read.saturating_add(u64::try_from(source.len()).unwrap_or(u64::MAX));
        if model_hash(&source) != entry.content_hash {
            misses += 1;
            continue;
        }
        hits += 1;
        if read_facts {
            let raw = fs::read(cache_dir.join(FACTS_DIR).join(&entry.fact_file))?;
            bytes_read = bytes_read.saturating_add(u64::try_from(raw.len()).unwrap_or(u64::MAX));
            let facts: FileFacts = serde_json::from_slice(&raw)?;
            ensure(
                facts.path == entry.path && facts.content_hash == entry.content_hash,
                format!(
                    "fact file `{}` does not match its manifest entry",
                    entry.path
                ),
            )?;
        }
    }
    let mut measurement = timer.finish();
    measurement.bytes_read = bytes_read;
    Ok(RestoreOutcome {
        measurement,
        compatible: true,
        hits,
        misses,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_facts(path: &str, content: &str) -> FileFacts {
        FileFacts {
            path: path.to_owned(),
            content_hash: model_hash(content.as_bytes()),
            byte_len: u64::try_from(content.len()).unwrap_or(u64::MAX),
            diagnostic_count: 0,
            declarations: Vec::new(),
            relationships: Vec::new(),
            call_candidates: Vec::new(),
            omitted_declarations: 0,
            omitted_relationships: 0,
            omitted_call_candidates: 0,
        }
    }

    #[test]
    fn key_changes_with_any_input() -> Check<()> {
        let budgets = IndexBudgets::default();
        let files = vec![sample_facts("src/lib.rs", "pub fn a() {}\n")];
        let key = CompatibilityKey::new("owner/repo", "abc", &budgets, &files)?;
        assert_eq!(
            key,
            CompatibilityKey::new("owner/repo", "abc", &budgets, &files)?
        );
        assert_ne!(
            key,
            CompatibilityKey::new("owner/other", "abc", &budgets, &files)?
        );
        assert_ne!(
            key,
            CompatibilityKey::new("owner/repo", "def", &budgets, &files)?
        );
        let changed = vec![sample_facts("src/lib.rs", "pub fn b() {}\n")];
        assert_ne!(
            key,
            CompatibilityKey::new("owner/repo", "abc", &budgets, &changed)?
        );
        Ok(())
    }

    #[test]
    fn roundtrip_restore_validate_and_fallback() -> Check<()> {
        let worktree = tempfile::TempDir::new()?;
        let cache = tempfile::TempDir::new()?;
        fs::write(worktree.path().join("a.rs"), "pub fn a() {}\n")?;
        fs::write(worktree.path().join("b.rs"), "pub fn b() {}\n")?;
        let files = vec![
            sample_facts("a.rs", "pub fn a() {}\n"),
            sample_facts("b.rs", "pub fn b() {}\n"),
        ];
        let budgets = IndexBudgets::default();
        let key = CompatibilityKey::new("tiny/tiny", "sha", &budgets, &files)?;

        let written = write_cache(cache.path(), &key, &files)?;
        assert!(written.measurement.bytes_written > 0);
        assert_eq!(
            fs::read_dir(cache.path().join(FACTS_DIR))?.count(),
            files.len()
        );

        let restored = restore_cache(cache.path(), &key, worktree.path())?;
        assert!(restored.compatible);
        assert_eq!((restored.hits, restored.misses), (2, 0));
        assert_eq!(restored.hit_ratio_per_mille(), 1_000);
        assert!(restored.measurement.bytes_read >= written.measurement.bytes_written);

        let validated = validate_cache(cache.path(), &key, worktree.path())?;
        assert!(validated.compatible);
        assert_eq!((validated.hits, validated.misses), (2, 0));
        assert!(
            validated.measurement.bytes_read < restored.measurement.bytes_read,
            "validation must not read fact files"
        );

        // One edited file: one miss; the hit is still deserialized.
        fs::write(worktree.path().join("b.rs"), "pub fn b() { a(); }\n")?;
        let restored = restore_cache(cache.path(), &key, worktree.path())?;
        assert!(restored.compatible);
        assert_eq!((restored.hits, restored.misses), (1, 1));
        assert_eq!(restored.hit_ratio_per_mille(), 500);

        // Compatibility mismatch: deterministic-rebuild fallback, no error.
        let other = CompatibilityKey::new("tiny/tiny", "other-sha", &budgets, &files)?;
        let rejected = restore_cache(cache.path(), &other, worktree.path())?;
        assert!(!rejected.compatible);
        assert_eq!((rejected.hits, rejected.misses), (0, 0));
        Ok(())
    }
}
