//! Language-neutral source classification and package scope.

use serde::{Deserialize, Serialize};

use crate::location::RepoRelativePath;

/// Operational role of a source file. Roles describe where code participates
/// in a project; they do not remove files or change symbol identity.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceRole {
    Production,
    Test,
    Example,
    Bench,
    Fixture,
    Generated,
    Vendor,
}

/// Evidence used to attach a role/package to a source path.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceClassification {
    /// Package ownership came from bounded `cargo metadata`; the role also
    /// uses deterministic path/target conventions.
    CargoMetadata,
    /// Package ownership and source root came from a Git-visible Composer
    /// `autoload.psr-4` or `autoload-dev.psr-4` declaration.
    ComposerMetadata,
    /// Package ownership came from a Git-visible `package.json` (npm-style
    /// package or workspace root); test conventions still come from
    /// deterministic TypeScript path rules.
    PackageJsonMetadata,
    /// Package ownership came from a Git-visible `pyproject.toml` (or a
    /// `setup.py`/`setup.cfg` project boundary without one); test
    /// conventions still come from deterministic Python path rules.
    PyprojectMetadata,
    /// No applicable package metadata was available; deterministic path
    /// conventions supplied the role.
    PathFallback,
}

/// Stable repository-relative identity of a Cargo or Composer package source
/// root containing a file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SourcePackage {
    pub name: String,
    /// Repository-relative package directory. `None` means repository root.
    pub root: Option<RepoRelativePath>,
}

/// Metadata attached to every indexed file and its query views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SourceMetadata {
    pub role: SourceRole,
    pub classification: SourceClassification,
    pub package: Option<SourcePackage>,
}

impl SourceMetadata {
    /// Language-neutral deterministic fallback for non-Cargo or partially
    /// described repositories.
    pub fn path_fallback(path: &RepoRelativePath) -> Self {
        Self {
            role: classify_path_role(path),
            classification: SourceClassification::PathFallback,
            package: None,
        }
    }
}

/// Coverage of source metadata in one immutable graph revision.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct SourceMetadataCoverage {
    pub total_files: u64,
    pub cargo_metadata_files: u64,
    pub composer_metadata_files: u64,
    pub package_json_metadata_files: u64,
    pub pyproject_metadata_files: u64,
    pub path_fallback_files: u64,
}

fn classify_path_role(path: &RepoRelativePath) -> SourceRole {
    let contains = |names: &[&str]| {
        path.as_str().split('/').any(|component| {
            names
                .iter()
                .any(|name| component.eq_ignore_ascii_case(name))
        })
    };

    if contains(&["vendor", "third_party", "third-party"]) {
        SourceRole::Vendor
    } else if contains(&["generated", "autogen", "codegen"]) {
        SourceRole::Generated
    } else if contains(&[
        "fixture",
        "fixtures",
        "snapshot",
        "snapshots",
        "test_data",
        "testdata",
        "golden",
    ]) {
        SourceRole::Fixture
    } else if contains(&["benches", "benchmarks"]) {
        SourceRole::Bench
    } else if contains(&["examples", "example"]) {
        SourceRole::Example
    } else if contains(&["tests", "test"]) {
        SourceRole::Test
    } else {
        SourceRole::Production
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_roles_are_deterministic_and_specific() -> Result<(), Box<dyn std::error::Error>> {
        for (path, expected) in [
            ("src/lib.rs", SourceRole::Production),
            ("tests/api.rs", SourceRole::Test),
            ("examples/demo.rs", SourceRole::Example),
            ("benches/read.rs", SourceRole::Bench),
            ("tests/fixtures/input.rs", SourceRole::Fixture),
            ("src/generated/schema.rs", SourceRole::Generated),
            ("vendor/dependency/lib.rs", SourceRole::Vendor),
        ] {
            assert_eq!(
                SourceMetadata::path_fallback(&RepoRelativePath::new(path)?).role,
                expected
            );
        }
        Ok(())
    }
}
