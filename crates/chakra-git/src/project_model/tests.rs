//! Hermetic temp-Git-repository coverage for the typed project model.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use chakra_domain::operation::OperationContext;
use chakra_domain::project::{
    ProjectDependencyKind, ProjectManifestIssueKind, ProjectOwnership, ProjectUnitId,
    ProjectUnitKind, ProjectWorkspaceKind,
};
use chakra_domain::source::SourceRole;
use chakra_domain::symbol::Language;
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

fn model(root: &Path) -> Result<ProjectModel, Box<dyn Error>> {
    let operation = OperationContext::unbounded();
    let inventory = crate::discover_workspace_inventory_in_worktree_with_context(root, &operation)?;
    Ok(discover_project_model_with_context(
        root,
        &inventory.sources,
        &inventory.metadata_inputs,
        &operation,
    )?)
}

fn path(raw: &str) -> Result<RepoRelativePath, Box<dyn Error>> {
    Ok(RepoRelativePath::new(raw)?)
}

#[test]
fn nested_cargo_workspaces_become_typed_units_with_dependencies() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"3\"\n",
    )?;
    write(
        root,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    write(
        root,
        "crates/cli/Cargo.toml",
        "[package]\nname = \"cli\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ncore = { path = \"../core\" }\n\n[dev-dependencies]\ncore-dev = { package = \"core\", path = \"../core\" }\n",
    )?;
    write(root, "crates/core/src/lib.rs", "pub fn core() {}\n")?;
    write(root, "crates/core/tests/api.rs", "fn integration() {}\n")?;
    write(
        root,
        "crates/cli/src/main.rs",
        "fn main() { core::core(); }\n",
    )?;
    // A nested, independent workspace inside the same repository.
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
    // Non-Cargo sources fall back to path units.
    write(root, "scripts/deploy.py", "def deploy():\n    pass\n")?;
    write(root, "vendor/tracked.rs", "fn vendored() {}\n")?;
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

    let model = model(root)?;
    assert!(model.issues.is_empty(), "issues: {:?}", model.issues);

    let core_id = ProjectUnitId::new(
        ProjectUnitKind::CargoPackage,
        Some(&path("crates/core")?),
        "core",
    );
    let cli_id = ProjectUnitId::new(
        ProjectUnitKind::CargoPackage,
        Some(&path("crates/cli")?),
        "cli",
    );
    let core = model.unit(&core_id).ok_or("core unit missing")?;
    assert_eq!(core.kind, ProjectUnitKind::CargoPackage);
    assert_eq!(core.language, Some(Language::Rust));
    assert_eq!(
        core.manifest.as_ref().map(RepoRelativePath::as_str),
        Some("crates/core/Cargo.toml")
    );
    assert!(
        core.source_roots
            .iter()
            .any(|root| root.role == SourceRole::Test),
        "cargo test target role must survive: {:?}",
        core.source_roots
    );

    let cli = model.unit(&cli_id).ok_or("cli unit missing")?;
    let normal = cli
        .dependencies
        .iter()
        .find(|dependency| dependency.kind == ProjectDependencyKind::Normal)
        .ok_or("cli normal dependency missing")?;
    assert_eq!(normal.name, "core");
    assert_eq!(
        normal.target.as_ref(),
        Some(&core_id),
        "path dependency must resolve to the workspace unit"
    );
    assert!(
        cli.dependencies
            .iter()
            .any(|dependency| dependency.kind == ProjectDependencyKind::Development),
        "dev-dependency kind must survive"
    );

    // Both the outer and the nested workspace are represented.
    let workspace_roots: Vec<_> = model
        .workspaces
        .iter()
        .map(|workspace| {
            assert_eq!(workspace.kind, ProjectWorkspaceKind::Cargo);
            workspace.root.as_ref().map(RepoRelativePath::as_str)
        })
        .collect();
    assert!(workspace_roots.contains(&None));
    assert!(workspace_roots.contains(&Some("tools/independent")));
    let outer = model
        .workspaces
        .iter()
        .find(|workspace| workspace.root.is_none())
        .ok_or("outer workspace missing")?;
    assert!(outer.members.contains(&core_id));
    assert!(outer.members.contains(&cli_id));

    // Ownership prefers the deepest member; other languages fall back.
    assert_eq!(
        model.ownership(&path("crates/core/src/lib.rs")?, Language::Rust),
        ProjectOwnership::Owned(core_id)
    );
    assert_eq!(
        model.ownership(&path("tools/independent/src/lib.rs")?, Language::Rust),
        ProjectOwnership::Owned(ProjectUnitId::new(
            ProjectUnitKind::CargoPackage,
            Some(&path("tools/independent")?),
            "independent",
        ))
    );
    let scripts = model.ownership(&path("scripts/deploy.py")?, Language::Python);
    match scripts {
        ProjectOwnership::Owned(unit) => {
            let unit = model.unit(&unit).ok_or("fallback unit missing")?;
            assert_eq!(unit.kind, ProjectUnitKind::PathFallback);
            assert_eq!(unit.name, "scripts");
        }
        other => {
            return Err(format!("expected path-fallback ownership, got {other:?}").into());
        }
    }
    let vendored = model.ownership(&path("vendor/tracked.rs")?, Language::Rust);
    assert!(
        matches!(vendored, ProjectOwnership::Owned(ref unit) if unit.as_str().starts_with("path:vendor:")),
        "vendor files stay outside cargo units: {vendored:?}"
    );
    Ok(())
}

#[test]
fn composer_packages_become_typed_units_with_psr4_roles_and_dependencies()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    write(
        root,
        "composer.json",
        r#"{
            "name": "acme/blog",
            "require": { "php": "^8.2", "ext-json": "*", "monolog/monolog": "^3.0" },
            "require-dev": { "phpunit/phpunit": "^10.0" },
            "autoload": { "psr-4": { "Acme\\Blog\\": "src/" } },
            "autoload-dev": { "psr-4": { "Acme\\Blog\\Tests\\": "tests/" } }
        }"#,
    )?;
    write(root, "src/Post.php", "<?php class Post {}\n")?;
    write(root, "tests/PostTest.php", "<?php class PostTest {}\n")?;
    write(root, "bin/console.php", "<?php\n")?;
    write(
        root,
        "packages/widget/composer.json",
        r#"{
            "name": "acme/widget",
            "autoload": { "psr-4": { "Acme\\Widget\\": "lib/" } }
        }"#,
    )?;
    write(
        root,
        "packages/widget/lib/Widget.php",
        "<?php class Widget {}\n",
    )?;
    command(root, "git", &["add", "."])?;

    let model = model(root)?;
    assert!(model.issues.is_empty(), "issues: {:?}", model.issues);
    let blog_id = ProjectUnitId::new(ProjectUnitKind::ComposerPackage, None, "acme/blog");
    let blog = model.unit(&blog_id).ok_or("blog unit missing")?;
    assert_eq!(blog.language, Some(Language::Php));
    let roles: BTreeMap<_, _> = blog
        .source_roots
        .iter()
        .map(|root| (root.root.as_ref().map(RepoRelativePath::as_str), root.role))
        .collect();
    assert_eq!(roles.get(&Some("src")), Some(&SourceRole::Production));
    assert_eq!(roles.get(&Some("tests")), Some(&SourceRole::Test));
    let dependency = |name: &str| {
        blog.dependencies
            .iter()
            .find(|dependency| dependency.name == name)
            .map(|dependency| dependency.kind)
    };
    assert_eq!(
        dependency("monolog/monolog"),
        Some(ProjectDependencyKind::Normal)
    );
    assert_eq!(
        dependency("phpunit/phpunit"),
        Some(ProjectDependencyKind::Development)
    );
    assert_eq!(dependency("php"), None, "platform packages are not edges");
    assert_eq!(dependency("ext-json"), None);

    assert_eq!(
        model.ownership(&path("src/Post.php")?, Language::Php),
        ProjectOwnership::Owned(blog_id.clone())
    );
    // Outside every declared PSR-4 root the package claims nothing and the
    // file degrades to a path-fallback unit.
    let console = model.ownership(&path("bin/console.php")?, Language::Php);
    assert!(
        matches!(console, ProjectOwnership::Owned(ref unit) if unit.as_str().starts_with("path:bin:")),
        "bin/ must fall back honestly: {console:?}"
    );
    assert_eq!(
        model.ownership(&path("packages/widget/lib/Widget.php")?, Language::Php),
        ProjectOwnership::Owned(ProjectUnitId::new(
            ProjectUnitKind::ComposerPackage,
            Some(&path("packages/widget")?),
            "acme/widget",
        ))
    );
    Ok(())
}

#[test]
fn malformed_composer_manifest_degrades_to_an_issue_and_path_fallback() -> Result<(), Box<dyn Error>>
{
    let repository = repository()?;
    let root = repository.path();
    write(root, "composer.json", "{ not json\n")?;
    write(root, "src/Post.php", "<?php class Post {}\n")?;
    command(root, "git", &["add", "."])?;

    let model = model(root)?;
    assert_eq!(
        model
            .units
            .iter()
            .filter(|unit| unit.kind == ProjectUnitKind::ComposerPackage)
            .count(),
        0
    );
    let issue = model
        .issues
        .iter()
        .find(|issue| issue.manifest.as_str() == "composer.json")
        .ok_or("malformed manifest issue missing")?;
    assert_eq!(issue.kind, ProjectManifestIssueKind::MalformedContent);
    let ownership = model.ownership(&path("src/Post.php")?, Language::Php);
    assert!(
        matches!(ownership, ProjectOwnership::Owned(ref unit) if unit.as_str().starts_with("path:src:")),
        "files of a malformed package degrade to path fallback: {ownership:?}"
    );
    Ok(())
}

#[test]
fn malformed_cargo_manifest_degrades_to_a_probe_issue_and_path_fallback()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    write(root, "Cargo.toml", "[package\nname = ")?;
    write(root, "src/lib.rs", "pub fn broken() {}\n")?;
    command(root, "git", &["add", "."])?;

    let model = model(root)?;
    assert!(
        model
            .units
            .iter()
            .all(|unit| unit.kind != ProjectUnitKind::CargoPackage),
        "a malformed manifest must not produce a cargo unit: {:?}",
        model.units
    );
    let issue = model
        .issues
        .iter()
        .find(|issue| issue.manifest.as_str() == "Cargo.toml")
        .ok_or("malformed cargo manifest issue missing")?;
    assert_eq!(issue.kind, ProjectManifestIssueKind::ProbeFailed);
    let ownership = model.ownership(&path("src/lib.rs")?, Language::Rust);
    assert!(
        matches!(ownership, ProjectOwnership::Owned(ref unit) if unit.as_str().starts_with("path:src:")),
        "sources of an unprobeable package degrade to path fallback: {ownership:?}"
    );
    Ok(())
}

#[test]
fn ambiguous_composer_ownership_reports_every_candidate() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    // The root package declares a PSR-4 root inside the nested package's
    // tree, so both packages claim the same directory at the same depth.
    write(
        root,
        "composer.json",
        r#"{ "name": "acme/root", "autoload": { "psr-4": { "Acme\\Root\\": "packages/shared/src/" } } }"#,
    )?;
    write(
        root,
        "packages/shared/composer.json",
        r#"{ "name": "acme/shared", "autoload": { "psr-4": { "Acme\\Shared\\": "src/" } } }"#,
    )?;
    write(
        root,
        "packages/shared/src/Shared.php",
        "<?php class Shared {}\n",
    )?;
    command(root, "git", &["add", "."])?;

    let model = model(root)?;
    match model.ownership(&path("packages/shared/src/Shared.php")?, Language::Php) {
        ProjectOwnership::Ambiguous { candidates } => {
            assert_eq!(candidates.len(), 2, "candidates: {candidates:?}");
            assert!(
                candidates
                    .iter()
                    .all(|candidate| candidate.as_str().starts_with("composer:"))
            );
        }
        other => {
            return Err(format!("expected ambiguous ownership, got {other:?}").into());
        }
    }
    Ok(())
}

#[test]
fn metadata_only_manifest_edit_changes_the_model() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    write(
        root,
        "composer.json",
        r#"{
            "name": "acme/blog",
            "autoload": { "psr-4": { "Acme\\Blog\\": "src/" } }
        }"#,
    )?;
    write(root, "src/Post.php", "<?php class Post {}\n")?;
    command(root, "git", &["add", "."])?;

    let before = model(root)?;
    write(
        root,
        "composer.json",
        r#"{
            "name": "acme/blog",
            "require": { "monolog/monolog": "^3.0" },
            "autoload": { "psr-4": { "Acme\\Blog\\": "src/" } }
        }"#,
    )?;
    command(root, "git", &["add", "."])?;
    let after = model(root)?;
    assert_ne!(
        before, after,
        "a metadata-only manifest edit must rebuild the project model"
    );
    let blog = after
        .unit(&ProjectUnitId::new(
            ProjectUnitKind::ComposerPackage,
            None,
            "acme/blog",
        ))
        .ok_or("blog unit missing after edit")?;
    assert!(
        blog.dependencies
            .iter()
            .any(|dependency| dependency.name == "monolog/monolog"),
        "new dependency must appear without any source change"
    );
    Ok(())
}
