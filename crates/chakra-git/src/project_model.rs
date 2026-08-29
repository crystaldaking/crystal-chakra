//! Typed project model built from Cargo and Composer manifest evidence
//! (issue #41).
//!
//! The model promotes package/workspace identity, roots, dependencies, source
//! roles, and generated/vendor boundaries into Chakra-owned domain types. It
//! reuses the same bounded probing discipline as source classification
//! (bounded `cargo metadata` invocations, bounded manifest reads, operation
//! cancellation) and never leaks Cargo or Composer protocol structures into
//! the domain. Manifests that cannot be probed or parsed degrade to recorded
//! [`ProjectManifestIssue`] entries plus path-fallback units; sources no
//! ecosystem unit claims are grouped into deterministic top-level-directory
//! units.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::project::{
    MAX_PROJECT_ISSUES, MAX_PROJECT_UNITS, MAX_PROJECT_WORKSPACES, MAX_UNIT_DEPENDENCIES,
    MAX_UNIT_SOURCE_ROOTS, ProjectDependency, ProjectDependencyKind, ProjectManifestIssue,
    ProjectManifestIssueKind, ProjectModel, ProjectOwnership, ProjectSourceRoot, ProjectUnit,
    ProjectUnitId, ProjectUnitKind, ProjectWorkspace, ProjectWorkspaceKind,
};
use chakra_domain::source::SourceRole;
use chakra_domain::symbol::Language;

use crate::source_metadata::{
    COMMAND_TIMEOUT, MAX_CARGO_METADATA_INVOCATIONS, MAX_COMPOSER_MANIFEST_BYTES,
    MAX_COMPOSER_MANIFESTS, MetadataCommandError, capture_command, manifests_named, package_root,
    repository_path,
};
use crate::{DiscoveryError, source_language};

/// One Cargo package extracted from a bounded `cargo metadata` document.
#[derive(Debug)]
struct CargoPackageFacts {
    name: String,
    manifest: RepoRelativePath,
    root: Option<RepoRelativePath>,
    source_roots: Vec<ProjectSourceRoot>,
    dependencies: Vec<CargoDependencyFacts>,
}

#[derive(Debug)]
struct CargoDependencyFacts {
    name: String,
    kind: ProjectDependencyKind,
    path: Option<RepoRelativePath>,
}

/// Result of one bounded `cargo metadata` invocation.
#[derive(Debug)]
struct CargoMetadataScan {
    packages: Vec<CargoPackageFacts>,
    /// `None` when the document carried no resolvable `workspace_root`;
    /// the inner value is `None` for a workspace at the repository root.
    workspace_root: Option<Option<RepoRelativePath>>,
}

/// One Composer package extracted from a Git-visible `composer.json`.
#[derive(Debug)]
struct ComposerPackageFacts {
    name: String,
    manifest: RepoRelativePath,
    root: Option<RepoRelativePath>,
    source_roots: Vec<ProjectSourceRoot>,
    dependencies: Vec<ProjectDependency>,
}

fn cargo_target_role(kinds: &[serde_json::Value]) -> SourceRole {
    if kinds.iter().any(|kind| kind.as_str() == Some("test")) {
        SourceRole::Test
    } else if kinds.iter().any(|kind| kind.as_str() == Some("example")) {
        SourceRole::Example
    } else if kinds.iter().any(|kind| kind.as_str() == Some("bench")) {
        SourceRole::Bench
    } else {
        SourceRole::Production
    }
}

fn source_role_priority(role: SourceRole) -> u8 {
    match role {
        SourceRole::Production => 0,
        SourceRole::Test => 1,
        SourceRole::Example => 2,
        SourceRole::Bench => 3,
        SourceRole::Fixture | SourceRole::Generated | SourceRole::Vendor => 4,
    }
}

fn parent_directory(path: &RepoRelativePath) -> Option<RepoRelativePath> {
    let (directory, _) = path.as_str().rsplit_once('/')?;
    RepoRelativePath::new(directory).ok()
}

fn cargo_dependency_kind(value: Option<&serde_json::Value>) -> ProjectDependencyKind {
    match value.and_then(serde_json::Value::as_str) {
        Some("dev") => ProjectDependencyKind::Development,
        Some("build") => ProjectDependencyKind::Build,
        _ => ProjectDependencyKind::Normal,
    }
}

/// Parses one `cargo metadata --no-deps` document into typed package facts.
fn parse_cargo_metadata(root: &Path, output: &[u8]) -> Option<CargoMetadataScan> {
    let metadata: serde_json::Value = serde_json::from_slice(output).ok()?;
    let packages = metadata.get("packages")?.as_array()?;
    let workspace_root = metadata
        .get("workspace_root")
        .and_then(serde_json::Value::as_str)
        .and_then(|workspace| package_root(root, &Path::new(workspace).join("Cargo.toml")));
    let mut parsed = Vec::new();
    for package in packages {
        let name = package.get("name")?.as_str()?.to_owned();
        let manifest_path = PathBuf::from(package.get("manifest_path")?.as_str()?);
        let manifest = repository_path(root, &manifest_path)?;
        let root_path = package_root(root, &manifest_path)?;
        let mut source_roots: BTreeMap<Option<RepoRelativePath>, SourceRole> = BTreeMap::new();
        if let Some(targets) = package.get("targets").and_then(serde_json::Value::as_array) {
            for target in targets {
                let Some(kinds) = target.get("kind").and_then(serde_json::Value::as_array) else {
                    continue;
                };
                let role = cargo_target_role(kinds);
                let Some(source) = target
                    .get("src_path")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|path| repository_path(root, Path::new(path)))
                else {
                    continue;
                };
                source_roots
                    .entry(parent_directory(&source))
                    .and_modify(|current| {
                        if source_role_priority(role) < source_role_priority(*current) {
                            *current = role;
                        }
                    })
                    .or_insert(role);
            }
        }
        let mut dependencies = Vec::new();
        if let Some(declared) = package
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
        {
            for dependency in declared {
                let Some(name) = dependency.get("name").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                dependencies.push(CargoDependencyFacts {
                    name: name.to_owned(),
                    kind: cargo_dependency_kind(dependency.get("kind")),
                    path: dependency
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|path| repository_path(root, Path::new(path))),
                });
            }
        }
        dependencies
            .sort_by(|left, right| left.name.cmp(&right.name).then(left.kind.cmp(&right.kind)));
        dependencies.dedup_by(|candidate, retained| {
            candidate.name == retained.name && candidate.kind == retained.kind
        });
        parsed.push(CargoPackageFacts {
            name,
            manifest,
            root: root_path,
            source_roots: source_roots
                .into_iter()
                .map(|(root, role)| ProjectSourceRoot { root, role })
                .collect(),
            dependencies,
        });
    }
    Some(CargoMetadataScan {
        packages: parsed,
        workspace_root,
    })
}

/// Probes every Git-visible `Cargo.toml` with bounded `cargo metadata`
/// invocations, mirroring the classification probing discipline. Failed or
/// unparsable probes are recorded as manifest issues, never as panics.
fn cargo_metadata_scans(
    root: &Path,
    metadata_inputs: &[RepoRelativePath],
    operation: &OperationContext,
    issues: &mut Vec<ProjectManifestIssue>,
) -> Result<Vec<CargoMetadataScan>, OperationAbort> {
    let manifests = manifests_named(metadata_inputs, "Cargo.toml");
    let mut covered = BTreeSet::new();
    let mut scans = Vec::new();
    let mut invocations = 0_usize;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    for manifest in manifests {
        operation.check()?;
        if covered.contains(&manifest) || invocations == MAX_CARGO_METADATA_INVOCATIONS {
            continue;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        covered.insert(manifest.clone());
        invocations += 1;
        let manifest_path = root.join(manifest.as_str());
        let args = [
            OsString::from("metadata"),
            OsString::from("--format-version"),
            OsString::from("1"),
            OsString::from("--no-deps"),
            OsString::from("--offline"),
            OsString::from("--locked"),
            OsString::from("--manifest-path"),
            manifest_path.into_os_string(),
        ];
        let output = match capture_command(root, "cargo", &args, remaining, operation) {
            Ok(output) => output,
            Err(MetadataCommandError::Operation(error)) => return Err(error),
            Err(MetadataCommandError::Io) => {
                issues.push(ProjectManifestIssue {
                    manifest,
                    kind: ProjectManifestIssueKind::ProbeFailed,
                });
                continue;
            }
        };
        if !output.success || output.exceeded {
            issues.push(ProjectManifestIssue {
                manifest,
                kind: ProjectManifestIssueKind::ProbeFailed,
            });
            continue;
        }
        match parse_cargo_metadata(root, &output.stdout) {
            Some(scan) => {
                covered.extend(scan.packages.iter().map(|package| package.manifest.clone()));
                scans.push(scan);
            }
            None => issues.push(ProjectManifestIssue {
                manifest,
                kind: ProjectManifestIssueKind::MalformedContent,
            }),
        }
    }
    operation.check()?;
    Ok(scans)
}

fn composer_declared_paths(value: &serde_json::Value) -> Vec<&str> {
    if let Some(path) = value.as_str() {
        vec![path]
    } else {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .collect()
    }
}

/// Composer platform packages are runtime requirements, not project
/// dependency edges.
fn is_composer_platform_package(name: &str) -> bool {
    name == "php"
        || name == "hhvm"
        || name.starts_with("ext-")
        || name.starts_with("lib-")
        || name.starts_with("composer-")
}

fn composer_source_root(
    repository_root: &Path,
    manifest_path: &Path,
    declared: &str,
) -> Option<Option<RepoRelativePath>> {
    let directory = manifest_path.parent()?;
    let candidate = directory.join(declared);
    let relative = candidate.strip_prefix(repository_root).ok()?;
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => components.push(value.to_str()?.to_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if components.is_empty() {
        Some(None)
    } else {
        RepoRelativePath::new(components.join("/")).ok().map(Some)
    }
}

fn collect_composer_source_roots(
    repository_root: &Path,
    manifest_path: &Path,
    metadata: &serde_json::Value,
    section: &str,
    role: SourceRole,
    roots: &mut BTreeMap<Option<RepoRelativePath>, SourceRole>,
) {
    let Some(psr4) = metadata
        .get(section)
        .and_then(|autoload| autoload.get("psr-4"))
        .and_then(serde_json::Value::as_object)
    else {
        return;
    };
    for declared in psr4.values().flat_map(composer_declared_paths) {
        let Some(root) = composer_source_root(repository_root, manifest_path, declared) else {
            continue;
        };
        roots
            .entry(root)
            .and_modify(|current| {
                if source_role_priority(role) < source_role_priority(*current) {
                    *current = role;
                }
            })
            .or_insert(role);
    }
}

fn collect_composer_dependencies(
    metadata: &serde_json::Value,
    section: &str,
    kind: ProjectDependencyKind,
    dependencies: &mut BTreeSet<(String, ProjectDependencyKind)>,
) {
    let Some(required) = metadata.get(section).and_then(serde_json::Value::as_object) else {
        return;
    };
    for name in required.keys() {
        if !is_composer_platform_package(name) {
            dependencies.insert((name.clone(), kind));
        }
    }
}

/// Reads every Git-visible `composer.json` within the classification bounds.
/// Unreadable, oversized, or unparsable manifests are recorded as issues.
fn composer_package_facts(
    root: &Path,
    metadata_inputs: &[RepoRelativePath],
    operation: &OperationContext,
    issues: &mut Vec<ProjectManifestIssue>,
) -> Result<Vec<ComposerPackageFacts>, OperationAbort> {
    let mut packages = Vec::new();
    for manifest in manifests_named(metadata_inputs, "composer.json")
        .into_iter()
        .take(MAX_COMPOSER_MANIFESTS)
    {
        operation.check()?;
        let manifest_path = root.join(manifest.as_str());
        let parsed = (|| {
            let file = fs::File::open(&manifest_path).ok()?;
            let mut bytes = Vec::new();
            if file
                .take((MAX_COMPOSER_MANIFEST_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .is_err()
                || bytes.len() > MAX_COMPOSER_MANIFEST_BYTES
            {
                return None;
            }
            serde_json::from_slice::<serde_json::Value>(&bytes).ok()
        })();
        let Some(metadata) = parsed else {
            issues.push(ProjectManifestIssue {
                manifest: manifest.clone(),
                kind: ProjectManifestIssueKind::MalformedContent,
            });
            continue;
        };
        let fallback_name = manifest
            .as_str()
            .strip_suffix("/composer.json")
            .unwrap_or("repository");
        let package_name = metadata
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(fallback_name)
            .to_owned();
        let mut roots = BTreeMap::new();
        collect_composer_source_roots(
            root,
            &manifest_path,
            &metadata,
            "autoload",
            SourceRole::Production,
            &mut roots,
        );
        collect_composer_source_roots(
            root,
            &manifest_path,
            &metadata,
            "autoload-dev",
            SourceRole::Test,
            &mut roots,
        );
        let mut dependencies = BTreeSet::new();
        collect_composer_dependencies(
            &metadata,
            "require",
            ProjectDependencyKind::Normal,
            &mut dependencies,
        );
        collect_composer_dependencies(
            &metadata,
            "require-dev",
            ProjectDependencyKind::Development,
            &mut dependencies,
        );
        let Some(root_path) = package_root(root, &manifest_path) else {
            continue;
        };
        packages.push(ComposerPackageFacts {
            name: package_name,
            root: root_path,
            manifest,
            source_roots: roots
                .into_iter()
                .map(|(root, role)| ProjectSourceRoot { root, role })
                .collect(),
            dependencies: dependencies
                .into_iter()
                .map(|(name, kind)| ProjectDependency {
                    name,
                    kind,
                    target: None,
                })
                .collect(),
        });
    }
    operation.check()?;
    Ok(packages)
}

fn push_bounded<T>(items: &mut Vec<T>, item: T, limit: usize, omitted: &mut u64) {
    if items.len() < limit {
        items.push(item);
    } else {
        *omitted = omitted.saturating_add(1);
    }
}

fn bounded_source_roots(source_roots: &[ProjectSourceRoot]) -> (Vec<ProjectSourceRoot>, u64) {
    let omitted = source_roots.len().saturating_sub(MAX_UNIT_SOURCE_ROOTS) as u64;
    let mut retained = source_roots.to_vec();
    retained.truncate(MAX_UNIT_SOURCE_ROOTS);
    (retained, omitted)
}

fn bounded_dependencies(dependencies: Vec<ProjectDependency>) -> (Vec<ProjectDependency>, u64) {
    let omitted = dependencies.len().saturating_sub(MAX_UNIT_DEPENDENCIES) as u64;
    let mut retained = dependencies;
    retained.truncate(MAX_UNIT_DEPENDENCIES);
    (retained, omitted)
}

/// Builds the typed project model for one pinned Git inventory (issue #41).
///
/// Cargo workspaces/packages and Composer packages become typed units; every
/// other source falls back to a deterministic top-level-directory unit. All
/// collections are bounded by the domain `MAX_PROJECT_*` constants with the
/// cut amount counted in the matching `*_omitted` field.
pub fn discover_project_model_with_context(
    root: &Path,
    sources: &[RepoRelativePath],
    metadata_inputs: &[RepoRelativePath],
    operation: &OperationContext,
) -> Result<ProjectModel, DiscoveryError> {
    operation.check()?;
    let mut issues = Vec::new();
    let cargo_scans = cargo_metadata_scans(root, metadata_inputs, operation, &mut issues)?;
    let composer_packages = composer_package_facts(root, metadata_inputs, operation, &mut issues)?;

    // First pass: Cargo unit ids are derivable from (root, name) alone, so
    // path dependencies can resolve against every scanned package before any
    // unit is retained or cut by the unit bound.
    let cargo_units: BTreeMap<Option<RepoRelativePath>, ProjectUnitId> = cargo_scans
        .iter()
        .flat_map(|scan| scan.packages.iter())
        .map(|package| {
            (
                package.root.clone(),
                ProjectUnitId::new(
                    ProjectUnitKind::CargoPackage,
                    package.root.as_ref(),
                    &package.name,
                ),
            )
        })
        .collect();

    let mut units = Vec::new();
    let mut units_omitted = 0_u64;
    let mut workspaces = Vec::new();
    let mut workspaces_omitted = 0_u64;

    for scan in &cargo_scans {
        let mut members = Vec::new();
        for package in &scan.packages {
            let id = ProjectUnitId::new(
                ProjectUnitKind::CargoPackage,
                package.root.as_ref(),
                &package.name,
            );
            members.push(id.clone());
            let (source_roots, source_roots_omitted) = bounded_source_roots(&package.source_roots);
            let (dependencies, dependencies_omitted) = bounded_dependencies(
                package
                    .dependencies
                    .iter()
                    .map(|dependency| ProjectDependency {
                        name: dependency.name.clone(),
                        kind: dependency.kind,
                        target: dependency
                            .path
                            .as_ref()
                            .and_then(|path| cargo_units.get(&Some(path.clone())).cloned()),
                    })
                    .collect(),
            );
            push_bounded(
                &mut units,
                ProjectUnit {
                    id,
                    kind: ProjectUnitKind::CargoPackage,
                    name: package.name.clone(),
                    root: package.root.clone(),
                    manifest: Some(package.manifest.clone()),
                    language: Some(Language::Rust),
                    source_roots,
                    source_roots_omitted,
                    dependencies,
                    dependencies_omitted,
                },
                MAX_PROJECT_UNITS,
                &mut units_omitted,
            );
        }
        if let Some(workspace_root) = &scan.workspace_root {
            members.sort();
            members.dedup();
            push_bounded(
                &mut workspaces,
                ProjectWorkspace {
                    kind: ProjectWorkspaceKind::Cargo,
                    root: workspace_root.clone(),
                    members,
                },
                MAX_PROJECT_WORKSPACES,
                &mut workspaces_omitted,
            );
        }
    }

    for package in composer_packages {
        let id = ProjectUnitId::new(
            ProjectUnitKind::ComposerPackage,
            package.root.as_ref(),
            &package.name,
        );
        let (source_roots, source_roots_omitted) = bounded_source_roots(&package.source_roots);
        let (dependencies, dependencies_omitted) = bounded_dependencies(package.dependencies);
        push_bounded(
            &mut units,
            ProjectUnit {
                id,
                kind: ProjectUnitKind::ComposerPackage,
                name: package.name,
                root: package.root,
                manifest: Some(package.manifest),
                language: Some(Language::Php),
                source_roots,
                source_roots_omitted,
                dependencies,
                dependencies_omitted,
            },
            MAX_PROJECT_UNITS,
            &mut units_omitted,
        );
    }

    // Sources no ecosystem unit claims fall back to deterministic
    // top-level-directory units (issue #41 scope: other ecosystems use the
    // path fallback).
    let partial = ProjectModel {
        units: units.clone(),
        ..ProjectModel::default()
    };
    let mut fallback_roots: BTreeSet<Option<RepoRelativePath>> = BTreeSet::new();
    for path in sources {
        operation.check()?;
        let Some(language) = source_language(path.as_str()) else {
            continue;
        };
        if matches!(
            partial.ownership(path, language),
            ProjectOwnership::Unassigned
        ) {
            let fallback = path
                .as_str()
                .split('/')
                .next()
                .filter(|component| *component != path.as_str())
                .and_then(|component| RepoRelativePath::new(component).ok());
            fallback_roots.insert(fallback);
        }
    }
    for root in fallback_roots {
        let name = root
            .as_ref()
            .map_or("(root)", RepoRelativePath::as_str)
            .to_owned();
        push_bounded(
            &mut units,
            ProjectUnit {
                id: ProjectUnitId::new(ProjectUnitKind::PathFallback, root.as_ref(), &name),
                kind: ProjectUnitKind::PathFallback,
                name,
                root,
                manifest: None,
                language: None,
                source_roots: Vec::new(),
                source_roots_omitted: 0,
                dependencies: Vec::new(),
                dependencies_omitted: 0,
            },
            MAX_PROJECT_UNITS,
            &mut units_omitted,
        );
    }

    units.sort_by(|left, right| left.id.cmp(&right.id));
    units.dedup_by(|candidate, retained| candidate.id == retained.id);
    workspaces.sort_by(|left, right| left.root.cmp(&right.root));
    workspaces.dedup_by(|candidate, retained| {
        candidate.root == retained.root && candidate.kind == retained.kind
    });
    issues.sort_by(|left, right| left.manifest.cmp(&right.manifest));
    issues.dedup_by(|candidate, retained| {
        candidate.manifest == retained.manifest && candidate.kind == retained.kind
    });
    let issues_omitted = issues.len().saturating_sub(MAX_PROJECT_ISSUES) as u64;
    issues.truncate(MAX_PROJECT_ISSUES);

    operation.check()?;
    Ok(ProjectModel {
        workspaces,
        workspaces_omitted,
        units,
        units_omitted,
        issues,
        issues_omitted,
    })
}

#[cfg(test)]
mod tests;
