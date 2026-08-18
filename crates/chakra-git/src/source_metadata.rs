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
use chakra_domain::source::{SourceClassification, SourceMetadata, SourcePackage, SourceRole};
use chakra_domain::symbol::Language;

use crate::{DiscoveryError, discover_language_files, resolve_repository_root};

const COMMAND_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const COMMAND_STDERR_LIMIT: usize = 16 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(1);
const MAX_CARGO_METADATA_INVOCATIONS: usize = 64;
const MAX_COMPOSER_MANIFESTS: usize = 64;
const MAX_COMPOSER_MANIFEST_BYTES: usize = 1024 * 1024;

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
) -> io::Result<CommandOutput> {
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
        return Err(io::Error::other("command stdout pipe is unavailable"));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate(&mut child);
        return Err(io::Error::other("command stderr pipe is unavailable"));
    };
    let stdout_reader = match thread::Builder::new()
        .name("chakra-source-metadata-stdout".to_owned())
        .spawn(move || read_bounded(stdout, COMMAND_OUTPUT_LIMIT))
    {
        Ok(reader) => reader,
        Err(error) => {
            terminate(&mut child);
            return Err(error);
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
            return Err(error);
        }
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
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
                ));
            }
            Err(error) => {
                terminate(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(error);
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

fn manifest_inventory(root: &Path, manifest_name: &str) -> Vec<RepoRelativePath> {
    let args = [
        OsString::from("ls-files"),
        OsString::from("--cached"),
        OsString::from("--others"),
        OsString::from("--exclude-standard"),
        OsString::from("-z"),
    ];
    let Ok(output) = capture_command(root, "git", &args, COMMAND_TIMEOUT) else {
        return Vec::new();
    };
    if !output.success || output.exceeded {
        return Vec::new();
    }
    let suffix = format!("/{manifest_name}");
    let mut manifests: Vec<_> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| *raw == manifest_name.as_bytes() || raw.ends_with(suffix.as_bytes()))
        .filter_map(|raw| std::str::from_utf8(raw).ok())
        .filter_map(|path| RepoRelativePath::new(path).ok())
        .filter(|path| root.join(path.as_str()).is_file())
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

fn cargo_packages(root: &Path) -> Vec<CargoPackage> {
    let manifests = manifest_inventory(root, "Cargo.toml");
    let mut covered = BTreeSet::new();
    let mut packages: BTreeMap<(Option<RepoRelativePath>, String), CargoPackage> = BTreeMap::new();
    let mut invocations = 0_usize;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    for manifest in manifests {
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
        let Ok(output) = capture_command(root, "cargo", &args, remaining) else {
            continue;
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
    packages.into_values().collect()
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

fn composer_packages(root: &Path) -> Vec<ComposerRoot> {
    let mut packages = BTreeMap::new();
    for manifest in manifest_inventory(root, "composer.json")
        .into_iter()
        .take(MAX_COMPOSER_MANIFESTS)
    {
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
    packages.into_values().collect()
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
    let files = discover_language_files(&root, language)?;
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let cargo = (language == Language::Rust).then(|| cargo_packages(&root));
    let composer = (language == Language::Php).then(|| composer_packages(&root));
    Ok(files
        .into_iter()
        .map(|path| ClassifiedSource {
            metadata: match language {
                Language::Rust => classify_rust(&path, cargo.as_deref().unwrap_or_default()),
                Language::Php => classify_php(&path, composer.as_deref().unwrap_or_default()),
            },
            path,
            language,
        })
        .collect())
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
}
