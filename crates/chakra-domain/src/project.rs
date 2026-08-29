//! Typed project scope model (issue #41).
//!
//! Package/module information from ecosystem manifests (Cargo workspaces and
//! Composer packages) is promoted from per-file annotations into a typed,
//! queryable project model. The model carries Chakra-owned types only: it is
//! built from manifest evidence by the Git/source-classification layer and
//! never leaks Cargo or Composer protocol structures. Files outside any
//! ecosystem unit are grouped into deterministic path-fallback units.
//!
//! Ownership is honest about degradation: a path claimed by several units at
//! the same depth reports [`ProjectOwnership::Ambiguous`] with its candidates
//! instead of silently picking one, and manifests that could not be probed or
//! parsed are recorded as [`ProjectManifestIssue`] entries while their files
//! degrade to path-fallback units.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::location::RepoRelativePath;
use crate::source::SourceRole;
use crate::symbol::Language;

/// Hard bound on project units retained in one model revision.
pub const MAX_PROJECT_UNITS: usize = 256;
/// Hard bound on source roots retained per unit.
pub const MAX_UNIT_SOURCE_ROOTS: usize = 64;
/// Hard bound on declared dependencies retained per unit.
pub const MAX_UNIT_DEPENDENCIES: usize = 128;
/// Hard bound on workspace groupings retained in one model revision.
pub const MAX_PROJECT_WORKSPACES: usize = 64;
/// Hard bound on manifest issues retained in one model revision.
pub const MAX_PROJECT_ISSUES: usize = 64;
/// Hard bound on candidates reported for one ambiguous ownership.
pub const MAX_AMBIGUITY_CANDIDATES: usize = 16;

/// Ecosystem that contributed a project unit.
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
pub enum ProjectUnitKind {
    /// A Cargo package discovered through bounded `cargo metadata`.
    CargoPackage,
    /// A Composer package discovered through a Git-visible `composer.json`.
    ComposerPackage,
    /// Deterministic top-level-directory grouping for sources no Cargo or
    /// Composer unit claims.
    PathFallback,
}

impl ProjectUnitKind {
    fn tag(self) -> &'static str {
        match self {
            Self::CargoPackage => "cargo",
            Self::ComposerPackage => "composer",
            Self::PathFallback => "path",
        }
    }
}

/// Stable, deterministic identity of one project unit inside a revision.
///
/// Ids are derived from the unit kind, its repository-relative root, and its
/// name, so the same manifest evidence always produces the same id and query
/// filters can address a unit unambiguously even when names collide.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct ProjectUnitId(String);

impl ProjectUnitId {
    pub fn new(kind: ProjectUnitKind, root: Option<&RepoRelativePath>, name: &str) -> Self {
        Self(format!(
            "{}:{}:{}",
            kind.tag(),
            root.map_or(".", RepoRelativePath::as_str),
            name
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProjectUnitId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One source root of a unit with the role files below it participate in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectSourceRoot {
    /// Repository-relative root directory. `None` denotes the repository
    /// root.
    pub root: Option<RepoRelativePath>,
    pub role: SourceRole,
}

/// Declared relationship between a unit and an external or workspace package.
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
pub enum ProjectDependencyKind {
    Normal,
    Development,
    Build,
}

/// One declared dependency edge of a project unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectDependency {
    pub name: String,
    pub kind: ProjectDependencyKind,
    /// Resolved target when the dependency points at another unit of the same
    /// repository (for example a Cargo path dependency). External registries
    /// and unresolvable references leave this `None`.
    pub target: Option<ProjectUnitId>,
}

/// One typed project unit: a Cargo package, a Composer package, or a
/// deterministic path-fallback grouping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectUnit {
    pub id: ProjectUnitId,
    pub kind: ProjectUnitKind,
    pub name: String,
    /// Repository-relative package directory. `None` denotes the repository
    /// root. Path-fallback units use their grouping directory.
    pub root: Option<RepoRelativePath>,
    /// Manifest that contributed this unit; absent for path-fallback units.
    pub manifest: Option<RepoRelativePath>,
    /// Language whose sources this unit claims for ownership. Path-fallback
    /// units claim sources of any language left over after ecosystem units.
    pub language: Option<Language>,
    pub source_roots: Vec<ProjectSourceRoot>,
    pub source_roots_omitted: u64,
    pub dependencies: Vec<ProjectDependency>,
    pub dependencies_omitted: u64,
}

impl ProjectUnit {
    /// Deepest root depth at which this unit claims `path`, if it claims it
    /// at all. Cargo packages claim their package directory; Composer
    /// packages claim their declared PSR-4 source roots; path-fallback units
    /// claim their grouping directory.
    fn claim_depth(&self, path: &RepoRelativePath) -> Option<usize> {
        match self.kind {
            ProjectUnitKind::CargoPackage | ProjectUnitKind::PathFallback => {
                path_within(self.root.as_ref(), path).then(|| root_depth(self.root.as_ref()))
            }
            ProjectUnitKind::ComposerPackage => self
                .source_roots
                .iter()
                .filter(|root| path_within(root.root.as_ref(), path))
                .map(|root| root_depth(root.root.as_ref()))
                .max(),
        }
    }
}

/// Ecosystem workspace grouping several units (issue #41: nested workspaces).
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
pub enum ProjectWorkspaceKind {
    Cargo,
}

/// A Cargo workspace and its member units. Composer has no workspace
/// concept; its packages appear as plain units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectWorkspace {
    pub kind: ProjectWorkspaceKind,
    /// Repository-relative workspace directory. `None` denotes the repository
    /// root.
    pub root: Option<RepoRelativePath>,
    pub members: Vec<ProjectUnitId>,
}

/// Why one manifest contributed no typed unit.
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
pub enum ProjectManifestIssueKind {
    /// The manifest could not be probed: the ecosystem tool failed or was
    /// unavailable, or the manifest was unreadable or oversized.
    ProbeFailed,
    /// The manifest was read but its content could not be parsed.
    MalformedContent,
}

/// One manifest whose evidence degraded to path fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectManifestIssue {
    pub manifest: RepoRelativePath,
    pub kind: ProjectManifestIssueKind,
}

/// Ownership of one source path within the project model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectOwnership {
    /// No unit claims the path (for example because unit bounds were hit).
    Unassigned,
    /// Exactly one deepest unit claims the path.
    Owned(ProjectUnitId),
    /// Several units claim the path at the same deepest root; ownership is
    /// reported honestly with all candidates instead of guessing one.
    Ambiguous { candidates: Vec<ProjectUnitId> },
}

/// Typed project-unit filter shared by scoped queries (issue #41).
///
/// Exactly one of `unit` or `package` must be set; validation happens in the
/// query layer with [`ProjectModel::resolve_selector`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectScopeSelector {
    /// Exact unit id, as reported by the `repo_map` project section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<ProjectUnitId>,
    /// Ecosystem package name selecting every unit carrying that name
    /// (including path-fallback groupings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

/// Typed project-scope resolution failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProjectScopeError {
    #[error("project scope selector must set exactly one of `unit` or `package`")]
    InvalidSelector,
    #[error("unknown project unit `{0}`")]
    UnknownUnit(ProjectUnitId),
    #[error("no project unit is named `{0}`")]
    UnknownPackage(String),
}

/// Typed, queryable project model published with one workspace revision.
///
/// All collections are bounded by the `MAX_PROJECT_*` constants; anything cut
/// by a bound is counted in the matching `*_omitted` field rather than
/// silently dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectModel {
    pub workspaces: Vec<ProjectWorkspace>,
    pub workspaces_omitted: u64,
    pub units: Vec<ProjectUnit>,
    pub units_omitted: u64,
    pub issues: Vec<ProjectManifestIssue>,
    pub issues_omitted: u64,
}

impl ProjectModel {
    pub fn unit(&self, id: &ProjectUnitId) -> Option<&ProjectUnit> {
        self.units.iter().find(|unit| &unit.id == id)
    }

    /// Deterministic structural ownership of one path.
    ///
    /// Ecosystem units claim only sources of their own language. Among
    /// claiming units the deepest root wins; a tie at the deepest root is
    /// [`ProjectOwnership::Ambiguous`]. Paths no ecosystem unit claims fall
    /// back to their top-level-directory unit, if one was retained.
    pub fn ownership(&self, path: &RepoRelativePath, language: Language) -> ProjectOwnership {
        let mut best_depth = None;
        let mut candidates: Vec<ProjectUnitId> = Vec::new();
        for unit in &self.units {
            if unit.kind == ProjectUnitKind::PathFallback || unit.language != Some(language) {
                continue;
            }
            let Some(depth) = unit.claim_depth(path) else {
                continue;
            };
            match best_depth {
                None => {
                    best_depth = Some(depth);
                    candidates.push(unit.id.clone());
                }
                Some(current) if depth > current => {
                    best_depth = Some(depth);
                    candidates.clear();
                    candidates.push(unit.id.clone());
                }
                Some(current) if depth == current => {
                    if !candidates.contains(&unit.id) {
                        candidates.push(unit.id.clone());
                    }
                }
                Some(_) => {}
            }
        }
        match candidates.len() {
            0 => self
                .units
                .iter()
                .filter(|unit| unit.kind == ProjectUnitKind::PathFallback)
                .filter(|unit| path_within(unit.root.as_ref(), path))
                .max_by_key(|unit| root_depth(unit.root.as_ref()))
                .map_or(ProjectOwnership::Unassigned, |unit| {
                    ProjectOwnership::Owned(unit.id.clone())
                }),
            1 => {
                let mut candidates = candidates;
                match candidates.pop() {
                    Some(unit) => ProjectOwnership::Owned(unit),
                    None => ProjectOwnership::Unassigned,
                }
            }
            _ => {
                candidates.truncate(MAX_AMBIGUITY_CANDIDATES);
                ProjectOwnership::Ambiguous { candidates }
            }
        }
    }

    /// Resolves a typed selector to the concrete unit ids it matches.
    pub fn resolve_selector(
        &self,
        selector: &ProjectScopeSelector,
    ) -> Result<BTreeSet<ProjectUnitId>, ProjectScopeError> {
        match (&selector.unit, &selector.package) {
            (Some(unit), None) => {
                if self.unit(unit).is_some() {
                    Ok(BTreeSet::from([unit.clone()]))
                } else {
                    Err(ProjectScopeError::UnknownUnit(unit.clone()))
                }
            }
            (None, Some(package)) => {
                let matches: BTreeSet<ProjectUnitId> = self
                    .units
                    .iter()
                    .filter(|unit| unit.name == *package)
                    .map(|unit| unit.id.clone())
                    .collect();
                if matches.is_empty() {
                    Err(ProjectScopeError::UnknownPackage(package.clone()))
                } else {
                    Ok(matches)
                }
            }
            _ => Err(ProjectScopeError::InvalidSelector),
        }
    }
}

fn path_within(root: Option<&RepoRelativePath>, path: &RepoRelativePath) -> bool {
    let Some(root) = root else {
        return true;
    };
    path == root
        || path
            .as_str()
            .strip_prefix(root.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn root_depth(root: Option<&RepoRelativePath>) -> usize {
    root.map_or(0, |root| root.as_str().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(raw: &str) -> Result<RepoRelativePath, Box<dyn std::error::Error>> {
        Ok(RepoRelativePath::new(raw)?)
    }

    fn cargo_unit(
        name: &str,
        root: Option<&str>,
        manifest: &str,
    ) -> Result<ProjectUnit, Box<dyn std::error::Error>> {
        let root_path = root.map(path).transpose()?;
        Ok(ProjectUnit {
            id: ProjectUnitId::new(ProjectUnitKind::CargoPackage, root_path.as_ref(), name),
            kind: ProjectUnitKind::CargoPackage,
            name: name.to_owned(),
            root: root_path,
            manifest: Some(path(manifest)?),
            language: Some(Language::Rust),
            source_roots: Vec::new(),
            source_roots_omitted: 0,
            dependencies: Vec::new(),
            dependencies_omitted: 0,
        })
    }

    fn composer_unit(
        name: &str,
        manifest: &str,
        roots: &[(&str, SourceRole)],
    ) -> Result<ProjectUnit, Box<dyn std::error::Error>> {
        let manifest_path = path(manifest)?;
        let directory = manifest.rsplit_once('/').map(|(directory, _)| directory);
        let root_path = directory.map(path).transpose()?;
        Ok(ProjectUnit {
            id: ProjectUnitId::new(ProjectUnitKind::ComposerPackage, root_path.as_ref(), name),
            kind: ProjectUnitKind::ComposerPackage,
            name: name.to_owned(),
            root: root_path,
            manifest: Some(manifest_path),
            language: Some(Language::Php),
            source_roots: roots
                .iter()
                .map(|(root, role)| {
                    Ok(ProjectSourceRoot {
                        root: Some(path(root)?),
                        role: *role,
                    })
                })
                .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?,
            source_roots_omitted: 0,
            dependencies: Vec::new(),
            dependencies_omitted: 0,
        })
    }

    #[test]
    fn nested_cargo_workspace_ownership_prefers_the_deepest_member()
    -> Result<(), Box<dyn std::error::Error>> {
        let model = ProjectModel {
            units: vec![
                cargo_unit("workspace", None, "Cargo.toml")?,
                cargo_unit("core", Some("crates/core"), "crates/core/Cargo.toml")?,
            ],
            ..ProjectModel::default()
        };
        assert_eq!(
            model.ownership(&path("crates/core/src/lib.rs")?, Language::Rust),
            ProjectOwnership::Owned(ProjectUnitId::new(
                ProjectUnitKind::CargoPackage,
                Some(&path("crates/core")?),
                "core",
            ))
        );
        assert_eq!(
            model.ownership(&path("src/lib.rs")?, Language::Rust),
            ProjectOwnership::Owned(ProjectUnitId::new(
                ProjectUnitKind::CargoPackage,
                None,
                "workspace",
            ))
        );
        Ok(())
    }

    #[test]
    fn composer_ownership_uses_psr4_roots_and_ignores_other_languages()
    -> Result<(), Box<dyn std::error::Error>> {
        let model = ProjectModel {
            units: vec![composer_unit(
                "acme/blog",
                "composer.json",
                &[("src", SourceRole::Production), ("tests", SourceRole::Test)],
            )?],
            ..ProjectModel::default()
        };
        assert!(matches!(
            model.ownership(&path("src/Post.php")?, Language::Php),
            ProjectOwnership::Owned(_)
        ));
        // Outside every declared PSR-4 root the package claims nothing.
        assert_eq!(
            model.ownership(&path("bin/console.php")?, Language::Php),
            ProjectOwnership::Unassigned
        );
        // A Rust file at the same path is never claimed by a Composer unit.
        assert_eq!(
            model.ownership(&path("src/lib.rs")?, Language::Rust),
            ProjectOwnership::Unassigned
        );
        Ok(())
    }

    #[test]
    fn tied_deepest_claims_are_ambiguous_with_all_candidates()
    -> Result<(), Box<dyn std::error::Error>> {
        let model = ProjectModel {
            units: vec![
                composer_unit(
                    "acme/one",
                    "packages/one/composer.json",
                    &[("src", SourceRole::Production)],
                )?,
                composer_unit(
                    "acme/two",
                    "packages/two/composer.json",
                    &[("src", SourceRole::Production)],
                )?,
            ],
            ..ProjectModel::default()
        };
        match model.ownership(&path("src/Shared.php")?, Language::Php) {
            ProjectOwnership::Ambiguous { candidates } => assert_eq!(candidates.len(), 2),
            other => {
                return Err(format!("expected ambiguous ownership, got {other:?}").into());
            }
        }
        Ok(())
    }

    #[test]
    fn path_fallback_units_claim_unowned_sources() -> Result<(), Box<dyn std::error::Error>> {
        let model = ProjectModel {
            units: vec![
                cargo_unit("core", Some("crates/core"), "crates/core/Cargo.toml")?,
                ProjectUnit {
                    id: ProjectUnitId::new(
                        ProjectUnitKind::PathFallback,
                        Some(&path("scripts")?),
                        "scripts",
                    ),
                    kind: ProjectUnitKind::PathFallback,
                    name: "scripts".to_owned(),
                    root: Some(path("scripts")?),
                    manifest: None,
                    language: None,
                    source_roots: Vec::new(),
                    source_roots_omitted: 0,
                    dependencies: Vec::new(),
                    dependencies_omitted: 0,
                },
            ],
            ..ProjectModel::default()
        };
        assert_eq!(
            model.ownership(&path("scripts/deploy.py")?, Language::Python),
            ProjectOwnership::Owned(ProjectUnitId::new(
                ProjectUnitKind::PathFallback,
                Some(&path("scripts")?),
                "scripts",
            ))
        );
        assert_eq!(
            model.ownership(&path("crates/core/build.py")?, Language::Python),
            ProjectOwnership::Unassigned
        );
        Ok(())
    }

    #[test]
    fn selector_resolution_is_typed_and_honest() -> Result<(), Box<dyn std::error::Error>> {
        let model = ProjectModel {
            units: vec![
                cargo_unit("core", Some("crates/core"), "crates/core/Cargo.toml")?,
                cargo_unit("core", Some("vendor/core"), "vendor/core/Cargo.toml")?,
            ],
            ..ProjectModel::default()
        };
        let id = ProjectUnitId::new(
            ProjectUnitKind::CargoPackage,
            Some(&path("crates/core")?),
            "core",
        );
        let by_unit = model.resolve_selector(&ProjectScopeSelector {
            unit: Some(id.clone()),
            package: None,
        });
        assert_eq!(by_unit, Ok(BTreeSet::from([id])));
        let by_name = model.resolve_selector(&ProjectScopeSelector {
            unit: None,
            package: Some("core".to_owned()),
        });
        assert_eq!(by_name.map(|set| set.len()), Ok(2));
        assert_eq!(
            model.resolve_selector(&ProjectScopeSelector {
                unit: None,
                package: Some("missing".to_owned()),
            }),
            Err(ProjectScopeError::UnknownPackage("missing".to_owned()))
        );
        assert_eq!(
            model.resolve_selector(&ProjectScopeSelector::default()),
            Err(ProjectScopeError::InvalidSelector)
        );
        Ok(())
    }

    #[test]
    fn selector_serializes_with_optional_fields_only() -> Result<(), Box<dyn std::error::Error>> {
        let selector = ProjectScopeSelector {
            unit: None,
            package: Some("acme/blog".to_owned()),
        };
        assert_eq!(
            serde_json::to_value(&selector)?,
            serde_json::json!({ "package": "acme/blog" })
        );
        let empty: ProjectScopeSelector = serde_json::from_value(serde_json::json!({}))?;
        assert_eq!(empty, ProjectScopeSelector::default());
        Ok(())
    }
}
