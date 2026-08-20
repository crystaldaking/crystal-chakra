//! Bounded Cargo/Composer-aware source classification with path fallback.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chakra_domain::location::RepoRelativePath;
use chakra_domain::operation::{OperationAbort, OperationContext};
use chakra_domain::source::{SourceClassification, SourceMetadata, SourcePackage, SourceRole};
use chakra_domain::symbol::Language;

use crate::{
    DiscoveryError, discover_workspace_inventory_in_worktree_with_context, resolve_repository_root,
};

const COMMAND_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const COMMAND_STDERR_LIMIT: usize = 16 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(1);
const MAX_CARGO_METADATA_INVOCATIONS: usize = 64;
const MAX_COMPOSER_MANIFESTS: usize = 64;
const MAX_COMPOSER_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_PACKAGE_JSON_MANIFESTS: usize = 256;
const MAX_PACKAGE_JSON_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_PYPROJECT_MANIFESTS: usize = 256;
const MAX_PYPROJECT_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_JAVA_BUILD_MANIFESTS: usize = 512;
const MAX_JAVA_BUILD_MANIFEST_BYTES: usize = 1024 * 1024;

/// One discovered source plus deterministic query metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedSource {
    pub path: RepoRelativePath,
    pub language: Language,
    pub metadata: SourceMetadata,
}

#[derive(Debug)]
struct BoundedRead {
    bytes: Vec<u8>,
    exceeded: bool,
}

#[derive(Debug)]
struct CommandOutput {
    success: bool,
    stdout: Vec<u8>,
    exceeded: bool,
}

#[derive(Debug)]
enum MetadataCommandError {
    Operation(OperationAbort),
    Io,
}

impl From<OperationAbort> for MetadataCommandError {
    fn from(value: OperationAbort) -> Self {
        Self::Operation(value)
    }
}

impl From<io::Error> for MetadataCommandError {
    fn from(_value: io::Error) -> Self {
        Self::Io
    }
}

#[derive(Debug, Clone)]
struct CargoPackage {
    scope: SourcePackage,
    target_roles: BTreeMap<RepoRelativePath, SourceRole>,
}

#[derive(Debug, Clone)]
struct ComposerRoot {
    package: SourcePackage,
    role: SourceRole,
}

/// npm-style package scope from a `package.json` (or a `tsconfig.json`
/// project boundary without a sibling `package.json`).
#[derive(Debug, Clone)]
struct PackageJsonRoot {
    package: SourcePackage,
}

/// Python package scope from a `pyproject.toml` (or a `setup.py`/`setup.cfg`
/// project boundary without a sibling `pyproject.toml`).
#[derive(Debug, Clone)]
struct PyprojectRoot {
    package: SourcePackage,
}

/// Which Java build tool contributed a project scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JavaBuildKind {
    Maven,
    Gradle,
}

/// Java project scope from a Maven `pom.xml` or a Gradle
/// `settings.gradle(.kts)` (or a `build.gradle(.kts)` project boundary
/// without one).
#[derive(Debug, Clone)]
struct JavaRoot {
    package: SourcePackage,
    build: JavaBuildKind,
}

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<BoundedRead> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut exceeded = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let retained = read.min(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..retained]);
        exceeded |= retained < read;
    }
    Ok(BoundedRead { bytes, exceeded })
}

fn terminate(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn capture_command(
    root: &Path,
    program: &str,
    args: &[OsString],
    timeout: Duration,
    operation: &OperationContext,
) -> Result<CommandOutput, MetadataCommandError> {
    operation.check()?;
    let mut child = Command::new(program)
        .current_dir(root)
        .env("LC_ALL", "C")
        .env("CARGO_NET_OFFLINE", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args)
        .spawn()?;
    let Some(stdout) = child.stdout.take() else {
        terminate(&mut child);
        return Err(io::Error::other("command stdout pipe is unavailable").into());
    };
    let Some(stderr) = child.stderr.take() else {
        terminate(&mut child);
        return Err(io::Error::other("command stderr pipe is unavailable").into());
    };
    let stdout_reader = match thread::Builder::new()
        .name("chakra-source-metadata-stdout".to_owned())
        .spawn(move || read_bounded(stdout, COMMAND_OUTPUT_LIMIT))
    {
        Ok(reader) => reader,
        Err(error) => {
            terminate(&mut child);
            return Err(error.into());
        }
    };
    let stderr_reader = match thread::Builder::new()
        .name("chakra-source-metadata-stderr".to_owned())
        .spawn(move || read_bounded(stderr, COMMAND_STDERR_LIMIT))
    {
        Ok(reader) => reader,
        Err(error) => {
            terminate(&mut child);
            let _ = stdout_reader.join();
            return Err(error.into());
        }
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Err(error) = operation.check() {
            terminate(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(error.into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(COMMAND_POLL_INTERVAL),
            Ok(None) => {
                terminate(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "source metadata command timed out",
                )
                .into());
            }
            Err(error) => {
                terminate(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(error.into());
            }
        }
    };
    // Join both pipe owners before propagating either result. Returning after
    // the first failed join would detach the other reader thread.
    let stdout = stdout_reader.join();
    let stderr = stderr_reader.join();
    let stdout = stdout.map_err(|_| io::Error::other("command stdout reader panicked"))??;
    let _stderr = stderr.map_err(|_| io::Error::other("command stderr reader panicked"))??;
    Ok(CommandOutput {
        success: status.success(),
        stdout: stdout.bytes,
        exceeded: stdout.exceeded,
    })
}

fn repository_path(root: &Path, path: &Path) -> Option<RepoRelativePath> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let relative = absolute.strip_prefix(root).ok()?;
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => components.push(value.to_str()?.to_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if components.is_empty() {
        None
    } else {
        RepoRelativePath::new(components.join("/")).ok()
    }
}

fn package_root(root: &Path, manifest: &Path) -> Option<Option<RepoRelativePath>> {
    let directory = manifest.parent()?;
    if directory == root {
        Some(None)
    } else {
        repository_path(root, directory).map(Some)
    }
}

fn manifests_named(
    metadata_inputs: &[RepoRelativePath],
    manifest_name: &str,
) -> Vec<RepoRelativePath> {
    let suffix = format!("/{manifest_name}");
    let mut manifests: Vec<_> = metadata_inputs
        .iter()
        .filter(|path| path.as_str() == manifest_name || path.as_str().ends_with(suffix.as_str()))
        .cloned()
        .collect();
    manifests.sort_by(|a, b| {
        a.as_str()
            .matches('/')
            .count()
            .cmp(&b.as_str().matches('/').count())
            .then(a.cmp(b))
    });
    manifests.dedup();
    manifests
}

fn target_role(kinds: &[serde_json::Value]) -> SourceRole {
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

fn target_role_priority(role: SourceRole) -> u8 {
    match role {
        SourceRole::Production => 0,
        SourceRole::Test => 1,
        SourceRole::Example => 2,
        SourceRole::Bench => 3,
        SourceRole::Fixture | SourceRole::Generated | SourceRole::Vendor => 4,
    }
}

fn parse_metadata(
    root: &Path,
    output: &[u8],
) -> Option<(Vec<CargoPackage>, BTreeSet<RepoRelativePath>)> {
    let metadata: serde_json::Value = serde_json::from_slice(output).ok()?;
    let packages = metadata.get("packages")?.as_array()?;
    let mut parsed = Vec::new();
    let mut manifests = BTreeSet::new();
    for package in packages {
        let name = package.get("name")?.as_str()?.to_owned();
        let manifest_path = PathBuf::from(package.get("manifest_path")?.as_str()?);
        let manifest = repository_path(root, &manifest_path)?;
        manifests.insert(manifest.clone());
        let root_path = package_root(root, &manifest_path)?;
        let mut target_roles = BTreeMap::new();
        for target in package.get("targets")?.as_array()? {
            let Some(kinds) = target.get("kind").and_then(serde_json::Value::as_array) else {
                continue;
            };
            let role = target_role(kinds);
            let Some(path) = target
                .get("src_path")
                .and_then(serde_json::Value::as_str)
                .and_then(|path| repository_path(root, Path::new(path)))
            else {
                continue;
            };
            target_roles
                .entry(path)
                .and_modify(|current| {
                    if target_role_priority(role) < target_role_priority(*current) {
                        *current = role;
                    }
                })
                .or_insert(role);
        }
        parsed.push(CargoPackage {
            scope: SourcePackage {
                name,
                root: root_path,
            },
            target_roles,
        });
    }
    Some((parsed, manifests))
}

fn cargo_packages(
    root: &Path,
    metadata_inputs: &[RepoRelativePath],
    operation: &OperationContext,
) -> Result<Vec<CargoPackage>, OperationAbort> {
    let manifests = manifests_named(metadata_inputs, "Cargo.toml");
    let mut covered = BTreeSet::new();
    let mut packages: BTreeMap<(Option<RepoRelativePath>, String), CargoPackage> = BTreeMap::new();
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
            Err(MetadataCommandError::Io) => continue,
        };
        if !output.success || output.exceeded {
            continue;
        }
        let Some((discovered, package_manifests)) = parse_metadata(root, &output.stdout) else {
            continue;
        };
        covered.extend(package_manifests);
        for package in discovered {
            packages.insert(
                (package.scope.root.clone(), package.scope.name.clone()),
                package,
            );
        }
    }
    operation.check()?;
    Ok(packages.into_values().collect())
}

fn composer_source_root(
    repository_root: &Path,
    manifest_path: &Path,
    declared: &str,
) -> Option<Option<RepoRelativePath>> {
    let manifest_directory = manifest_path.parent()?;
    let candidate = manifest_directory.join(declared);
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

fn collect_composer_section(
    repository_root: &Path,
    manifest_path: &Path,
    package_name: &str,
    metadata: &serde_json::Value,
    section: &str,
    role: SourceRole,
    roots: &mut BTreeMap<(Option<RepoRelativePath>, String), ComposerRoot>,
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
        let key = (root.clone(), package_name.to_owned());
        roots
            .entry(key)
            .and_modify(|current| {
                if target_role_priority(role) < target_role_priority(current.role) {
                    current.role = role;
                }
            })
            .or_insert_with(|| ComposerRoot {
                package: SourcePackage {
                    name: package_name.to_owned(),
                    root,
                },
                role,
            });
    }
}

fn composer_packages(
    root: &Path,
    metadata_inputs: &[RepoRelativePath],
    operation: &OperationContext,
) -> Result<Vec<ComposerRoot>, OperationAbort> {
    let mut packages = BTreeMap::new();
    for manifest in manifests_named(metadata_inputs, "composer.json")
        .into_iter()
        .take(MAX_COMPOSER_MANIFESTS)
    {
        operation.check()?;
        let manifest_path = root.join(manifest.as_str());
        let Ok(file) = fs::File::open(&manifest_path) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take((MAX_COMPOSER_MANIFEST_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() > MAX_COMPOSER_MANIFEST_BYTES
        {
            continue;
        }
        let Ok(metadata) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
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
            .unwrap_or(fallback_name);
        collect_composer_section(
            root,
            &manifest_path,
            package_name,
            &metadata,
            "autoload",
            SourceRole::Production,
            &mut packages,
        );
        collect_composer_section(
            root,
            &manifest_path,
            package_name,
            &metadata,
            "autoload-dev",
            SourceRole::Test,
            &mut packages,
        );
    }
    operation.check()?;
    Ok(packages.into_values().collect())
}

fn is_inside(path: &RepoRelativePath, root: Option<&RepoRelativePath>) -> bool {
    let Some(root) = root else {
        return true;
    };
    path == root
        || path
            .as_str()
            .strip_prefix(root.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn read_json_manifest(root: &Path, manifest: &RepoRelativePath) -> Option<serde_json::Value> {
    let manifest_path = root.join(manifest.as_str());
    let file = fs::File::open(&manifest_path).ok()?;
    let mut bytes = Vec::new();
    if file
        .take((MAX_PACKAGE_JSON_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_PACKAGE_JSON_MANIFEST_BYTES
    {
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(&bytes).ok()
}

fn manifest_directory(manifest: &RepoRelativePath, file_name: &str) -> Option<RepoRelativePath> {
    let suffix = format!("/{file_name}");
    let directory = manifest.as_str().strip_suffix(suffix.as_str())?;
    RepoRelativePath::new(directory).ok()
}

/// Collects npm-style package scopes. Every `package.json` is a package
/// root (workspaces are covered because each workspace member carries its
/// own manifest); a `tsconfig.json` or `jsconfig.json` without a sibling
/// `package.json` is a project boundary named after its directory.
fn package_json_packages(
    root: &Path,
    metadata_inputs: &[RepoRelativePath],
    operation: &OperationContext,
) -> Result<Vec<PackageJsonRoot>, OperationAbort> {
    let package_manifests = manifests_named(metadata_inputs, "package.json");
    let mut packages = Vec::new();
    let mut covered = BTreeSet::new();
    for manifest in package_manifests.iter().take(MAX_PACKAGE_JSON_MANIFESTS) {
        operation.check()?;
        let directory = manifest_directory(manifest, "package.json");
        if let Some(directory) = &directory {
            covered.insert(directory.clone());
        }
        let metadata = read_json_manifest(root, manifest);
        let fallback_name = manifest
            .as_str()
            .strip_suffix("/package.json")
            .unwrap_or("repository");
        let name = metadata
            .as_ref()
            .and_then(|metadata| metadata.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(fallback_name);
        packages.push(PackageJsonRoot {
            package: SourcePackage {
                name: name.to_owned(),
                root: directory,
            },
        });
    }
    for boundary in ["tsconfig.json", "jsconfig.json"] {
        for config in manifests_named(metadata_inputs, boundary)
            .iter()
            .take(MAX_PACKAGE_JSON_MANIFESTS)
        {
            operation.check()?;
            let Some(directory) = manifest_directory(config, boundary) else {
                continue;
            };
            if covered.contains(&directory) {
                continue;
            }
            covered.insert(directory.clone());
            packages.push(PackageJsonRoot {
                package: SourcePackage {
                    name: directory.as_str().to_owned(),
                    root: Some(directory),
                },
            });
        }
    }
    operation.check()?;
    Ok(packages)
}

fn read_text_manifest(root: &Path, manifest: &RepoRelativePath) -> Option<String> {
    let manifest_path = root.join(manifest.as_str());
    let file = fs::File::open(&manifest_path).ok()?;
    let mut bytes = Vec::new();
    if file
        .take((MAX_PYPROJECT_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_PYPROJECT_MANIFEST_BYTES
    {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Minimal `[project]` `name` extraction from `pyproject.toml` text. A full
/// TOML parser is deliberately not a dependency for one bounded string
/// field; anything unparseable falls back to the directory name.
fn pyproject_name(text: &str) -> Option<&str> {
    let mut in_project = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_project = trimmed == "[project]";
            continue;
        }
        if !in_project {
            continue;
        }
        let Some(value) = trimmed.strip_prefix("name") else {
            continue;
        };
        let Some(value) = value.trim_start().strip_prefix('=') else {
            continue;
        };
        let name = value.trim().trim_matches('"').trim_matches('\'').trim();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Collects Python package scopes. Every `pyproject.toml` is a package root
/// (PEP 621 `[project].name` when parseable); a `setup.py` or `setup.cfg`
/// without a sibling `pyproject.toml` is a project boundary named after its
/// directory.
fn pyproject_packages(
    root: &Path,
    metadata_inputs: &[RepoRelativePath],
    operation: &OperationContext,
) -> Result<Vec<PyprojectRoot>, OperationAbort> {
    let mut packages = Vec::new();
    let mut covered = BTreeSet::new();
    for manifest in manifests_named(metadata_inputs, "pyproject.toml")
        .iter()
        .take(MAX_PYPROJECT_MANIFESTS)
    {
        operation.check()?;
        let directory = manifest_directory(manifest, "pyproject.toml");
        if let Some(directory) = &directory {
            covered.insert(directory.clone());
        }
        let fallback_name = manifest
            .as_str()
            .strip_suffix("/pyproject.toml")
            .unwrap_or("repository");
        let text = read_text_manifest(root, manifest);
        let name = text
            .as_deref()
            .and_then(pyproject_name)
            .unwrap_or(fallback_name);
        packages.push(PyprojectRoot {
            package: SourcePackage {
                name: name.to_owned(),
                root: directory,
            },
        });
    }
    let mut root_covered = packages
        .iter()
        .any(|package| package.package.root.is_none());
    for boundary in ["setup.py", "setup.cfg"] {
        for manifest in manifests_named(metadata_inputs, boundary)
            .iter()
            .take(MAX_PYPROJECT_MANIFESTS)
        {
            operation.check()?;
            match manifest_directory(manifest, boundary) {
                Some(directory) => {
                    if covered.contains(&directory) {
                        continue;
                    }
                    covered.insert(directory.clone());
                    packages.push(PyprojectRoot {
                        package: SourcePackage {
                            name: directory.as_str().to_owned(),
                            root: Some(directory),
                        },
                    });
                }
                None => {
                    if root_covered {
                        continue;
                    }
                    root_covered = true;
                    packages.push(PyprojectRoot {
                        package: SourcePackage {
                            name: "repository".to_owned(),
                            root: None,
                        },
                    });
                }
            }
        }
    }
    operation.check()?;
    Ok(packages)
}

/// TypeScript and JavaScript test conventions beyond the language-neutral
/// path fallback: `__tests__/` directories and `*.test.*` / `*.spec.*` file
/// stems.
fn typescript_path_role(path: &RepoRelativePath) -> SourceRole {
    let fallback = SourceMetadata::path_fallback(path);
    if fallback.role != SourceRole::Production {
        return fallback.role;
    }
    if path
        .as_str()
        .split('/')
        .any(|component| component.eq_ignore_ascii_case("__tests__"))
    {
        return SourceRole::Test;
    }
    let is_test_stem = path
        .as_str()
        .rsplit('/')
        .next()
        .is_some_and(|file| file.contains(".test.") || file.contains(".spec."));
    if is_test_stem {
        SourceRole::Test
    } else {
        SourceRole::Production
    }
}

fn classify_typescript(path: &RepoRelativePath, packages: &[PackageJsonRoot]) -> SourceMetadata {
    let role = typescript_path_role(path);
    let package = packages
        .iter()
        .filter(|package| is_inside(path, package.package.root.as_ref()))
        .max_by_key(|package| {
            package
                .package
                .root
                .as_ref()
                .map_or(0, |root| root.as_str().len())
        });
    let Some(package) = package else {
        return SourceMetadata {
            role,
            classification: SourceClassification::PathFallback,
            package: None,
        };
    };
    SourceMetadata {
        role,
        classification: SourceClassification::PackageJsonMetadata,
        package: Some(package.package.clone()),
    }
}

/// Python test conventions beyond the language-neutral path fallback:
/// `test_*.py` and `*_test.py` file stems (pytest/unittest discovery rules).
fn python_path_role(path: &RepoRelativePath) -> SourceRole {
    let fallback = SourceMetadata::path_fallback(path);
    if fallback.role != SourceRole::Production {
        return fallback.role;
    }
    let is_test_stem = path
        .as_str()
        .rsplit('/')
        .next()
        .and_then(|file| {
            file.strip_suffix(".py")
                .or_else(|| file.strip_suffix(".pyi"))
        })
        .is_some_and(|stem| stem.starts_with("test_") || stem.ends_with("_test"));
    if is_test_stem {
        SourceRole::Test
    } else {
        SourceRole::Production
    }
}

fn classify_python(path: &RepoRelativePath, packages: &[PyprojectRoot]) -> SourceMetadata {
    let role = python_path_role(path);
    let package = packages
        .iter()
        .filter(|package| is_inside(path, package.package.root.as_ref()))
        .max_by_key(|package| {
            package
                .package
                .root
                .as_ref()
                .map_or(0, |root| root.as_str().len())
        });
    let Some(package) = package else {
        return SourceMetadata {
            role,
            classification: SourceClassification::PathFallback,
            package: None,
        };
    };
    SourceMetadata {
        role,
        classification: SourceClassification::PyprojectMetadata,
        package: Some(package.package.clone()),
    }
}

/// Minimal `<artifactId>` extraction from `pom.xml` text with the `<parent>`
/// block excluded. A full XML parser is deliberately not a dependency for
/// one bounded string field; anything unparseable falls back to the
/// directory name.
fn pom_artifact_id(text: &str) -> Option<&str> {
    let parent = text.find("<parent>").zip(text.find("</parent>"));
    let search_from = match parent {
        Some((_, end)) => end + "</parent>".len(),
        None => 0,
    };
    let rest = text.get(search_from..)?;
    let start = rest.find("<artifactId>")? + "<artifactId>".len();
    let end = rest[start..].find("</artifactId>")?;
    let name = rest[start..start + end].trim();
    (!name.is_empty()).then_some(name)
}

/// Minimal `rootProject.name` extraction from a `settings.gradle(.kts)`:
/// `rootProject.name = "x"` (or single quotes). Anything unparseable falls
/// back to the directory name.
fn gradle_root_project_name(text: &str) -> Option<&str> {
    for line in text.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix("rootProject.name") else {
            continue;
        };
        let Some(value) = value.trim_start().strip_prefix('=') else {
            continue;
        };
        let name = value.trim().trim_matches('"').trim_matches('\'').trim();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

fn read_java_build_manifest(root: &Path, manifest: &RepoRelativePath) -> Option<String> {
    let manifest_path = root.join(manifest.as_str());
    let file = fs::File::open(&manifest_path).ok()?;
    let mut bytes = Vec::new();
    if file
        .take((MAX_JAVA_BUILD_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_JAVA_BUILD_MANIFEST_BYTES
    {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Collects Java project scopes. Every `pom.xml` is a Maven module root
/// (`<artifactId>` when parseable); every `settings.gradle(.kts)` is a Gradle
/// project root (`rootProject.name` when parseable); a
/// `build.gradle(.kts)` in a directory without a Gradle settings file is a
/// project boundary named after its directory.
fn java_packages(
    root: &Path,
    metadata_inputs: &[RepoRelativePath],
    operation: &OperationContext,
) -> Result<Vec<JavaRoot>, OperationAbort> {
    let mut packages = Vec::new();
    let mut gradle_covered = BTreeSet::new();
    for manifest in manifests_named(metadata_inputs, "pom.xml")
        .iter()
        .take(MAX_JAVA_BUILD_MANIFESTS)
    {
        operation.check()?;
        let directory = manifest_directory(manifest, "pom.xml");
        let fallback_name = manifest
            .as_str()
            .strip_suffix("/pom.xml")
            .unwrap_or("repository");
        let text = read_java_build_manifest(root, manifest);
        let name = text
            .as_deref()
            .and_then(pom_artifact_id)
            .unwrap_or(fallback_name);
        packages.push(JavaRoot {
            package: SourcePackage {
                name: name.to_owned(),
                root: directory,
            },
            build: JavaBuildKind::Maven,
        });
    }
    for settings in ["settings.gradle", "settings.gradle.kts"] {
        for manifest in manifests_named(metadata_inputs, settings)
            .iter()
            .take(MAX_JAVA_BUILD_MANIFESTS)
        {
            operation.check()?;
            let directory = manifest_directory(manifest, settings);
            if let Some(directory) = &directory {
                gradle_covered.insert(directory.clone());
            }
            let fallback_name = manifest
                .as_str()
                .strip_suffix(&format!("/{settings}"))
                .unwrap_or("repository");
            let text = read_java_build_manifest(root, manifest);
            let name = text
                .as_deref()
                .and_then(gradle_root_project_name)
                .unwrap_or(fallback_name);
            packages.push(JavaRoot {
                package: SourcePackage {
                    name: name.to_owned(),
                    root: directory,
                },
                build: JavaBuildKind::Gradle,
            });
        }
    }
    let mut root_covered = packages
        .iter()
        .any(|package| package.build == JavaBuildKind::Gradle && package.package.root.is_none());
    for build in ["build.gradle", "build.gradle.kts"] {
        for manifest in manifests_named(metadata_inputs, build)
            .iter()
            .take(MAX_JAVA_BUILD_MANIFESTS)
        {
            operation.check()?;
            match manifest_directory(manifest, build) {
                Some(directory) => {
                    if gradle_covered.contains(&directory) {
                        continue;
                    }
                    gradle_covered.insert(directory.clone());
                    packages.push(JavaRoot {
                        package: SourcePackage {
                            name: directory.as_str().to_owned(),
                            root: Some(directory),
                        },
                        build: JavaBuildKind::Gradle,
                    });
                }
                None => {
                    if root_covered {
                        continue;
                    }
                    root_covered = true;
                    packages.push(JavaRoot {
                        package: SourcePackage {
                            name: "repository".to_owned(),
                            root: None,
                        },
                        build: JavaBuildKind::Gradle,
                    });
                }
            }
        }
    }
    operation.check()?;
    Ok(packages)
}

/// Java test conventions beyond the language-neutral path fallback: the
/// Maven/Gradle `src/test/java` source root and the JUnit `Test*.java` /
/// `*Test.java` / `*Tests.java` file-name conventions.
fn java_path_role(path: &RepoRelativePath) -> SourceRole {
    let fallback = SourceMetadata::path_fallback(path);
    if fallback.role != SourceRole::Production {
        return fallback.role;
    }
    let components: Vec<&str> = path.as_str().split('/').collect();
    if components.starts_with(&["src", "test", "java"]) {
        return SourceRole::Test;
    }
    let is_test_stem = path
        .as_str()
        .rsplit('/')
        .next()
        .and_then(|file| file.strip_suffix(".java"))
        .is_some_and(|stem| {
            stem.starts_with("Test") || stem.ends_with("Test") || stem.ends_with("Tests")
        });
    if is_test_stem {
        SourceRole::Test
    } else {
        SourceRole::Production
    }
}

fn classify_java(path: &RepoRelativePath, packages: &[JavaRoot]) -> SourceMetadata {
    let role = java_path_role(path);
    let package = packages
        .iter()
        .filter(|package| is_inside(path, package.package.root.as_ref()))
        .max_by_key(|package| {
            package
                .package
                .root
                .as_ref()
                .map_or(0, |root| root.as_str().len())
        });
    let Some(package) = package else {
        return SourceMetadata {
            role,
            classification: SourceClassification::PathFallback,
            package: None,
        };
    };
    SourceMetadata {
        role,
        classification: match package.build {
            JavaBuildKind::Maven => SourceClassification::MavenMetadata,
            JavaBuildKind::Gradle => SourceClassification::GradleMetadata,
        },
        package: Some(package.package.clone()),
    }
}

fn classify_rust(path: &RepoRelativePath, packages: &[CargoPackage]) -> SourceMetadata {
    let fallback = SourceMetadata::path_fallback(path);
    let package = packages
        .iter()
        .filter(|package| is_inside(path, package.scope.root.as_ref()))
        .max_by_key(|package| {
            package
                .scope
                .root
                .as_ref()
                .map_or(0, |root| root.as_str().len())
        });
    let Some(package) = package else {
        return fallback;
    };
    SourceMetadata {
        role: package
            .target_roles
            .get(path)
            .copied()
            .unwrap_or(fallback.role),
        classification: SourceClassification::CargoMetadata,
        package: Some(package.scope.clone()),
    }
}

fn classify_php(path: &RepoRelativePath, roots: &[ComposerRoot]) -> SourceMetadata {
    let fallback = SourceMetadata::path_fallback(path);
    let root = roots
        .iter()
        .filter(|root| is_inside(path, root.package.root.as_ref()))
        .max_by_key(|root| {
            root.package
                .root
                .as_ref()
                .map_or(0, |path| path.as_str().len())
        });
    let Some(root) = root else {
        return fallback;
    };
    SourceMetadata {
        role: if fallback.role == SourceRole::Production {
            root.role
        } else {
            fallback.role
        },
        classification: SourceClassification::ComposerMetadata,
        package: Some(root.package.clone()),
    }
}

/// Discovers one language and attaches bounded Cargo/Composer/path metadata
/// without excluding any source role.
pub fn discover_classified_sources(
    candidate: &Path,
    language: Language,
) -> Result<Vec<ClassifiedSource>, DiscoveryError> {
    let root = resolve_repository_root(candidate)?;
    let operation = OperationContext::unbounded();
    let inventory = discover_workspace_inventory_in_worktree_with_context(&root, &operation)?;
    classify_discovered_sources_with_context(
        &root,
        &inventory.sources,
        &inventory.metadata_inputs,
        language,
        &operation,
    )
}

/// Classifies a language from the already pinned shared Git inventory.
///
/// This avoids rediscovering Rust and PHP independently and lets live
/// reconciliation bind metadata subprocesses to the owning query operation.
pub fn classify_discovered_sources_with_context(
    root: &Path,
    sources: &[RepoRelativePath],
    metadata_inputs: &[RepoRelativePath],
    language: Language,
    operation: &OperationContext,
) -> Result<Vec<ClassifiedSource>, DiscoveryError> {
    operation.check()?;
    let files: Vec<_> = sources
        .iter()
        .filter(|path| crate::source_language(path.as_str()) == Some(language))
        .cloned()
        .collect();
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let cargo = if language == Language::Rust {
        Some(cargo_packages(root, metadata_inputs, operation)?)
    } else {
        None
    };
    let composer = if language == Language::Php {
        Some(composer_packages(root, metadata_inputs, operation)?)
    } else {
        None
    };
    let package_json = if matches!(language, Language::TypeScript | Language::JavaScript) {
        Some(package_json_packages(root, metadata_inputs, operation)?)
    } else {
        None
    };
    let pyproject = if language == Language::Python {
        Some(pyproject_packages(root, metadata_inputs, operation)?)
    } else {
        None
    };
    let java = if language == Language::Java {
        Some(java_packages(root, metadata_inputs, operation)?)
    } else {
        None
    };
    let mut classified = Vec::with_capacity(files.len());
    for path in files {
        operation.check()?;
        classified.push(ClassifiedSource {
            metadata: match language {
                Language::Rust => classify_rust(&path, cargo.as_deref().unwrap_or_default()),
                Language::Php => classify_php(&path, composer.as_deref().unwrap_or_default()),
                Language::TypeScript | Language::JavaScript => {
                    classify_typescript(&path, package_json.as_deref().unwrap_or_default())
                }
                Language::Python => {
                    classify_python(&path, pyproject.as_deref().unwrap_or_default())
                }
                Language::Java => classify_java(&path, java.as_deref().unwrap_or_default()),
            },
            path,
            language,
        });
    }
    Ok(classified)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn command(root: &Path, program: &str, args: &[&str]) -> Result<(), Box<dyn Error>> {
        let output = Command::new(program)
            .current_dir(root)
            .args(args)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{program} {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into())
        }
    }

    fn write(root: &Path, path: &str, contents: &str) -> Result<(), Box<dyn Error>> {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
        Ok(())
    }

    fn repository() -> Result<TempDir, Box<dyn Error>> {
        let repository = TempDir::new()?;
        let root = repository.path();
        command(root, "git", &["init", "--quiet"])?;
        command(
            root,
            "git",
            &["config", "user.email", "tests@example.invalid"],
        )?;
        command(root, "git", &["config", "user.name", "Chakra Tests"])?;
        Ok(repository)
    }

    #[test]
    fn cargo_workspace_roles_packages_and_fallback_are_preserved() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/core\", \"crates/nested/tool\"]\nresolver = \"3\"\n",
        )?;
        for (directory, name) in [("crates/core", "core"), ("crates/nested/tool", "tool")] {
            let custom_lib = if name == "core" {
                "\n[lib]\npath = \"fixtures/primary.rs\"\n"
            } else {
                ""
            };
            write(
                root,
                &format!("{directory}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{custom_lib}"
                ),
            )?;
            write(
                root,
                &format!("{directory}/src/lib.rs"),
                "pub fn lib() {}\n",
            )?;
        }
        for (path, source) in [
            ("crates/core/fixtures/primary.rs", "pub fn primary() {}\n"),
            ("crates/core/tests/api.rs", "fn integration() {}\n"),
            ("crates/core/examples/demo.rs", "fn main() {}\n"),
            ("crates/core/benches/read.rs", "fn bench() {}\n"),
            ("crates/core/tests/fixtures/input.rs", "fn fixture() {}\n"),
            ("crates/core/src/generated/schema.rs", "fn generated() {}\n"),
            ("vendor/tracked.rs", "fn vendored() {}\n"),
            ("standalone/lib.rs", "fn fallback() {}\n"),
        ] {
            write(root, path, source)?;
        }
        write(
            root,
            "tools/independent/Cargo.toml",
            "[workspace]\n\n[package]\nname = \"independent\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        write(
            root,
            "tools/independent/src/lib.rs",
            "pub fn independent() {}\n",
        )?;
        write(root, ".gitignore", "ignored-generated/\n")?;
        write(root, "ignored-generated/output.rs", "fn ignored() {}\n")?;
        command(root, "cargo", &["generate-lockfile", "--offline"])?;
        command(
            root,
            "cargo",
            &[
                "generate-lockfile",
                "--offline",
                "--manifest-path",
                "tools/independent/Cargo.toml",
            ],
        )?;
        command(root, "git", &["add", "."])?;

        let sources = discover_classified_sources(root, Language::Rust)?;
        let by_path: BTreeMap<_, _> = sources
            .iter()
            .map(|source| (source.path.as_str(), &source.metadata))
            .collect();
        let core = by_path["crates/core/src/lib.rs"];
        assert_eq!(core.role, SourceRole::Production);
        assert_eq!(core.classification, SourceClassification::CargoMetadata);
        assert_eq!(
            core.package.as_ref().map(|package| package.name.as_str()),
            Some("core")
        );
        assert_eq!(
            by_path["crates/core/fixtures/primary.rs"].role,
            SourceRole::Production,
            "an exact Cargo production target overrides its fixture-like path"
        );
        assert_eq!(by_path["crates/core/tests/api.rs"].role, SourceRole::Test);
        assert_eq!(
            by_path["crates/core/examples/demo.rs"].role,
            SourceRole::Example
        );
        assert_eq!(
            by_path["crates/core/benches/read.rs"].role,
            SourceRole::Bench
        );
        assert_eq!(
            by_path["crates/core/tests/fixtures/input.rs"].role,
            SourceRole::Fixture
        );
        assert_eq!(
            by_path["crates/core/src/generated/schema.rs"].role,
            SourceRole::Generated
        );
        assert_eq!(by_path["vendor/tracked.rs"].role, SourceRole::Vendor);
        let independent = by_path["tools/independent/src/lib.rs"];
        assert_eq!(
            independent
                .package
                .as_ref()
                .map(|package| package.name.as_str()),
            Some("independent")
        );
        assert_eq!(
            independent.classification,
            SourceClassification::CargoMetadata
        );
        assert_eq!(
            by_path["standalone/lib.rs"].classification,
            SourceClassification::PathFallback
        );
        assert!(!by_path.contains_key("ignored-generated/output.rs"));
        Ok(())
    }

    #[test]
    fn composer_psr4_roots_classify_production_and_dev_sources() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        write(
            repository.path(),
            "composer.json",
            r#"{
  "name": "chakra/fixture",
  "autoload": {"psr-4": {"App\\\\": "app/"}},
  "autoload-dev": {"psr-4": {"Tests\\\\": ["tests/", "spec/"]}}
}"#,
        )?;
        write(
            repository.path(),
            "app/Service.php",
            "<?php class Service {}\n",
        )?;
        write(
            repository.path(),
            "tests/ServiceTest.php",
            "<?php class ServiceTest {}\n",
        )?;
        write(repository.path(), "spec/Spec.php", "<?php class Spec {}\n")?;
        write(
            repository.path(),
            "legacy/Legacy.php",
            "<?php class Legacy {}\n",
        )?;

        let classified = discover_classified_sources(repository.path(), Language::Php)?;
        let by_path: BTreeMap<_, _> = classified
            .into_iter()
            .map(|source| (source.path, source.metadata))
            .collect();
        let app = &by_path[&RepoRelativePath::new("app/Service.php")?];
        assert_eq!(app.classification, SourceClassification::ComposerMetadata);
        assert_eq!(app.role, SourceRole::Production);
        assert_eq!(
            app.package.as_ref().map(|package| (
                package.name.as_str(),
                package.root.as_ref().map(RepoRelativePath::as_str)
            )),
            Some(("chakra/fixture", Some("app")))
        );
        for path in ["tests/ServiceTest.php", "spec/Spec.php"] {
            let source = &by_path[&RepoRelativePath::new(path)?];
            assert_eq!(
                source.classification,
                SourceClassification::ComposerMetadata
            );
            assert_eq!(source.role, SourceRole::Test);
        }
        assert_eq!(
            by_path[&RepoRelativePath::new("legacy/Legacy.php")?].classification,
            SourceClassification::PathFallback
        );
        Ok(())
    }

    #[test]
    fn package_json_scopes_and_typescript_test_conventions() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(
            root,
            "package.json",
            r#"{"name": "chakra/monorepo", "workspaces": ["packages/*"]}"#,
        )?;
        write(
            root,
            "packages/web/package.json",
            r#"{"name": "@chakra/web"}"#,
        )?;
        write(root, "packages/web/tsconfig.json", "{}\n")?;
        write(
            root,
            "packages/cli/tsconfig.json",
            "{\"compilerOptions\": {}}\n",
        )?;
        for (path, source) in [
            (
                "packages/web/src/app.ts",
                "export function app(): void {}\n",
            ),
            (
                "packages/web/src/view.tsx",
                "export function View() { return null; }\n",
            ),
            (
                "packages/web/src/app.test.ts",
                "export function appTest(): void {}\n",
            ),
            (
                "packages/web/src/__tests__/hook.ts",
                "export function hook(): void {}\n",
            ),
            (
                "packages/cli/src/main.ts",
                "export function cliMain(): void {}\n",
            ),
            ("scripts/tool.ts", "export function tool(): void {}\n"),
        ] {
            write(root, path, source)?;
        }

        let classified = discover_classified_sources(root, Language::TypeScript)?;
        let by_path: BTreeMap<_, _> = classified
            .into_iter()
            .map(|source| (source.path, source.metadata))
            .collect();
        let app = &by_path[&RepoRelativePath::new("packages/web/src/app.ts")?];
        assert_eq!(
            app.classification,
            SourceClassification::PackageJsonMetadata
        );
        assert_eq!(app.role, SourceRole::Production);
        assert_eq!(
            app.package.as_ref().map(|package| (
                package.name.as_str(),
                package.root.as_ref().map(RepoRelativePath::as_str)
            )),
            Some(("@chakra/web", Some("packages/web")))
        );
        let view = &by_path[&RepoRelativePath::new("packages/web/src/view.tsx")?];
        assert_eq!(view.role, SourceRole::Production);
        for path in [
            "packages/web/src/app.test.ts",
            "packages/web/src/__tests__/hook.ts",
        ] {
            let source = &by_path[&RepoRelativePath::new(path)?];
            assert_eq!(
                source.classification,
                SourceClassification::PackageJsonMetadata
            );
            assert_eq!(source.role, SourceRole::Test, "{path} must be a test role");
        }
        let cli = &by_path[&RepoRelativePath::new("packages/cli/src/main.ts")?];
        assert_eq!(
            cli.classification,
            SourceClassification::PackageJsonMetadata,
            "tsconfig.json without a sibling package.json is a project boundary"
        );
        assert_eq!(
            cli.package.as_ref().map(|package| package.name.as_str()),
            Some("packages/cli")
        );
        let tool = &by_path[&RepoRelativePath::new("scripts/tool.ts")?];
        assert_eq!(
            tool.classification,
            SourceClassification::PackageJsonMetadata
        );
        assert_eq!(
            tool.package.as_ref().map(|package| (
                package.name.as_str(),
                package.root.as_ref().map(RepoRelativePath::as_str)
            )),
            Some(("chakra/monorepo", None)),
            "the workspace root package.json scopes files without a nearer package"
        );
        Ok(())
    }

    #[test]
    fn package_json_scopes_and_javascript_test_conventions() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(
            root,
            "package.json",
            r#"{"name": "chakra/monorepo", "workspaces": ["packages/*"]}"#,
        )?;
        write(
            root,
            "packages/web/package.json",
            r#"{"name": "@chakra/web"}"#,
        )?;
        write(root, "packages/cli/jsconfig.json", "{}\n")?;
        for (path, source) in [
            ("packages/web/src/app.js", "export function app() {}\n"),
            (
                "packages/web/src/view.jsx",
                "export function View() { return null; }\n",
            ),
            (
                "packages/web/src/app.test.js",
                "export function appTest() {}\n",
            ),
            (
                "packages/web/src/__tests__/hook.js",
                "export function hook() {}\n",
            ),
            ("packages/cli/src/main.cjs", "module.exports = {};\n"),
            ("scripts/tool.mjs", "export function tool() {}\n"),
        ] {
            write(root, path, source)?;
        }

        let classified = discover_classified_sources(root, Language::JavaScript)?;
        let by_path: BTreeMap<_, _> = classified
            .into_iter()
            .map(|source| (source.path, source.metadata))
            .collect();
        let app = &by_path[&RepoRelativePath::new("packages/web/src/app.js")?];
        assert_eq!(
            app.classification,
            SourceClassification::PackageJsonMetadata
        );
        assert_eq!(app.role, SourceRole::Production);
        assert_eq!(
            app.package.as_ref().map(|package| (
                package.name.as_str(),
                package.root.as_ref().map(RepoRelativePath::as_str)
            )),
            Some(("@chakra/web", Some("packages/web")))
        );
        let view = &by_path[&RepoRelativePath::new("packages/web/src/view.jsx")?];
        assert_eq!(view.role, SourceRole::Production);
        for path in [
            "packages/web/src/app.test.js",
            "packages/web/src/__tests__/hook.js",
        ] {
            let source = &by_path[&RepoRelativePath::new(path)?];
            assert_eq!(
                source.classification,
                SourceClassification::PackageJsonMetadata
            );
            assert_eq!(source.role, SourceRole::Test, "{path} must be a test role");
        }
        let cli = &by_path[&RepoRelativePath::new("packages/cli/src/main.cjs")?];
        assert_eq!(
            cli.classification,
            SourceClassification::PackageJsonMetadata,
            "jsconfig.json without a sibling package.json is a project boundary"
        );
        assert_eq!(
            cli.package.as_ref().map(|package| package.name.as_str()),
            Some("packages/cli")
        );
        let tool = &by_path[&RepoRelativePath::new("scripts/tool.mjs")?];
        assert_eq!(
            tool.classification,
            SourceClassification::PackageJsonMetadata
        );
        assert_eq!(
            tool.package.as_ref().map(|package| (
                package.name.as_str(),
                package.root.as_ref().map(RepoRelativePath::as_str)
            )),
            Some(("chakra/monorepo", None)),
            "the workspace root package.json scopes files without a nearer package"
        );
        Ok(())
    }

    #[test]
    fn javascript_without_manifests_uses_test_conventions_and_fallback()
    -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(root, "src/util.js", "export function util() {}\n")?;
        write(root, "src/util.spec.js", "export function utilSpec() {}\n")?;

        let classified = discover_classified_sources(root, Language::JavaScript)?;
        let by_path: BTreeMap<_, _> = classified
            .into_iter()
            .map(|source| (source.path, source.metadata))
            .collect();
        let util = &by_path[&RepoRelativePath::new("src/util.js")?];
        assert_eq!(util.classification, SourceClassification::PathFallback);
        assert_eq!(util.role, SourceRole::Production);
        let spec = &by_path[&RepoRelativePath::new("src/util.spec.js")?];
        assert_eq!(spec.classification, SourceClassification::PathFallback);
        assert_eq!(spec.role, SourceRole::Test);
        Ok(())
    }

    #[test]
    fn pom_scopes_and_java_test_conventions() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(
            root,
            "pom.xml",
            "<project>\n  <artifactId>chakra-parent</artifactId>\n</project>\n",
        )?;
        write(
            root,
            "service/pom.xml",
            "<project>\n  <parent>\n    <artifactId>chakra-parent</artifactId>\n  </parent>\n  <artifactId>chakra-service</artifactId>\n</project>\n",
        )?;
        for (path, source) in [
            (
                "service/src/main/java/chakra/Service.java",
                "package chakra;\nclass Service {}\n",
            ),
            (
                "service/src/test/java/chakra/ServiceTest.java",
                "package chakra;\nclass ServiceTest {}\n",
            ),
            (
                "service/src/main/java/chakra/TestHelper.java",
                "package chakra;\nclass TestHelper {}\n",
            ),
        ] {
            write(root, path, source)?;
        }

        let classified = discover_classified_sources(root, Language::Java)?;
        let by_path: BTreeMap<_, _> = classified
            .into_iter()
            .map(|source| (source.path, source.metadata))
            .collect();
        let service =
            &by_path[&RepoRelativePath::new("service/src/main/java/chakra/Service.java")?];
        assert_eq!(service.classification, SourceClassification::MavenMetadata);
        assert_eq!(service.role, SourceRole::Production);
        assert_eq!(
            service.package.as_ref().map(|package| (
                package.name.as_str(),
                package.root.as_ref().map(RepoRelativePath::as_str)
            )),
            Some(("chakra-service", Some("service"))),
            "the module pom scopes its sources; the parent artifactId stays in <parent>"
        );
        let test =
            &by_path[&RepoRelativePath::new("service/src/test/java/chakra/ServiceTest.java")?];
        assert_eq!(test.classification, SourceClassification::MavenMetadata);
        assert_eq!(
            test.role,
            SourceRole::Test,
            "src/test/java is the Maven/Gradle test source root"
        );
        let helper =
            &by_path[&RepoRelativePath::new("service/src/main/java/chakra/TestHelper.java")?];
        assert_eq!(
            helper.role,
            SourceRole::Test,
            "Test*.java is a test convention even under src/main/java"
        );
        Ok(())
    }

    #[test]
    fn gradle_settings_and_build_boundaries_scope_java_sources() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(root, "settings.gradle", "rootProject.name = 'chakra-app'\n")?;
        write(root, "app/build.gradle.kts", "plugins { java }\n")?;
        write(
            root,
            "src/main/java/chakra/App.java",
            "package chakra;\nclass App {}\n",
        )?;
        write(
            root,
            "app/src/test/java/chakra/AppTests.java",
            "package chakra;\nclass AppTests {}\n",
        )?;

        let classified = discover_classified_sources(root, Language::Java)?;
        let by_path: BTreeMap<_, _> = classified
            .into_iter()
            .map(|source| (source.path, source.metadata))
            .collect();
        let app = &by_path[&RepoRelativePath::new("src/main/java/chakra/App.java")?];
        assert_eq!(app.classification, SourceClassification::GradleMetadata);
        assert_eq!(
            app.package.as_ref().map(|package| (
                package.name.as_str(),
                package.root.as_ref().map(RepoRelativePath::as_str)
            )),
            Some(("chakra-app", None))
        );
        let tests = &by_path[&RepoRelativePath::new("app/src/test/java/chakra/AppTests.java")?];
        assert_eq!(tests.classification, SourceClassification::GradleMetadata);
        assert_eq!(tests.role, SourceRole::Test);
        assert_eq!(
            tests.package.as_ref().map(|package| (
                package.name.as_str(),
                package.root.as_ref().map(RepoRelativePath::as_str)
            )),
            Some(("app", Some("app"))),
            "a build.gradle.kts without a sibling settings file is a project boundary"
        );
        Ok(())
    }

    #[test]
    fn java_without_manifests_uses_test_conventions_and_fallback() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(root, "src/Util.java", "class Util {}\n")?;
        write(root, "src/UtilTest.java", "class UtilTest {}\n")?;

        let classified = discover_classified_sources(root, Language::Java)?;
        let by_path: BTreeMap<_, _> = classified
            .into_iter()
            .map(|source| (source.path, source.metadata))
            .collect();
        let util = &by_path[&RepoRelativePath::new("src/Util.java")?];
        assert_eq!(util.classification, SourceClassification::PathFallback);
        assert_eq!(util.role, SourceRole::Production);
        let test = &by_path[&RepoRelativePath::new("src/UtilTest.java")?];
        assert_eq!(test.classification, SourceClassification::PathFallback);
        assert_eq!(test.role, SourceRole::Test);
        Ok(())
    }

    #[test]
    fn typescript_without_manifests_uses_test_conventions_and_fallback()
    -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(root, "src/util.ts", "export function util(): void {}\n")?;
        write(
            root,
            "src/util.spec.ts",
            "export function utilSpec(): void {}\n",
        )?;

        let classified = discover_classified_sources(root, Language::TypeScript)?;
        let by_path: BTreeMap<_, _> = classified
            .into_iter()
            .map(|source| (source.path, source.metadata))
            .collect();
        let util = &by_path[&RepoRelativePath::new("src/util.ts")?];
        assert_eq!(util.classification, SourceClassification::PathFallback);
        assert_eq!(util.role, SourceRole::Production);
        let spec = &by_path[&RepoRelativePath::new("src/util.spec.ts")?];
        assert_eq!(spec.classification, SourceClassification::PathFallback);
        assert_eq!(spec.role, SourceRole::Test);
        Ok(())
    }

    #[test]
    fn pyproject_scopes_and_python_test_conventions() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(
            root,
            "pyproject.toml",
            "[project]\nname = \"chakra-python-monorepo\"\n",
        )?;
        write(
            root,
            "packages/web/pyproject.toml",
            "[project]\nname = \"chakra-web\"\n",
        )?;
        write(
            root,
            "packages/cli/setup.cfg",
            "[metadata]\nname = chakra-cli\n",
        )?;
        write(
            root,
            "packages/legacy/setup.py",
            "from setuptools import setup\n",
        )?;
        for (path, source) in [
            ("packages/web/src/app.py", "def app():\n    pass\n"),
            (
                "packages/web/tests/test_app.py",
                "def test_app():\n    pass\n",
            ),
            (
                "packages/web/src/app_test.py",
                "def app_test():\n    pass\n",
            ),
            ("packages/cli/src/main.py", "def cli_main():\n    pass\n"),
            ("packages/legacy/src/old.py", "def old():\n    pass\n"),
            ("scripts/tool.py", "def tool():\n    pass\n"),
        ] {
            write(root, path, source)?;
        }

        let classified = discover_classified_sources(root, Language::Python)?;
        let by_path: BTreeMap<_, _> = classified
            .into_iter()
            .map(|source| (source.path, source.metadata))
            .collect();
        let app = &by_path[&RepoRelativePath::new("packages/web/src/app.py")?];
        assert_eq!(app.classification, SourceClassification::PyprojectMetadata);
        assert_eq!(app.role, SourceRole::Production);
        assert_eq!(
            app.package.as_ref().map(|package| (
                package.name.as_str(),
                package.root.as_ref().map(RepoRelativePath::as_str)
            )),
            Some(("chakra-web", Some("packages/web")))
        );
        for path in [
            "packages/web/tests/test_app.py",
            "packages/web/src/app_test.py",
        ] {
            let source = &by_path[&RepoRelativePath::new(path)?];
            assert_eq!(
                source.classification,
                SourceClassification::PyprojectMetadata
            );
            assert_eq!(source.role, SourceRole::Test, "{path} must be a test role");
        }
        let cli = &by_path[&RepoRelativePath::new("packages/cli/src/main.py")?];
        assert_eq!(
            cli.classification,
            SourceClassification::PyprojectMetadata,
            "setup.cfg without a sibling pyproject.toml is a project boundary"
        );
        assert_eq!(
            cli.package.as_ref().map(|package| package.name.as_str()),
            Some("packages/cli")
        );
        let legacy = &by_path[&RepoRelativePath::new("packages/legacy/src/old.py")?];
        assert_eq!(
            legacy.classification,
            SourceClassification::PyprojectMetadata,
            "setup.py without a sibling pyproject.toml is a project boundary"
        );
        assert_eq!(
            legacy.package.as_ref().map(|package| package.name.as_str()),
            Some("packages/legacy")
        );
        let tool = &by_path[&RepoRelativePath::new("scripts/tool.py")?];
        assert_eq!(tool.classification, SourceClassification::PyprojectMetadata);
        assert_eq!(
            tool.package.as_ref().map(|package| (
                package.name.as_str(),
                package.root.as_ref().map(RepoRelativePath::as_str)
            )),
            Some(("chakra-python-monorepo", None)),
            "the workspace root pyproject.toml scopes files without a nearer package"
        );
        Ok(())
    }

    #[test]
    fn python_without_manifests_uses_test_conventions_and_fallback() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(root, "src/util.py", "def util():\n    pass\n")?;
        write(root, "tests/test_util.py", "def test_util():\n    pass\n")?;

        let classified = discover_classified_sources(root, Language::Python)?;
        let by_path: BTreeMap<_, _> = classified
            .into_iter()
            .map(|source| (source.path, source.metadata))
            .collect();
        let util = &by_path[&RepoRelativePath::new("src/util.py")?];
        assert_eq!(util.classification, SourceClassification::PathFallback);
        assert_eq!(util.role, SourceRole::Production);
        let test = &by_path[&RepoRelativePath::new("tests/test_util.py")?];
        assert_eq!(test.classification, SourceClassification::PathFallback);
        assert_eq!(test.role, SourceRole::Test);
        Ok(())
    }

    #[test]
    fn staged_rename_reclassifies_the_current_path() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(root, "tests/original.rs", "fn moved() {}\n")?;
        command(root, "git", &["add", "tests/original.rs"])?;
        command(root, "git", &["commit", "--quiet", "-m", "base"])?;
        fs::create_dir_all(root.join("examples"))?;
        command(
            root,
            "git",
            &["mv", "tests/original.rs", "examples/renamed.rs"],
        )?;

        let sources = discover_classified_sources(root, Language::Rust)?;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path.as_str(), "examples/renamed.rs");
        assert_eq!(sources[0].metadata.role, SourceRole::Example);
        assert_eq!(
            sources[0].metadata.classification,
            SourceClassification::PathFallback
        );
        Ok(())
    }

    #[test]
    fn metadata_without_lockfile_does_not_mutate_the_worktree() -> Result<(), Box<dyn Error>> {
        let repository = repository()?;
        let root = repository.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"unlocked\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        write(root, "src/lib.rs", "pub fn unlocked() {}\n")?;

        let sources = discover_classified_sources(root, Language::Rust)?;
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].metadata.classification,
            SourceClassification::CargoMetadata
        );
        assert_eq!(
            sources[0]
                .metadata
                .package
                .as_ref()
                .map(|package| package.name.as_str()),
            Some("unlocked")
        );
        assert!(!root.join("Cargo.lock").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_an_in_flight_metadata_command() -> Result<(), Box<dyn Error>> {
        use std::time::Duration;

        let repository = TempDir::new()?;
        let marker = repository.path().join("metadata-command-started");
        let operation = OperationContext::unbounded();
        let worker_operation = operation.clone();
        let worker_root = repository.path().to_path_buf();
        let worker = thread::spawn(move || {
            capture_command(
                &worker_root,
                "sh",
                &[
                    OsString::from("-c"),
                    OsString::from("printf ready > metadata-command-started; exec sleep 30"),
                ],
                COMMAND_TIMEOUT,
                &worker_operation,
            )
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.is_file() && Instant::now() < deadline {
            thread::park_timeout(Duration::from_millis(1));
        }
        if !marker.is_file() {
            operation.cancel();
            let _ = worker.join();
            return Err("metadata command did not start within the test bound".into());
        }

        operation.cancel();
        let result = worker
            .join()
            .map_err(|_| "metadata command worker panicked")?;
        assert!(matches!(
            result,
            Err(MetadataCommandError::Operation(OperationAbort::Cancelled))
        ));
        Ok(())
    }
}
