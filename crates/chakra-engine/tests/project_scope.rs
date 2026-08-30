//! Query-layer coverage for the typed project scope model (issue #41).

use std::error::Error;
use std::path::Path;

use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::operation::OperationContext;
use chakra_domain::project::{
    ProjectDependency, ProjectDependencyKind, ProjectModel, ProjectScopeSelector,
    ProjectSourceRoot, ProjectUnit, ProjectUnitId, ProjectUnitKind, ProjectWorkspace,
    ProjectWorkspaceKind,
};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    CallersRequest, ChangeKind, ContextRequest, DiffContextRequest, QueryError, QueryService,
    RepoMapRequest, ResolvedDiffScope, SourceFilter, SymbolRef, SymbolSearchRequest,
};
use chakra_domain::revision::Revision;
use chakra_domain::source::{SourceClassification, SourceMetadata, SourcePackage, SourceRole};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_domain::symbol::{EdgeKind, EntityId, Language, SymbolKey, SymbolKind};
use chakra_engine::{
    DiffWorkspace, SymbolGraph, WorkspaceDiff, WorkspaceDiffError, WorkspaceDiffProvider,
    WorkspaceEngine, WorkspaceFileChange,
};

const CORE_RS: &str = "crates/core/src/lib.rs";
const CLI_RS: &str = "crates/cli/src/main.rs";
const TOOL_PY: &str = "scripts/tool.py";

fn path(raw: &str) -> Result<RepoRelativePath, Box<dyn Error>> {
    Ok(RepoRelativePath::new(raw)?)
}

fn cargo_metadata(name: &str, root: &str) -> Result<SourceMetadata, Box<dyn Error>> {
    Ok(SourceMetadata {
        role: SourceRole::Production,
        classification: SourceClassification::CargoMetadata,
        package: Some(SourcePackage {
            name: name.to_owned(),
            root: Some(path(root)?),
        }),
    })
}

fn cargo_unit(
    name: &str,
    root: &str,
    dependencies: Vec<ProjectDependency>,
) -> Result<ProjectUnit, Box<dyn Error>> {
    let root_path = path(root)?;
    Ok(ProjectUnit {
        id: ProjectUnitId::new(ProjectUnitKind::CargoPackage, Some(&root_path), name),
        kind: ProjectUnitKind::CargoPackage,
        name: name.to_owned(),
        root: Some(root_path.clone()),
        manifest: Some(path(&format!("{root}/Cargo.toml"))?),
        language: Some(Language::Rust),
        source_roots: vec![ProjectSourceRoot {
            root: Some(path(&format!("{root}/src"))?),
            role: SourceRole::Production,
        }],
        source_roots_omitted: 0,
        dependencies,
        dependencies_omitted: 0,
    })
}

fn core_id() -> Result<ProjectUnitId, Box<dyn Error>> {
    Ok(ProjectUnitId::new(
        ProjectUnitKind::CargoPackage,
        Some(&path("crates/core")?),
        "core",
    ))
}

fn cli_id() -> Result<ProjectUnitId, Box<dyn Error>> {
    Ok(ProjectUnitId::new(
        ProjectUnitKind::CargoPackage,
        Some(&path("crates/cli")?),
        "cli",
    ))
}

fn project_model() -> Result<ProjectModel, Box<dyn Error>> {
    let core = cargo_unit("core", "crates/core", Vec::new())?;
    let cli = cargo_unit(
        "cli",
        "crates/cli",
        vec![ProjectDependency {
            name: "core".to_owned(),
            kind: ProjectDependencyKind::Normal,
            target: Some(core_id()?),
        }],
    )?;
    let scripts_root = path("scripts")?;
    let scripts = ProjectUnit {
        id: ProjectUnitId::new(
            ProjectUnitKind::PathFallback,
            Some(&scripts_root),
            "scripts",
        ),
        kind: ProjectUnitKind::PathFallback,
        name: "scripts".to_owned(),
        root: Some(scripts_root),
        manifest: None,
        language: None,
        source_roots: Vec::new(),
        source_roots_omitted: 0,
        dependencies: Vec::new(),
        dependencies_omitted: 0,
    };
    Ok(ProjectModel {
        workspaces: vec![ProjectWorkspace {
            kind: ProjectWorkspaceKind::Cargo,
            root: None,
            members: vec![core_id()?, cli_id()?],
        }],
        workspaces_omitted: 0,
        units: vec![core, cli, scripts],
        units_omitted: 0,
        issues: Vec::new(),
        issues_omitted: 0,
    })
}

fn range(file: &RepoRelativePath, line: u32) -> Result<SourceRange, Box<dyn Error>> {
    Ok(SourceRange::new(
        file.clone(),
        TextPosition::new(line, 1)?,
        TextPosition::new(line + 1, 1)?,
    )?)
}

fn add_function(
    graph: &mut SymbolGraph,
    qualified_name: &str,
    file: &RepoRelativePath,
    line: u32,
) -> Result<EntityId, Box<dyn Error>> {
    Ok(graph.add_symbol(
        SymbolKey {
            language: Language::Rust,
            qualified_name: qualified_name.to_owned(),
            container: None,
            kind: SymbolKind::Function,
            path: file.clone(),
        },
        range(file, line)?,
        None,
        Provenance::TreeSitter,
        Precision::Syntax,
    )?)
}

fn engine() -> Result<WorkspaceEngine, Box<dyn Error>> {
    engine_with_additional_core_caller(false)
}

fn engine_with_additional_core_caller(
    include_additional_core_caller: bool,
) -> Result<WorkspaceEngine, Box<dyn Error>> {
    let identity = WorkspaceIdentity::for_primary_worktree(Path::new("."))?;
    let engine = WorkspaceEngine::new(identity);
    let mut graph = SymbolGraph::new();
    let core = path(CORE_RS)?;
    let cli = path(CLI_RS)?;
    graph.add_file_with_metadata(
        core.clone(),
        "pub fn shared() {}\n",
        cargo_metadata("core", "crates/core")?,
    )?;
    graph.add_file_with_metadata(
        cli.clone(),
        "fn main() { shared(); }\n",
        cargo_metadata("cli", "crates/cli")?,
    )?;
    graph.add_file(path(TOOL_PY)?, "def tool():\n    pass\n")?;
    let shared = add_function(&mut graph, "core::shared", &core, 1)?;
    let main = add_function(&mut graph, "cli::main", &cli, 1)?;
    graph.add_edge(
        EdgeKind::Calls,
        main,
        shared,
        Provenance::TreeSitter,
        Precision::Syntax,
        None,
    )?;
    if include_additional_core_caller {
        let additional = path("crates/core/src/zz_caller.rs")?;
        graph.add_file_with_metadata(
            additional.clone(),
            "pub fn zz_caller() { shared(); }\n",
            cargo_metadata("core", "crates/core")?,
        )?;
        let caller = add_function(&mut graph, "zz::core_caller", &additional, 1)?;
        graph.add_edge(
            EdgeKind::Calls,
            caller,
            shared,
            Provenance::TreeSitter,
            Precision::Syntax,
            None,
        )?;
    }
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_project_model(project_model()?);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;
    Ok(engine)
}

#[test]
fn project_scope_is_applied_before_related_item_limits() -> Result<(), Box<dyn Error>> {
    let engine = engine_with_additional_core_caller(true)?;
    let scoped = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName("core::shared".to_owned())),
        source: unit_selector(core_id()?),
        limit: Some(1),
        ..CallersRequest::default()
    })?;

    assert_eq!(scoped.data.callers.len(), 1);
    assert_eq!(
        scoped.data.callers[0].symbol.qualified_name,
        "zz::core_caller"
    );
    Ok(())
}

fn unit_selector(id: ProjectUnitId) -> SourceFilter {
    SourceFilter {
        project: Some(ProjectScopeSelector {
            unit: Some(id),
            package: None,
        }),
        ..SourceFilter::default()
    }
}

fn package_selector(name: &str) -> SourceFilter {
    SourceFilter {
        project: Some(ProjectScopeSelector {
            unit: None,
            package: Some(name.to_owned()),
        }),
        ..SourceFilter::default()
    }
}

#[test]
fn repo_map_summarizes_and_scopes_by_project_unit() -> Result<(), Box<dyn Error>> {
    let engine = engine()?;
    let map = engine.repo_map(RepoMapRequest {
        include_project_scope: true,
        ..RepoMapRequest::default()
    })?;
    let scope = map
        .data
        .project_scope
        .as_ref()
        .ok_or("project scope section missing")?;
    let core = scope
        .units
        .iter()
        .find(|unit| unit.name == "core")
        .ok_or("core unit summary missing")?;
    assert_eq!(core.id, core_id()?);
    assert_eq!(core.file_count, 1);
    assert_eq!(core.symbol_count, 1);
    let cli = scope
        .units
        .iter()
        .find(|unit| unit.name == "cli")
        .ok_or("cli unit summary missing")?;
    assert_eq!(cli.dependencies.len(), 1);
    assert_eq!(cli.dependencies[0].target.as_ref(), Some(&core_id()?));
    let scripts = scope
        .units
        .iter()
        .find(|unit| unit.kind == ProjectUnitKind::PathFallback)
        .ok_or("path-fallback unit summary missing")?;
    assert_eq!(scripts.name, "scripts");
    assert_eq!(scope.ambiguous_files, 0);
    assert!(scope.issues.is_empty());

    // Without the opt-in flag the section stays absent.
    let plain = engine.repo_map(RepoMapRequest::default())?;
    assert!(plain.data.project_scope.is_none());

    // Typed scoping by unit id and by package name.
    for filter in [unit_selector(core_id()?), package_selector("core")] {
        let scoped = engine.repo_map(RepoMapRequest {
            source: filter,
            ..RepoMapRequest::default()
        })?;
        let files: Vec<_> = scoped
            .data
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(files, vec![CORE_RS], "unit scope must keep only core files");
    }
    // An unknown unit is a typed error, not an empty filter.
    let unknown = engine.repo_map(RepoMapRequest {
        source: package_selector("missing"),
        ..RepoMapRequest::default()
    });
    assert!(matches!(unknown, Err(QueryError::Invalid(_))));

    // The selector survives cursor normalization.
    let scoped = engine.repo_map(RepoMapRequest {
        source: unit_selector(core_id()?),
        limit: Some(1),
        include_project_scope: true,
        ..RepoMapRequest::default()
    })?;
    if let Some(cursor) = scoped.data.next_cursor {
        assert_eq!(
            cursor.scope.source.project,
            Some(ProjectScopeSelector {
                unit: Some(core_id()?),
                package: None,
            })
        );
    }
    Ok(())
}

#[test]
fn symbol_search_scopes_by_project_unit() -> Result<(), Box<dyn Error>> {
    let engine = engine()?;
    let unscoped = engine.symbol_search(SymbolSearchRequest {
        query: "shared".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(unscoped.data.candidates.len(), 1);

    let in_core = engine.symbol_search(SymbolSearchRequest {
        query: "shared".to_owned(),
        source: package_selector("core"),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(in_core.data.candidates.len(), 1);

    let in_cli = engine.symbol_search(SymbolSearchRequest {
        query: "shared".to_owned(),
        source: unit_selector(cli_id()?),
        ..SymbolSearchRequest::default()
    })?;
    assert!(
        in_cli.data.candidates.is_empty(),
        "core::shared must not match the cli unit scope"
    );
    Ok(())
}

#[test]
fn context_and_callers_filter_related_sections_by_project_unit() -> Result<(), Box<dyn Error>> {
    let engine = engine()?;
    let callers = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName("core::shared".to_owned())),
        ..CallersRequest::default()
    })?;
    assert_eq!(callers.data.callers.len(), 1);

    // The only caller lives in `cli`; scoping to `core` removes it while the
    // anchor symbol itself stays untouched.
    let scoped = engine.callers(CallersRequest {
        symbol: Some(SymbolRef::ByName("core::shared".to_owned())),
        source: unit_selector(core_id()?),
        ..CallersRequest::default()
    })?;
    assert_eq!(scoped.data.target.qualified_name, "core::shared");
    assert!(scoped.data.callers.is_empty());

    let context = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("cli::main".to_owned())),
        ..ContextRequest::default()
    })?;
    assert_eq!(context.data.callees.len(), 1);

    let outbound = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("cli::main".to_owned())),
        source: package_selector("core"),
        ..ContextRequest::default()
    })?;
    assert_eq!(
        outbound.data.callees.len(),
        1,
        "callees in the selected unit survive"
    );
    let inbound = engine.context(ContextRequest {
        symbol: Some(SymbolRef::ByName("cli::main".to_owned())),
        source: unit_selector(cli_id()?),
        ..ContextRequest::default()
    })?;
    assert!(
        inbound.data.callees.is_empty(),
        "callees outside the selected unit are filtered"
    );
    Ok(())
}

#[derive(Debug)]
struct TwoFileDiffProvider;

impl WorkspaceDiffProvider for TwoFileDiffProvider {
    fn diff_with_context(
        &self,
        workspace: DiffWorkspace,
        _operation: &OperationContext,
    ) -> Result<WorkspaceDiff, WorkspaceDiffError> {
        let change = |path: &str| {
            RepoRelativePath::new(path).map(|path| WorkspaceFileChange {
                path,
                previous_path: None,
                change: ChangeKind::Modified,
                provenance: Provenance::Git,
                precision: Precision::Precise,
            })
        };
        let files = [change(CORE_RS), change(CLI_RS)]
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error: chakra_domain::location::RepoPathError| {
                WorkspaceDiffError::new(error.to_string())
            })?;
        Ok(WorkspaceDiff {
            revision: Revision(1),
            scope: ResolvedDiffScope {
                requested: workspace.scope,
                base_commit: None,
            },
            files,
            truncation: None,
        })
    }
}

#[test]
fn diff_context_scopes_changed_sections_by_project_unit() -> Result<(), Box<dyn Error>> {
    let engine = engine()?;
    engine
        .install_diff_provider(std::sync::Arc::new(TwoFileDiffProvider))
        .map_err(|error| error.to_string())?;

    let unscoped = engine.diff_context(DiffContextRequest::default())?;
    assert_eq!(unscoped.data.changed_files.len(), 2);
    assert_eq!(unscoped.data.changed_symbols.len(), 2);
    assert_eq!(unscoped.data.related_callers.len(), 1);

    let scoped = engine.diff_context(DiffContextRequest {
        source: unit_selector(core_id()?),
        ..DiffContextRequest::default()
    })?;
    let files: Vec<_> = scoped
        .data
        .changed_files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(files, vec![CORE_RS]);
    let symbols: Vec<_> = scoped
        .data
        .changed_symbols
        .iter()
        .map(|symbol| symbol.symbol.qualified_name.as_str())
        .collect();
    assert_eq!(symbols, vec!["core::shared"]);
    assert!(
        scoped.data.related_callers.is_empty(),
        "the cli caller is outside the core scope"
    );
    Ok(())
}

#[test]
fn repo_map_project_scope_counts_ambiguous_ownership_honestly() -> Result<(), Box<dyn Error>> {
    let identity = WorkspaceIdentity::for_primary_worktree(Path::new("."))?;
    let engine = WorkspaceEngine::new(identity);
    let shared_php = path("packages/shared/src/Shared.php")?;
    let mut graph = SymbolGraph::new();
    graph.add_file_with_metadata(
        shared_php,
        "<?php class Shared {}\n",
        SourceMetadata {
            role: SourceRole::Production,
            classification: SourceClassification::ComposerMetadata,
            package: Some(SourcePackage {
                name: "acme/shared".to_owned(),
                root: Some(path("packages/shared/src")?),
            }),
        },
    )?;
    let composer_unit = |name: &str, manifest: &str| -> Result<ProjectUnit, Box<dyn Error>> {
        let manifest_path = path(manifest)?;
        let directory = manifest
            .rsplit_once('/')
            .map(|(directory, _)| directory)
            .map(path)
            .transpose()?;
        Ok(ProjectUnit {
            id: ProjectUnitId::new(ProjectUnitKind::ComposerPackage, directory.as_ref(), name),
            kind: ProjectUnitKind::ComposerPackage,
            name: name.to_owned(),
            root: directory,
            manifest: Some(manifest_path),
            language: Some(Language::Php),
            source_roots: vec![ProjectSourceRoot {
                root: Some(path("packages/shared/src")?),
                role: SourceRole::Production,
            }],
            source_roots_omitted: 0,
            dependencies: Vec::new(),
            dependencies_omitted: 0,
        })
    };
    let model = ProjectModel {
        units: vec![
            composer_unit("acme/root", "composer.json")?,
            composer_unit("acme/shared", "packages/shared/composer.json")?,
        ],
        ..ProjectModel::default()
    };
    let mut update = engine.begin_update();
    update.replace_graph(graph);
    update.set_project_model(model);
    update.set_status(WorkspaceStatus::Ready);
    update.set_freshness(Freshness::Fresh);
    engine.publish(update)?;

    let map = engine.repo_map(RepoMapRequest {
        include_project_scope: true,
        ..RepoMapRequest::default()
    })?;
    let scope = map
        .data
        .project_scope
        .as_ref()
        .ok_or("project scope section missing")?;
    assert_eq!(
        scope.ambiguous_files, 1,
        "the shared file must be counted as ambiguous, not assigned"
    );
    assert!(
        scope.units.is_empty(),
        "ambiguous files are attributed to no unit"
    );

    // A unit selector matches nothing ambiguous instead of guessing.
    let selector = engine.repo_map(RepoMapRequest {
        source: package_selector("acme/shared"),
        ..RepoMapRequest::default()
    })?;
    assert!(selector.data.files.is_empty());
    Ok(())
}
