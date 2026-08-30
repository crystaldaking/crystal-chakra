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
            escape_id_component(root.map_or(".", RepoRelativePath::as_str)),
            escape_id_component(name)
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn escape_id_component(component: &str) -> String {
    component.replace('%', "%25").replace(':', "%3A")
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
    /// Chakra intentionally did not probe the manifest because a manifest
    /// count, byte, or shared command-time budget was exhausted.
    ProbeOmitted,
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
    /// reported honestly with bounded candidates instead of guessing one.
    Ambiguous {
        candidates: Vec<ProjectUnitId>,
        candidates_omitted: u64,
    },
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

/// Why one project unit's external inputs changed between two model
/// revisions (issue #40). Typed so invalidation and diagnostics never
/// pattern-match on free-form strings.
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
pub enum ProjectUnitChangeKind {
    /// The unit appeared: a manifest was created or a package newly joined.
    Added,
    /// The unit disappeared: its manifest was deleted, renamed away, or the
    /// package left its workspace.
    Removed,
    /// Identity-carrier fields other than source roots or dependency edges
    /// changed (for example the contributing manifest path or language).
    DefinitionChanged,
    /// Declared source roots changed: Cargo targets or Composer PSR-4
    /// autoload/autoload-dev roots.
    SourceRootsChanged,
    /// Declared dependency edges changed.
    DependenciesChanged,
    /// The unit's workspace grouping membership changed.
    MembershipChanged,
}

/// One unit whose external manifest/config inputs changed, with every
/// reason that applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectUnitChange {
    pub unit: ProjectUnitId,
    /// Sorted, deduplicated reasons for the change.
    pub kinds: Vec<ProjectUnitChangeKind>,
}

/// Aggregated per-reason unit change counts of one impact diff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectUnitChangeCounts {
    pub added: u64,
    pub removed: u64,
    pub definition_changed: u64,
    pub source_roots_changed: u64,
    pub dependencies_changed: u64,
    pub membership_changed: u64,
}

/// Typed record of which project units a manifest/config edit invalidated
/// between two model revisions, and which units depend on them (issue #40).
///
/// This is the dependency-tracking contract for derived facts: package
/// moves surface as `Removed` + `Added`, autoload edits as
/// [`ProjectUnitChangeKind::SourceRootsChanged`], membership edits as
/// [`ProjectUnitChangeKind::MembershipChanged`], and dependency-edge edits
/// as [`ProjectUnitChangeKind::DependenciesChanged`]. All collections are
/// bounded; cuts are counted in the matching `*_omitted` field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectModelImpact {
    /// Units whose own manifest evidence changed, sorted by unit id.
    pub changes: Vec<ProjectUnitChange>,
    pub changes_omitted: u64,
    /// Units that declare a dependency edge targeting a changed unit, in
    /// either revision, sorted by unit id. Changed units are not repeated
    /// here.
    pub dependents: Vec<ProjectUnitId>,
    pub dependents_omitted: u64,
    /// Manifests whose recorded probe/parse issue state changed.
    pub manifest_issue_changes: Vec<RepoRelativePath>,
    pub manifest_issue_changes_omitted: u64,
}

impl ProjectModelImpact {
    /// No unit, dependent, or manifest-issue state changed.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
            && self.changes_omitted == 0
            && self.dependents.is_empty()
            && self.manifest_issue_changes.is_empty()
            && self.manifest_issue_changes_omitted == 0
    }

    /// Aggregated per-reason counts over [`Self::changes`], including the
    /// units cut by the change bound (an omitted unit changed for at least
    /// one reason, but the specific kinds are unknown and not counted).
    pub fn counts(&self) -> ProjectUnitChangeCounts {
        let mut counts = ProjectUnitChangeCounts::default();
        for change in &self.changes {
            for kind in &change.kinds {
                match kind {
                    ProjectUnitChangeKind::Added => counts.added += 1,
                    ProjectUnitChangeKind::Removed => counts.removed += 1,
                    ProjectUnitChangeKind::DefinitionChanged => counts.definition_changed += 1,
                    ProjectUnitChangeKind::SourceRootsChanged => counts.source_roots_changed += 1,
                    ProjectUnitChangeKind::DependenciesChanged => counts.dependencies_changed += 1,
                    ProjectUnitChangeKind::MembershipChanged => counts.membership_changed += 1,
                }
            }
        }
        counts
    }
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
                candidates.sort();
                let candidates_omitted =
                    candidates.len().saturating_sub(MAX_AMBIGUITY_CANDIDATES) as u64;
                candidates.truncate(MAX_AMBIGUITY_CANDIDATES);
                ProjectOwnership::Ambiguous {
                    candidates,
                    candidates_omitted,
                }
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

    /// Typed diff of this (current) model against a `previous` revision
    /// (issue #40): which units' external inputs changed, why, and which
    /// unchanged units declare dependency edges targeting them.
    ///
    /// The diff is a pure function of the two models, so the same manifest
    /// evidence always produces the same impact record.
    pub fn impact_since(&self, previous: &ProjectModel) -> ProjectModelImpact {
        let previous_units: std::collections::BTreeMap<&ProjectUnitId, &ProjectUnit> =
            previous.units.iter().map(|unit| (&unit.id, unit)).collect();
        let current_units: std::collections::BTreeMap<&ProjectUnitId, &ProjectUnit> =
            self.units.iter().map(|unit| (&unit.id, unit)).collect();

        let mut kinds: std::collections::BTreeMap<ProjectUnitId, BTreeSet<ProjectUnitChangeKind>> =
            std::collections::BTreeMap::new();
        for (id, unit) in &current_units {
            let mut unit_kinds = BTreeSet::new();
            match previous_units.get(id) {
                None => {
                    unit_kinds.insert(ProjectUnitChangeKind::Added);
                }
                Some(previous_unit) => {
                    if unit.source_roots != previous_unit.source_roots
                        || unit.source_roots_omitted != previous_unit.source_roots_omitted
                    {
                        unit_kinds.insert(ProjectUnitChangeKind::SourceRootsChanged);
                    }
                    if unit.dependencies != previous_unit.dependencies
                        || unit.dependencies_omitted != previous_unit.dependencies_omitted
                    {
                        unit_kinds.insert(ProjectUnitChangeKind::DependenciesChanged);
                    }
                    if unit.manifest != previous_unit.manifest
                        || unit.language != previous_unit.language
                    {
                        unit_kinds.insert(ProjectUnitChangeKind::DefinitionChanged);
                    }
                }
            }
            if !unit_kinds.is_empty() {
                kinds.insert((*id).clone(), unit_kinds);
            }
        }
        for id in previous_units.keys() {
            if !current_units.contains_key(id) {
                kinds
                    .entry((*id).clone())
                    .or_default()
                    .insert(ProjectUnitChangeKind::Removed);
            }
        }
        // Workspace membership is tracked for units present in both
        // revisions; joining or leaving units already read as Added/Removed.
        let previous_membership = workspace_membership(previous);
        let current_membership = workspace_membership(self);
        let membership_ids: BTreeSet<&ProjectUnitId> = previous_membership
            .keys()
            .chain(current_membership.keys())
            .collect();
        for id in membership_ids {
            if previous_membership.get(id) == current_membership.get(id) {
                continue;
            }
            if previous_units.contains_key(id) && current_units.contains_key(id) {
                kinds
                    .entry(id.clone())
                    .or_default()
                    .insert(ProjectUnitChangeKind::MembershipChanged);
            }
        }

        let mut impact = ProjectModelImpact::default();
        let changed_ids: BTreeSet<ProjectUnitId> = kinds.keys().cloned().collect();
        let total_changes = kinds.len();
        for (unit, unit_kinds) in kinds {
            if impact.changes.len() < MAX_PROJECT_UNITS {
                impact.changes.push(ProjectUnitChange {
                    unit,
                    kinds: unit_kinds.into_iter().collect(),
                });
            }
        }
        impact.changes_omitted = total_changes.saturating_sub(impact.changes.len()) as u64;

        // Dependents come from both revisions so that a removed edge still
        // reports the previously dependent unit.
        let mut dependents = BTreeSet::new();
        for unit in previous.units.iter().chain(self.units.iter()) {
            if changed_ids.contains(&unit.id) {
                continue;
            }
            if unit
                .dependencies
                .iter()
                .filter_map(|dependency| dependency.target.as_ref())
                .any(|target| changed_ids.contains(target))
            {
                dependents.insert(unit.id.clone());
            }
        }
        let total_dependents = dependents.len();
        for dependent in dependents.into_iter().take(MAX_PROJECT_UNITS) {
            impact.dependents.push(dependent);
        }
        impact.dependents_omitted = total_dependents.saturating_sub(impact.dependents.len()) as u64;

        let previous_issues = issue_kinds_by_manifest(previous);
        let current_issues = issue_kinds_by_manifest(self);
        let manifests: BTreeSet<&RepoRelativePath> = previous_issues
            .keys()
            .chain(current_issues.keys())
            .copied()
            .collect();
        let mut issue_changes = Vec::new();
        for manifest in manifests {
            if previous_issues.get(manifest) != current_issues.get(manifest) {
                issue_changes.push(manifest.clone());
            }
        }
        let total_issue_changes = issue_changes.len();
        impact.manifest_issue_changes =
            issue_changes.into_iter().take(MAX_PROJECT_ISSUES).collect();
        impact.manifest_issue_changes_omitted =
            total_issue_changes.saturating_sub(impact.manifest_issue_changes.len()) as u64;
        impact
    }
}

/// Maps each workspace member unit id to the workspaces containing it.
fn workspace_membership(
    model: &ProjectModel,
) -> std::collections::BTreeMap<
    ProjectUnitId,
    BTreeSet<(ProjectWorkspaceKind, Option<RepoRelativePath>)>,
> {
    let mut membership: std::collections::BTreeMap<
        ProjectUnitId,
        BTreeSet<(ProjectWorkspaceKind, Option<RepoRelativePath>)>,
    > = std::collections::BTreeMap::new();
    for workspace in &model.workspaces {
        for member in &workspace.members {
            membership
                .entry(member.clone())
                .or_default()
                .insert((workspace.kind, workspace.root.clone()));
        }
    }
    membership
}

/// Maps each manifest with recorded issues to its issue kinds.
fn issue_kinds_by_manifest(
    model: &ProjectModel,
) -> std::collections::BTreeMap<&RepoRelativePath, BTreeSet<ProjectManifestIssueKind>> {
    let mut issues: std::collections::BTreeMap<
        &RepoRelativePath,
        BTreeSet<ProjectManifestIssueKind>,
    > = std::collections::BTreeMap::new();
    for issue in &model.issues {
        issues
            .entry(&issue.manifest)
            .or_default()
            .insert(issue.kind);
    }
    issues
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
            ProjectOwnership::Ambiguous {
                candidates,
                candidates_omitted,
            } => {
                assert_eq!(candidates.len(), 2);
                assert_eq!(candidates_omitted, 0);
            }
            other => {
                return Err(format!("expected ambiguous ownership, got {other:?}").into());
            }
        }
        Ok(())
    }

    #[test]
    fn ambiguous_ownership_reports_candidate_omissions() -> Result<(), Box<dyn std::error::Error>> {
        let mut units = Vec::new();
        for index in 0..=MAX_AMBIGUITY_CANDIDATES {
            units.push(composer_unit(
                &format!("acme/package-{index:02}"),
                &format!("packages/{index:02}/composer.json"),
                &[("src", SourceRole::Production)],
            )?);
        }
        let model = ProjectModel {
            units,
            ..ProjectModel::default()
        };
        match model.ownership(&path("src/Shared.php")?, Language::Php) {
            ProjectOwnership::Ambiguous {
                candidates,
                candidates_omitted,
            } => {
                assert_eq!(candidates.len(), MAX_AMBIGUITY_CANDIDATES);
                assert_eq!(candidates_omitted, 1);
                assert!(candidates.windows(2).all(|pair| pair[0] < pair[1]));
            }
            other => return Err(format!("expected ambiguous ownership, got {other:?}").into()),
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
    fn unit_ids_escape_component_delimiters_without_collisions()
    -> Result<(), Box<dyn std::error::Error>> {
        let root_delimiter = ProjectUnitId::new(
            ProjectUnitKind::PathFallback,
            Some(&path("group:child")?),
            "unit",
        );
        let name_delimiter = ProjectUnitId::new(
            ProjectUnitKind::PathFallback,
            Some(&path("group")?),
            "child:unit",
        );

        assert_ne!(root_delimiter, name_delimiter);
        assert_eq!(root_delimiter.as_str(), "path:group%3Achild:unit");
        assert_eq!(name_delimiter.as_str(), "path:group:child%3Aunit");
        assert_ne!(
            ProjectUnitId::new(
                ProjectUnitKind::PathFallback,
                Some(&path("group%3Achild")?),
                "unit",
            ),
            root_delimiter,
            "percent escaping must not reintroduce an encoded collision"
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

    fn change<'a>(
        model: &'a ProjectModelImpact,
        unit: &ProjectUnitId,
    ) -> Option<&'a ProjectUnitChange> {
        model.changes.iter().find(|change| &change.unit == unit)
    }

    #[test]
    fn unchanged_models_produce_an_empty_impact() -> Result<(), Box<dyn std::error::Error>> {
        let model = ProjectModel {
            units: vec![cargo_unit(
                "core",
                Some("crates/core"),
                "crates/core/Cargo.toml",
            )?],
            ..ProjectModel::default()
        };
        assert!(model.impact_since(&model).is_empty());
        Ok(())
    }

    #[test]
    fn package_move_reads_as_remove_and_add() -> Result<(), Box<dyn std::error::Error>> {
        let previous = ProjectModel {
            units: vec![cargo_unit(
                "core",
                Some("crates/core"),
                "crates/core/Cargo.toml",
            )?],
            ..ProjectModel::default()
        };
        let current = ProjectModel {
            units: vec![cargo_unit(
                "core",
                Some("packages/core"),
                "packages/core/Cargo.toml",
            )?],
            ..ProjectModel::default()
        };
        let impact = current.impact_since(&previous);
        let counts = impact.counts();
        assert_eq!(counts.added, 1);
        assert_eq!(counts.removed, 1);
        assert_eq!(impact.changes.len(), 2);
        assert!(impact.dependents.is_empty());
        Ok(())
    }

    #[test]
    fn autoload_edits_read_as_source_root_changes() -> Result<(), Box<dyn std::error::Error>> {
        let previous = ProjectModel {
            units: vec![composer_unit(
                "acme/blog",
                "composer.json",
                &[("src", SourceRole::Production)],
            )?],
            ..ProjectModel::default()
        };
        let current = ProjectModel {
            units: vec![composer_unit(
                "acme/blog",
                "composer.json",
                &[
                    ("src", SourceRole::Production),
                    ("lib", SourceRole::Production),
                ],
            )?],
            ..ProjectModel::default()
        };
        let impact = current.impact_since(&previous);
        assert_eq!(impact.changes.len(), 1);
        assert_eq!(
            impact.changes[0].kinds,
            vec![ProjectUnitChangeKind::SourceRootsChanged]
        );
        Ok(())
    }

    #[test]
    fn dependency_edits_report_the_edge_owner_and_unchanged_dependents()
    -> Result<(), Box<dyn std::error::Error>> {
        let core = ProjectUnitId::new(
            ProjectUnitKind::CargoPackage,
            Some(&path("crates/core")?),
            "core",
        );
        let cli = ProjectUnitId::new(
            ProjectUnitKind::CargoPackage,
            Some(&path("crates/cli")?),
            "cli",
        );
        let dependent = ProjectUnitId::new(
            ProjectUnitKind::CargoPackage,
            Some(&path("crates/app")?),
            "app",
        );
        let mut cli_unit = cargo_unit("cli", Some("crates/cli"), "crates/cli/Cargo.toml")?;
        let mut app_unit = cargo_unit("app", Some("crates/app"), "crates/app/Cargo.toml")?;
        app_unit.dependencies = vec![ProjectDependency {
            name: "core".to_owned(),
            kind: ProjectDependencyKind::Normal,
            target: Some(core.clone()),
        }];
        let previous = ProjectModel {
            units: vec![
                cargo_unit("core", Some("crates/core"), "crates/core/Cargo.toml")?,
                cli_unit.clone(),
                app_unit.clone(),
            ],
            ..ProjectModel::default()
        };
        cli_unit.dependencies = vec![ProjectDependency {
            name: "serde".to_owned(),
            kind: ProjectDependencyKind::Normal,
            target: None,
        }];
        let current = ProjectModel {
            units: vec![
                cargo_unit("core", Some("crates/core"), "crates/core/Cargo.toml")?,
                cli_unit,
                app_unit,
            ],
            ..ProjectModel::default()
        };
        let impact = current.impact_since(&previous);
        assert_eq!(
            change(&impact, &cli).map(|change| change.kinds.as_slice()),
            Some([ProjectUnitChangeKind::DependenciesChanged].as_slice())
        );
        // `app` depends on `core`, which did not change: no dependent noise.
        assert!(impact.dependents.is_empty());

        // Now change `core` itself: `app` must surface as a dependent.
        let mut renamed_core_dependencies = previous.units.clone();
        renamed_core_dependencies[0].dependencies = vec![ProjectDependency {
            name: "anyhow".to_owned(),
            kind: ProjectDependencyKind::Normal,
            target: None,
        }];
        let current = ProjectModel {
            units: renamed_core_dependencies,
            ..ProjectModel::default()
        };
        let impact = current.impact_since(&previous);
        assert_eq!(
            change(&impact, &core).map(|change| change.kinds.as_slice()),
            Some([ProjectUnitChangeKind::DependenciesChanged].as_slice())
        );
        assert_eq!(impact.dependents, vec![dependent]);
        Ok(())
    }

    #[test]
    fn workspace_membership_changes_are_typed() -> Result<(), Box<dyn std::error::Error>> {
        let core = cargo_unit("core", Some("crates/core"), "crates/core/Cargo.toml")?;
        let previous = ProjectModel {
            workspaces: vec![ProjectWorkspace {
                kind: ProjectWorkspaceKind::Cargo,
                root: None,
                members: Vec::new(),
            }],
            units: vec![core.clone()],
            ..ProjectModel::default()
        };
        let current = ProjectModel {
            workspaces: vec![ProjectWorkspace {
                kind: ProjectWorkspaceKind::Cargo,
                root: None,
                members: vec![core.id.clone()],
            }],
            units: vec![core.clone()],
            ..ProjectModel::default()
        };
        let impact = current.impact_since(&previous);
        assert_eq!(
            change(&impact, &core.id).map(|change| change.kinds.as_slice()),
            Some([ProjectUnitChangeKind::MembershipChanged].as_slice())
        );
        Ok(())
    }

    #[test]
    fn manifest_issue_transitions_are_recorded() -> Result<(), Box<dyn std::error::Error>> {
        let previous = ProjectModel::default();
        let current = ProjectModel {
            issues: vec![ProjectManifestIssue {
                manifest: path("composer.json")?,
                kind: ProjectManifestIssueKind::MalformedContent,
            }],
            ..ProjectModel::default()
        };
        let impact = current.impact_since(&previous);
        assert_eq!(impact.manifest_issue_changes, vec![path("composer.json")?]);
        assert!(impact.changes.is_empty());
        assert!(!impact.is_empty());
        Ok(())
    }
}
