//! Discovery and budgeted read of workspace source files.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chakra_domain::indexing::{
    IndexBudgetKind, IndexCancellation, IndexCapability, IndexDegradation, IndexPhase,
    IndexPhaseMeasurement,
};
use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::OperationContext;
use chakra_domain::symbol::Language;
use chakra_engine::ProviderInput;
use tracing::warn;

use crate::adapter::{LanguageSources, registered_languages};

use super::resources::process_peak_rss_bytes;
use super::{
    IndexOptions, PhaseConcurrency, PhaseTimer, WorkspaceIndexError, WorkspaceLanguageSources,
    WorkspaceSourceScan, WorkspaceSources, measured_phase,
};

/// Compatibility helper using safe defaults.
pub fn scan_repository_sources(
    repository_root: &Path,
) -> Result<WorkspaceSources, WorkspaceIndexError> {
    Ok(scan_repository_sources_with_options(repository_root, &IndexOptions::default())?.sources)
}

pub fn scan_repository_sources_with_options(
    repository_root: &Path,
    options: &IndexOptions,
) -> Result<WorkspaceSourceScan, WorkspaceIndexError> {
    check_cancelled(&options.cancellation)?;
    let operation = OperationContext::from_cancellation(options.cancellation.clone());
    let inventory_started = PhaseTimer::start();
    let inventory = chakra_git::discover_workspace_inventory_in_worktree_with_context(
        repository_root,
        &operation,
    )?;
    let inventory_phase = measured_phase(
        IndexPhase::GitInventory,
        None,
        inventory_started,
        inventory.sources.len() as u64,
        0,
        PhaseConcurrency::SERIAL,
    );
    scan_discovered_sources_with_inventory_phase(
        repository_root,
        options,
        &inventory,
        inventory_phase,
        &mut FilesystemSourceLoader,
        &operation,
    )
}

pub(crate) trait WorkspaceSourceLoader {
    fn observe(&mut self, path: &RepoRelativePath, metadata: &fs::Metadata);

    fn observe_metadata(&mut self, path: &RepoRelativePath, metadata: &fs::Metadata);

    fn load(
        &mut self,
        absolute: &Path,
        path: &RepoRelativePath,
        metadata: &fs::Metadata,
        max_bytes: u64,
    ) -> Result<Arc<str>, WorkspaceIndexError>;
}

struct FilesystemSourceLoader;

impl WorkspaceSourceLoader for FilesystemSourceLoader {
    fn observe(&mut self, _path: &RepoRelativePath, _metadata: &fs::Metadata) {}

    fn observe_metadata(&mut self, _path: &RepoRelativePath, _metadata: &fs::Metadata) {}

    fn load(
        &mut self,
        absolute: &Path,
        path: &RepoRelativePath,
        _metadata: &fs::Metadata,
        max_bytes: u64,
    ) -> Result<Arc<str>, WorkspaceIndexError> {
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
        Ok(Arc::<str>::from(source))
    }
}

pub(crate) fn scan_discovered_sources_with_options(
    repository_root: &Path,
    options: &IndexOptions,
    inventory: &chakra_git::WorkspaceInventory,
    inventory_elapsed: Duration,
    loader: &mut impl WorkspaceSourceLoader,
    operation: &OperationContext,
) -> Result<WorkspaceSourceScan, WorkspaceIndexError> {
    scan_discovered_sources_with_inventory_phase(
        repository_root,
        options,
        inventory,
        phase(
            IndexPhase::GitInventory,
            None,
            inventory_elapsed,
            inventory.sources.len() as u64,
            0,
        ),
        loader,
        operation,
    )
}

fn scan_discovered_sources_with_inventory_phase(
    repository_root: &Path,
    options: &IndexOptions,
    inventory: &chakra_git::WorkspaceInventory,
    inventory_phase: IndexPhaseMeasurement,
    loader: &mut impl WorkspaceSourceLoader,
    operation: &OperationContext,
) -> Result<WorkspaceSourceScan, WorkspaceIndexError> {
    let budgets = options.budgets.validate()?;
    check_cancelled(&options.cancellation)?;
    operation
        .check()
        .map_err(|_| WorkspaceIndexError::Cancelled)?;
    let discovered_files = inventory.sources.len() as u64;
    let read_started = PhaseTimer::start();
    let mut files_by_language: BTreeMap<Language, BTreeMap<RepoRelativePath, Arc<str>>> =
        BTreeMap::new();
    let mut source_bytes = 0_u64;
    let mut oversized_files = 0_u64;
    let mut largest_file = 0_u64;
    let mut workspace_omitted = 0_u64;
    let mut workspace_observed = 0_u64;
    let mut unreadable_files = 0_u64;
    let mut unreadable_paths = Vec::new();

    for (index, path) in inventory.sources.iter().enumerate() {
        check_cancelled(&options.cancellation)?;
        if index as u64 >= budgets.max_files {
            continue;
        }
        let absolute = repository_root.join(path.as_str());
        // A file may vanish or become unreadable between inventory and read;
        // skip it instead of aborting the whole scan.
        let metadata = match fs::metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(source) => {
                let error = WorkspaceIndexError::Read {
                    path: path.clone(),
                    source,
                };
                warn!(error = %error, "skipping source file that cannot be inspected");
                unreadable_files = unreadable_files.saturating_add(1);
                unreadable_paths.push(path.clone());
                continue;
            }
        };
        loader.observe(path, &metadata);
        let measured_len = metadata.len();
        if measured_len > budgets.max_source_file_bytes {
            oversized_files = oversized_files.saturating_add(1);
            largest_file = largest_file.max(measured_len);
            continue;
        }
        if source_bytes.saturating_add(measured_len) > budgets.max_workspace_source_bytes {
            workspace_omitted = workspace_omitted.saturating_add(1);
            workspace_observed = workspace_observed.max(source_bytes.saturating_add(measured_len));
            continue;
        }
        let source = match loader.load(&absolute, path, &metadata, budgets.max_source_file_bytes) {
            Ok(source) => source,
            Err(error @ WorkspaceIndexError::Read { .. }) => {
                warn!(error = %error, "skipping source file that cannot be read");
                unreadable_files = unreadable_files.saturating_add(1);
                unreadable_paths.push(path.clone());
                continue;
            }
            Err(error) => return Err(error),
        };
        let actual_len = source.len() as u64;
        if actual_len > budgets.max_source_file_bytes {
            oversized_files = oversized_files.saturating_add(1);
            largest_file = largest_file.max(actual_len);
            continue;
        }
        if source_bytes.saturating_add(actual_len) > budgets.max_workspace_source_bytes {
            workspace_omitted = workspace_omitted.saturating_add(1);
            workspace_observed = workspace_observed.max(source_bytes.saturating_add(actual_len));
            continue;
        }
        source_bytes = source_bytes.saturating_add(actual_len);
        if let Some(language) = chakra_git::source_language(path.as_str()) {
            files_by_language
                .entry(language)
                .or_default()
                .insert(path.clone(), source);
        }
    }

    let mut provider_inputs = Vec::new();
    for path in &inventory.metadata_inputs {
        operation
            .check()
            .map_err(|_| WorkspaceIndexError::Cancelled)?;
        let absolute = repository_root.join(path.as_str());
        let metadata = match fs::metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(source) => {
                let error = WorkspaceIndexError::Read {
                    path: path.clone(),
                    source,
                };
                warn!(error = %error, "skipping metadata input that cannot be inspected");
                unreadable_files = unreadable_files.saturating_add(1);
                unreadable_paths.push(path.clone());
                continue;
            }
        };
        if let Some(input) = ProviderInput::from_metadata(
            path.clone(),
            chakra_git::metadata_languages(path.as_str())
                .iter()
                .copied(),
            &metadata,
        ) {
            provider_inputs.push(input);
        }
        loader.observe_metadata(path, &metadata);
    }

    let mut languages = Vec::new();
    let mut indexed_files = 0_u64;
    for language in registered_languages() {
        let files = files_by_language.remove(&language).unwrap_or_default();
        let metadata = chakra_git::classify_discovered_sources_with_context(
            repository_root,
            &inventory.sources,
            &inventory.metadata_inputs,
            language,
            operation,
        )?
        .into_iter()
        .filter(|source| files.contains_key(&source.path))
        .map(|source| (source.path, source.metadata))
        .collect();
        indexed_files = indexed_files.saturating_add(files.len() as u64);
        languages.push(WorkspaceLanguageSources {
            language,
            sources: LanguageSources { files, metadata },
        });
    }
    let mut degradations = Vec::new();
    if discovered_files > budgets.max_files {
        degradations.push(IndexDegradation {
            phase: IndexPhase::GitInventory,
            language: None,
            cause: IndexBudgetKind::Files,
            affected_capabilities: all_index_capabilities(),
            limit: budgets.max_files,
            observed: discovered_files,
            omitted: discovered_files.saturating_sub(budgets.max_files),
        });
    }
    if oversized_files > 0 {
        degradations.push(IndexDegradation {
            phase: IndexPhase::SourceRead,
            language: None,
            cause: IndexBudgetKind::SourceFileBytes,
            affected_capabilities: all_index_capabilities(),
            limit: budgets.max_source_file_bytes,
            observed: largest_file,
            omitted: oversized_files,
        });
    }
    if workspace_omitted > 0 {
        degradations.push(IndexDegradation {
            phase: IndexPhase::SourceRead,
            language: None,
            cause: IndexBudgetKind::WorkspaceSourceBytes,
            affected_capabilities: all_index_capabilities(),
            limit: budgets.max_workspace_source_bytes,
            observed: workspace_observed,
            omitted: workspace_omitted,
        });
    }
    let phases = vec![
        inventory_phase,
        measured_phase(
            IndexPhase::SourceRead,
            None,
            read_started,
            indexed_files,
            source_bytes,
            PhaseConcurrency::SERIAL,
        ),
    ];
    let project_model = chakra_git::discover_project_model_with_context(
        repository_root,
        &inventory.sources,
        &inventory.metadata_inputs,
        operation,
    )?;
    Ok(WorkspaceSourceScan {
        sources: WorkspaceSources { languages },
        provider_inputs,
        project_model,
        discovered_files,
        indexed_files,
        source_bytes,
        unreadable_files,
        unreadable_paths,
        degradations,
        phases,
    })
}
fn all_index_capabilities() -> Vec<IndexCapability> {
    vec![
        IndexCapability::FileInventory,
        IndexCapability::TextSearch,
        IndexCapability::Declarations,
        IndexCapability::Relationships,
        IndexCapability::CallSites,
    ]
}

pub(super) fn check_cancelled(cancellation: &IndexCancellation) -> Result<(), WorkspaceIndexError> {
    if cancellation.is_cancelled() {
        Err(WorkspaceIndexError::Cancelled)
    } else {
        Ok(())
    }
}

fn phase(
    phase: IndexPhase,
    language: Option<Language>,
    elapsed: Duration,
    work_items: u64,
    bytes: u64,
) -> IndexPhaseMeasurement {
    IndexPhaseMeasurement {
        phase,
        language,
        elapsed_micros: elapsed.as_micros().min(u128::from(u64::MAX)) as u64,
        cpu_micros: None,
        cpu_utilization_per_mille: None,
        work_items,
        bytes,
        effective_workers: u64::from(work_items > 0),
        peak_active_workers: u64::from(work_items > 0),
        peak_queue_depth: 0,
        rss_bytes: None,
        peak_rss_bytes: process_peak_rss_bytes(),
    }
}
#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io;
    use std::process::Command;

    use chakra_domain::indexing::IndexCancellation;
    use tempfile::TempDir;

    use super::*;

    fn repository() -> Result<TempDir, Box<dyn Error>> {
        let repository = TempDir::new()?;
        let status = Command::new("git")
            .current_dir(repository.path())
            .args(["init", "--quiet"])
            .status()?;
        if !status.success() {
            return Err("git init failed".into());
        }
        Ok(repository)
    }

    #[test]
    fn sources_vanishing_between_inventory_and_read_are_skipped() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        fs::write(repository.path().join("lib.rs"), "pub fn retained() {}\n")?;
        let inventory = chakra_git::WorkspaceInventory {
            sources: vec![
                RepoRelativePath::new("lib.rs")?,
                RepoRelativePath::new("vanished.rs")?,
            ],
            metadata_inputs: vec![RepoRelativePath::new("missing/Cargo.toml")?],
        };
        let operation = OperationContext::from_cancellation(IndexCancellation::default());
        let scan = scan_discovered_sources_with_options(
            repository.path(),
            &IndexOptions::default(),
            &inventory,
            Duration::ZERO,
            &mut FilesystemSourceLoader,
            &operation,
        )?;
        assert_eq!(scan.discovered_files, 2);
        assert_eq!(scan.indexed_files, 1);
        assert_eq!(scan.unreadable_files, 2);
        assert_eq!(
            scan.unreadable_paths,
            vec![
                RepoRelativePath::new("vanished.rs")?,
                RepoRelativePath::new("missing/Cargo.toml")?,
            ]
        );
        assert_eq!(scan.sources.file_count(Language::Rust), 1);
        Ok(())
    }

    #[test]
    fn per_file_read_failures_skip_but_other_loader_errors_abort() -> Result<(), Box<dyn Error>> {
        struct FlakyLoader {
            failing: RepoRelativePath,
            error: fn(&RepoRelativePath) -> WorkspaceIndexError,
        }

        impl WorkspaceSourceLoader for FlakyLoader {
            fn observe(&mut self, _path: &RepoRelativePath, _metadata: &fs::Metadata) {}

            fn observe_metadata(&mut self, _path: &RepoRelativePath, _metadata: &fs::Metadata) {}

            fn load(
                &mut self,
                absolute: &Path,
                path: &RepoRelativePath,
                metadata: &fs::Metadata,
                max_bytes: u64,
            ) -> Result<Arc<str>, WorkspaceIndexError> {
                if *path == self.failing {
                    return Err((self.error)(path));
                }
                FilesystemSourceLoader.load(absolute, path, metadata, max_bytes)
            }
        }

        let repository = repository()?;
        fs::write(repository.path().join("lib.rs"), "pub fn retained() {}\n")?;
        fs::write(repository.path().join("broken.rs"), "pub fn lost() {}\n")?;
        let inventory = chakra_git::WorkspaceInventory {
            sources: vec![
                RepoRelativePath::new("broken.rs")?,
                RepoRelativePath::new("lib.rs")?,
            ],
            metadata_inputs: Vec::new(),
        };

        let mut read_failure = FlakyLoader {
            failing: RepoRelativePath::new("broken.rs")?,
            error: |path| WorkspaceIndexError::Read {
                path: path.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream did not contain valid UTF-8",
                ),
            },
        };
        let operation = OperationContext::from_cancellation(IndexCancellation::default());
        let scan = scan_discovered_sources_with_options(
            repository.path(),
            &IndexOptions::default(),
            &inventory,
            Duration::ZERO,
            &mut read_failure,
            &operation,
        )?;
        assert_eq!(scan.indexed_files, 1);
        assert_eq!(scan.unreadable_files, 1);
        assert_eq!(
            scan.unreadable_paths,
            vec![RepoRelativePath::new("broken.rs")?]
        );

        let mut update_failure = FlakyLoader {
            failing: RepoRelativePath::new("broken.rs")?,
            error: |path| {
                WorkspaceIndexError::Update(format!("source `{path}` changed while reading"))
            },
        };
        let result = scan_discovered_sources_with_options(
            repository.path(),
            &IndexOptions::default(),
            &inventory,
            Duration::ZERO,
            &mut update_failure,
            &operation,
        );
        assert!(matches!(result, Err(WorkspaceIndexError::Update(_))));
        Ok(())
    }
}
