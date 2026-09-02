//! Real complete-snapshot benchmark pipeline (issue #50).

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use chakra_domain::composition::{CommitSnapshotOrigin, CommitSnapshotRejection};
use chakra_domain::indexing::IndexCancellation;
use chakra_domain::operation::OperationContext;
use chakra_engine::SymbolGraph;
use chakra_language::{CommitIndexReport, IndexOptions, index_commit_with_options};
use chakra_workspace::{CommitSnapshotCache, CommitSnapshotCacheConfig};
use tempfile::TempDir;

use super::report::{
    CachePopulationReport, ColdRebuildReport, GateReport, GraphSummary, PrebuiltProvenance,
    RestoreReport, RunReport, SharedIndexReport, TransportReport,
};
use crate::fixture::copy_fixture_tree;
use crate::persistence::{PersistenceTarget, PhaseTimer, TargetKind};
use crate::{Check, failure};

const CACHE_METADATA_FILE: &str = ".chakra-corpus.json";
const MANIFEST_FILE: &str = "manifest.json";
const PAYLOAD_FILE: &str = "snapshot.bin";
const ACCESS_FILE: &str = "access";
const SIZE_GATE_FILES: u64 = 1_000;
const MIN_RESTORE_SPEEDUP_PER_MILLE: u64 = 5_000;
const MAX_ARTIFACT_TO_SOURCE_PER_MILLE: u64 = 2_000;
const IO_CHUNK_BYTES: usize = 64 * 1024;

pub fn evaluate_target(
    target: &PersistenceTarget,
    runs: u32,
    spool_dir: &Path,
) -> Check<SharedIndexReport> {
    if runs == 0 {
        return Err(failure("shared-index benchmark needs at least one run").into());
    }

    let mut fixture_repo: Option<TempDir> = None;
    let (checkout, sha) = match target.kind {
        TargetKind::Corpus => {
            let checkout = target
                .checkout
                .clone()
                .ok_or_else(|| failure("corpus target without a checkout path"))?;
            if !checkout.is_dir() {
                return Ok(SharedIndexReport::skipped(
                    &target.name,
                    target.kind,
                    &target.sha,
                    format!(
                        "checkout not cached at {}; fetch with `python3 tools/fetch_corpus.py`",
                        checkout.display()
                    ),
                ));
            }
            let head = git(&checkout, &["rev-parse", "HEAD"])?;
            if head != target.sha {
                return Ok(SharedIndexReport::skipped(
                    &target.name,
                    target.kind,
                    &target.sha,
                    format!(
                        "checkout HEAD {head} does not match pinned SHA {}; refusing to evaluate",
                        target.sha
                    ),
                ));
            }
            let status = git(&checkout, &["status", "--porcelain"])?;
            let dirty = dirty_worktree_entries(&status);
            if !dirty.is_empty() {
                return Ok(SharedIndexReport::skipped(
                    &target.name,
                    target.kind,
                    &target.sha,
                    format!("checkout has local changes; refusing to benchmark it: {dirty:?}"),
                ));
            }
            (checkout, target.sha.clone())
        }
        TargetKind::Fixture => {
            let fixture_dir = target
                .fixture_dir
                .clone()
                .ok_or_else(|| failure("fixture target without a fixture directory"))?;
            let seeded = seed_fixture(&fixture_dir)?;
            let sha = git(seeded.path(), &["rev-parse", "HEAD"])?;
            let checkout = seeded.path().to_path_buf();
            fixture_repo = Some(seeded);
            (checkout, sha)
        }
    };

    let mut reports = Vec::new();
    for run in 1..=runs {
        reports.push(evaluate_run(run, &checkout, &sha, spool_dir)?);
    }
    drop(fixture_repo);
    Ok(SharedIndexReport::measured(
        &target.name,
        target.kind,
        &sha,
        IndexOptions::default().budgets,
        reports,
    ))
}

fn evaluate_run(run: u32, checkout: &Path, sha: &str, spool: &Path) -> Check<RunReport> {
    let identity = chakra_git::resolve_workspace_identity(checkout)?;
    let commit =
        chakra_git::resolve_head_commit_with_context(checkout, &OperationContext::unbounded())?
            .ok_or_else(|| failure("shared-index benchmark requires a committed repository"))?;
    if commit != sha {
        return Err(failure(format!(
            "resolved HEAD {commit} does not match evaluated SHA {sha}"
        ))
        .into());
    }

    let cold_timer = PhaseTimer::start();
    let cold = index_commit_with_options(checkout, Some(sha), IndexOptions::default())?;
    let cold_phase = cold_timer.finish();
    let expected_graph = graph_summary(&cold);
    let cold_rebuild = ColdRebuildReport {
        phase: cold_phase.into(),
        graph: expected_graph.clone(),
        indexer_phase_peak_rss_bytes: cold.indexing.memory.observed_phase_peak_rss_bytes,
    };

    let local_cache_root = TempDir::new_in(spool)?;
    let local_config = cache_config(local_cache_root.path());
    let population_timer = PhaseTimer::start();
    let populated = CommitSnapshotCache::new(local_config.clone())?.load_or_build(
        checkout,
        &identity.repository,
        Some(sha),
        IndexOptions::default(),
    )?;
    let mut population_phase = population_timer.finish();
    let artifact = find_only_artifact(local_cache_root.path())?;
    let artifact_bytes = artifact.as_deref().map(artifact_bytes).transpose()?;
    population_phase.bytes_written = artifact_bytes.unwrap_or(0);
    let artifact_unavailable_detail = if artifact.is_none()
        && populated.reuse.rejection == Some(CommitSnapshotRejection::Corrupt)
    {
        populated
            .encode_snapshot(&IndexCancellation::default())
            .err()
            .map(|error| error.to_string())
    } else {
        None
    };
    let cache_population = CachePopulationReport {
        phase: population_phase.into(),
        artifact_available: artifact.is_some(),
        artifact_bytes,
        lookup_or_store_rejection: populated.reuse.rejection.map(rejection_name),
        artifact_unavailable_detail,
    };

    let Some(artifact) = artifact else {
        return Ok(RunReport {
            run,
            cold_rebuild,
            cache_population,
            local_restore: None,
            prebuilt_transport: None,
            prebuilt_restore: None,
            prebuilt_provenance: None,
            gates: unavailable_gates(&expected_graph),
        });
    };

    let local_restore = measure_restore(
        checkout,
        &identity.repository,
        sha,
        local_config,
        artifact_bytes.unwrap_or(0),
        &expected_graph,
    )?;

    let imported_root = TempDir::new_in(spool)?;
    let transport_timer = PhaseTimer::start();
    let (source_digest, source_bytes) = digest_artifact(&artifact)?;
    let imported_artifact = copy_artifact(&artifact, imported_root.path())?;
    let (imported_digest, imported_bytes) = digest_artifact(&imported_artifact)?;
    let mut transport_phase = transport_timer.finish();
    // Digesting the producer artifact, copying it, and digesting the imported
    // artifact performs three complete reads. The copy writes one artifact.
    transport_phase.bytes_read = source_bytes
        .saturating_mul(2)
        .saturating_add(imported_bytes);
    transport_phase.bytes_written = imported_bytes;
    let digest_verified = source_digest == imported_digest && source_bytes == imported_bytes;
    if !digest_verified {
        return Err(failure("prebuilt artifact transport digest mismatch").into());
    }
    let prebuilt_transport = TransportReport {
        phase: transport_phase.into(),
        artifact_bytes: imported_bytes,
        artifact_blake3: imported_digest.clone(),
        digest_verified,
    };
    let prebuilt_restore = measure_restore(
        checkout,
        &identity.repository,
        sha,
        cache_config(imported_root.path()),
        imported_bytes,
        &expected_graph,
    )?;
    let prebuilt_provenance = PrebuiltProvenance {
        producer: "benchmark-local-ci-simulation".to_owned(),
        trust_boundary: "authenticity requires an authenticated CI artifact channel and explicit user import; BLAKE3 proves integrity only".to_owned(),
        fact_scope: "materialization_independent_commit_syntax".to_owned(),
        provider_enrichment_included: false,
        repository: identity.repository.as_str().to_owned(),
        commit: sha.to_owned(),
        compatibility: cold.snapshot_compatibility(),
        artifact_blake3: imported_digest,
    };
    let gates = evaluate_gates(
        &cold_rebuild,
        &cache_population,
        Some(&local_restore),
        Some(&prebuilt_restore),
    );
    Ok(RunReport {
        run,
        cold_rebuild,
        cache_population,
        local_restore: Some(local_restore),
        prebuilt_transport: Some(prebuilt_transport),
        prebuilt_restore: Some(prebuilt_restore),
        prebuilt_provenance: Some(prebuilt_provenance),
        gates,
    })
}

fn measure_restore(
    checkout: &Path,
    repository: &chakra_domain::identity::RepositoryId,
    sha: &str,
    config: CommitSnapshotCacheConfig,
    artifact_bytes: u64,
    expected: &GraphSummary,
) -> Check<RestoreReport> {
    let timer = PhaseTimer::start();
    let restored = CommitSnapshotCache::new(config)?.load_or_build(
        checkout,
        repository,
        Some(sha),
        IndexOptions::default(),
    )?;
    let mut phase = timer.finish();
    phase.bytes_read = artifact_bytes;
    if restored.reuse.origin != CommitSnapshotOrigin::DiskRestore {
        return Err(failure(format!(
            "expected disk restore but observed {:?} ({:?})",
            restored.reuse.origin, restored.reuse.rejection
        ))
        .into());
    }
    let graph = graph_summary(&restored);
    let graph_verified = &graph == expected;
    Ok(RestoreReport {
        phase: phase.into(),
        origin: "disk_restore".to_owned(),
        graph,
        graph_verified,
    })
}

fn evaluate_gates(
    cold: &ColdRebuildReport,
    population: &CachePopulationReport,
    local: Option<&RestoreReport>,
    prebuilt: Option<&RestoreReport>,
) -> GateReport {
    let eligible = cold.graph.source_files > SIZE_GATE_FILES;
    let ratio = population
        .artifact_bytes
        .map(|bytes| bytes.saturating_mul(1_000) / cold.graph.source_bytes.max(1));
    let local_speedup = local.map(|restore| {
        cold.phase.wall_micros.saturating_mul(1_000) / restore.phase.wall_micros.max(1)
    });
    let prebuilt_speedup = prebuilt.map(|restore| {
        cold.phase.wall_micros.saturating_mul(1_000) / restore.phase.wall_micros.max(1)
    });
    let local_exact_graph_match = local.is_some_and(|restore| restore.graph_verified);
    let prebuilt_exact_graph_match = prebuilt.is_some_and(|restore| restore.graph_verified);
    let exact_graph_match = local_exact_graph_match && prebuilt_exact_graph_match;
    let local_restore_gate_passed =
        local_speedup.is_some_and(|speedup| speedup >= MIN_RESTORE_SPEEDUP_PER_MILLE);
    let prebuilt_restore_gate_passed =
        prebuilt_speedup.is_some_and(|speedup| speedup >= MIN_RESTORE_SPEEDUP_PER_MILLE);
    let size_gate_passed = ratio.is_some_and(|ratio| ratio <= MAX_ARTIFACT_TO_SOURCE_PER_MILLE);
    let approved_local =
        eligible && local_exact_graph_match && local_restore_gate_passed && size_gate_passed;
    let approved_prebuilt =
        eligible && prebuilt_exact_graph_match && prebuilt_restore_gate_passed && size_gate_passed;
    GateReport {
        size_gate_files: SIZE_GATE_FILES,
        eligible_for_default_restore: eligible,
        artifact_to_source_per_mille: ratio,
        local_speedup_per_mille: local_speedup,
        prebuilt_speedup_per_mille: prebuilt_speedup,
        local_exact_graph_match,
        prebuilt_exact_graph_match,
        exact_graph_match,
        local_restore_gate_passed,
        prebuilt_restore_gate_passed,
        size_gate_passed,
        approved_for_default_local_restore: approved_local,
        approved_for_prebuilt_import: approved_prebuilt,
    }
}

fn unavailable_gates(graph: &GraphSummary) -> GateReport {
    GateReport {
        size_gate_files: SIZE_GATE_FILES,
        eligible_for_default_restore: graph.source_files > SIZE_GATE_FILES,
        artifact_to_source_per_mille: None,
        local_speedup_per_mille: None,
        prebuilt_speedup_per_mille: None,
        local_exact_graph_match: false,
        prebuilt_exact_graph_match: false,
        exact_graph_match: false,
        local_restore_gate_passed: false,
        prebuilt_restore_gate_passed: false,
        size_gate_passed: false,
        approved_for_default_local_restore: false,
        approved_for_prebuilt_import: false,
    }
}

fn graph_summary(report: &CommitIndexReport) -> GraphSummary {
    GraphSummary {
        files: report.graph.file_count(),
        source_files: report.source_files,
        source_bytes: report.source_bytes,
        symbols: report.graph.symbol_count(),
        edges: report.graph.edge_count(),
        call_sites: report.graph.call_site_count(),
        fingerprint: graph_fingerprint(&report.graph),
    }
}

fn graph_fingerprint(graph: &SymbolGraph) -> String {
    let mut fingerprint = blake3::Hasher::new();
    fingerprint.update(format!("{:?}", graph.file_summaries()).as_bytes());
    for symbol in graph.symbols() {
        fingerprint.update(format!("{symbol:?}").as_bytes());
        fingerprint.update(format!("{:?}", graph.outgoing_edges(symbol.id)).as_bytes());
        fingerprint.update(
            format!("{:?}", graph.call_sites_from(symbol.id).collect::<Vec<_>>()).as_bytes(),
        );
    }
    fingerprint.finalize().to_hex().to_string()
}

fn cache_config(directory: &Path) -> CommitSnapshotCacheConfig {
    let mut config = CommitSnapshotCacheConfig::with_directory(directory.to_path_buf());
    config.max_memory_entries = 1;
    config.max_disk_artifacts = 1;
    config
}

fn find_only_artifact(cache_root: &Path) -> Check<Option<PathBuf>> {
    let entries = cache_root.join("entries");
    let read_dir = match fs::read_dir(&entries) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut artifacts = Vec::new();
    for entry in read_dir {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            artifacts.push(entry.path());
        }
    }
    match artifacts.as_slice() {
        [] => Ok(None),
        [artifact] => Ok(Some(artifact.clone())),
        _ => Err(failure(format!(
            "expected at most one snapshot artifact in {}, found {}",
            entries.display(),
            artifacts.len()
        ))
        .into()),
    }
}

fn artifact_bytes(artifact: &Path) -> Check<u64> {
    let mut total = 0_u64;
    for name in [MANIFEST_FILE, PAYLOAD_FILE, ACCESS_FILE] {
        total = total.saturating_add(fs::metadata(artifact.join(name))?.len());
    }
    Ok(total)
}

fn digest_artifact(artifact: &Path) -> Check<(String, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut total = 0_u64;
    for name in [MANIFEST_FILE, PAYLOAD_FILE, ACCESS_FILE] {
        hasher.update(name.as_bytes());
        hasher.update(&[0]);
        let mut file = File::open(artifact.join(name))?;
        let mut chunk = [0_u8; IO_CHUNK_BYTES];
        loop {
            let read = file.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            hasher.update(&chunk[..read]);
            total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        }
    }
    Ok((hasher.finalize().to_hex().to_string(), total))
}

fn copy_artifact(source: &Path, destination_root: &Path) -> Check<PathBuf> {
    let fingerprint = source
        .file_name()
        .ok_or_else(|| failure("snapshot artifact has no fingerprint directory name"))?;
    let destination = destination_root.join("entries").join(fingerprint);
    fs::create_dir_all(&destination)?;
    for name in [MANIFEST_FILE, PAYLOAD_FILE, ACCESS_FILE] {
        fs::copy(source.join(name), destination.join(name))?;
    }
    Ok(destination)
}

fn rejection_name(rejection: CommitSnapshotRejection) -> String {
    match rejection {
        CommitSnapshotRejection::CacheDisabled => "cache_disabled",
        CommitSnapshotRejection::NotFound => "not_found",
        CommitSnapshotRejection::FormatMismatch => "format_mismatch",
        CommitSnapshotRejection::CompatibilityMismatch => "compatibility_mismatch",
        CommitSnapshotRejection::Corrupt => "corrupt",
        CommitSnapshotRejection::Oversized => "oversized",
        CommitSnapshotRejection::IoFailure => "io_failure",
    }
    .to_owned()
}

fn dirty_worktree_entries(status: &str) -> Vec<&str> {
    status
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|line| line.trim_start_matches("?? ").trim() != CACHE_METADATA_FILE)
        .collect()
}

fn seed_fixture(fixture_dir: &Path) -> Check<TempDir> {
    let seeded = TempDir::new()?;
    copy_fixture_tree(fixture_dir, seeded.path())?;
    git(seeded.path(), &["init", "--quiet"])?;
    git(seeded.path(), &["add", "-A"])?;
    git(
        seeded.path(),
        &[
            "-c",
            "user.email=shared-index@example.invalid",
            "-c",
            "user.name=Chakra Shared Index Benchmark",
            "commit",
            "--quiet",
            "-m",
            "shared-index fixture seed",
        ],
    )?;
    Ok(seeded)
}

fn git(root: &Path, args: &[&str]) -> Check<String> {
    let output = Command::new("git").current_dir(root).args(args).output()?;
    if !output.status.success() {
        return Err(failure(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::workspace_root;
    use crate::persistence::fixture_targets;
    use crate::shared_index::TargetStatus;

    #[test]
    fn fixture_restore_matches_cold_and_transport_is_verified() -> Check<()> {
        let targets = fixture_targets(&workspace_root())?;
        let target = targets
            .iter()
            .find(|target| target.name == "fixture/rust/controller-service-provider")
            .ok_or("rust fixture target missing")?;
        let spool = TempDir::new()?;
        let report = evaluate_target(target, 1, spool.path())?;
        assert_eq!(report.status, TargetStatus::Measured);
        let run = &report.runs[0];
        assert!(run.cache_population.artifact_available);
        assert!(run.local_restore.as_ref().is_some_and(|r| r.graph_verified));
        assert!(
            run.prebuilt_transport
                .as_ref()
                .is_some_and(|transport| transport.digest_verified)
        );
        assert!(
            run.prebuilt_restore
                .as_ref()
                .is_some_and(|r| r.graph_verified)
        );
        assert!(!run.gates.eligible_for_default_restore);
        assert!(!run.gates.approved_for_prebuilt_import);
        Ok(())
    }

    #[test]
    fn artifact_digest_detects_corruption() -> Check<()> {
        let source = TempDir::new()?;
        let artifact = source.path().join("artifact");
        fs::create_dir(&artifact)?;
        fs::write(artifact.join(MANIFEST_FILE), b"manifest")?;
        fs::write(artifact.join(PAYLOAD_FILE), b"payload")?;
        fs::write(artifact.join(ACCESS_FILE), b"1\n")?;
        let (expected, _) = digest_artifact(&artifact)?;
        fs::write(artifact.join(PAYLOAD_FILE), b"corrupt")?;
        let (actual, _) = digest_artifact(&artifact)?;
        assert_ne!(expected, actual);
        Ok(())
    }

    #[test]
    fn local_and_prebuilt_approvals_are_independent_and_bounded() {
        let phase = |wall_micros| crate::shared_index::report::PhaseReport {
            wall_micros,
            cpu_micros: None,
            end_peak_rss_bytes: None,
            bytes_read: 0,
            bytes_written: 0,
        };
        let graph = GraphSummary {
            files: 1_001,
            source_files: 1_001,
            source_bytes: 1_000,
            symbols: 1,
            edges: 0,
            call_sites: 0,
            fingerprint: "exact".to_owned(),
        };
        let cold = ColdRebuildReport {
            phase: phase(5_000),
            graph: graph.clone(),
            indexer_phase_peak_rss_bytes: None,
        };
        let population = CachePopulationReport {
            phase: phase(1_000),
            artifact_available: true,
            artifact_bytes: Some(2_000),
            lookup_or_store_rejection: Some("not_found".to_owned()),
            artifact_unavailable_detail: None,
        };
        let local = RestoreReport {
            phase: phase(1_000),
            origin: "disk_restore".to_owned(),
            graph,
            graph_verified: true,
        };

        let gates = evaluate_gates(&cold, &population, Some(&local), None);
        assert!(gates.approved_for_default_local_restore);
        assert!(!gates.approved_for_prebuilt_import);

        let oversized = CachePopulationReport {
            artifact_bytes: Some(2_001),
            ..population
        };
        let gates = evaluate_gates(&cold, &oversized, Some(&local), Some(&local));
        assert!(!gates.approved_for_default_local_restore);
        assert!(!gates.approved_for_prebuilt_import);
    }
}
