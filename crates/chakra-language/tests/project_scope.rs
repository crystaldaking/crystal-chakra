//! Live reconcile coverage for the typed project model (issue #41):
//! metadata-only manifest edits must republish the model in the same atomic
//! revision, and fresh queries must observe it (read-your-writes).

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::project::ProjectUnitKind;
use chakra_domain::query::{QueryService, RepoMapRequest};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_engine::WorkspaceEngine;
use chakra_language::{index_repository, start_live_index};
use tempfile::TempDir;

fn write(root: &Path, path: &str, source: &str) -> Result<(), Box<dyn Error>> {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, source)?;
    Ok(())
}

fn repository() -> Result<TempDir, Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    write(
        repository.path(),
        "composer.json",
        r#"{
            "name": "acme/blog",
            "autoload": { "psr-4": { "Acme\\Blog\\": "src/" } }
        }"#,
    )?;
    write(
        repository.path(),
        "src/Post.php",
        "<?php\nfinal class Post {\n    public function title(): string {\n        return \"hi\";\n    }\n}\n",
    )?;
    Ok(repository)
}

fn blog_unit_dependencies(engine: &WorkspaceEngine) -> Result<Vec<String>, Box<dyn Error>> {
    let map = engine.repo_map(RepoMapRequest {
        include_project_scope: true,
        ..RepoMapRequest::default()
    })?;
    let scope = map
        .data
        .project_scope
        .as_ref()
        .ok_or("project scope section missing")?;
    let blog = scope
        .units
        .iter()
        .find(|unit| unit.kind == ProjectUnitKind::ComposerPackage && unit.name == "acme/blog")
        .ok_or("acme/blog unit summary missing")?;
    Ok(blog
        .dependencies
        .iter()
        .map(|dependency| dependency.name.clone())
        .collect())
}

#[test]
fn metadata_only_manifest_edit_republishes_the_project_model() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let report = index_repository(repository.path())?;
    assert!(
        report
            .project_model
            .units
            .iter()
            .any(|unit| unit.kind == ProjectUnitKind::ComposerPackage),
        "cold build must carry the composer unit: {:?}",
        report.project_model.units
    );
    let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
    let engine = Arc::new(WorkspaceEngine::new(identity));
    let mut update = engine.begin_update();
    update.set_provider_inputs(report.provider_inputs.clone());
    update.set_project_model(report.project_model.clone());
    update.replace_graph(report.graph);
    update.set_indexing(report.metrics.indexing);
    update.set_status(WorkspaceStatus::Indexing);
    update.set_freshness(Freshness::Stale);
    engine.publish(update)?;
    let live = start_live_index(report.repository_root, report.syntax_index, engine.clone())?;
    let initial_revision = engine.snapshot().revision();
    assert!(blog_unit_dependencies(&engine)?.is_empty());
    let reparsed_before = live.metrics().files_reparsed;

    // Metadata-only edit: no source file changes, only a new requirement.
    write(
        repository.path(),
        "composer.json",
        r#"{
            "name": "acme/blog",
            "require": { "monolog/monolog": "^3.0" },
            "autoload": { "psr-4": { "Acme\\Blog\\": "src/" } }
        }"#,
    )?;
    engine.require_fresh()?;

    assert!(
        engine.snapshot().revision() > initial_revision,
        "a metadata-only change must publish a new revision"
    );
    assert_eq!(
        blog_unit_dependencies(&engine)?,
        vec!["monolog/monolog".to_owned()],
        "a fresh query must observe the rebuilt project model"
    );
    assert_eq!(
        live.metrics().files_reparsed,
        reparsed_before,
        "a metadata-only change must not reparse unchanged sources"
    );
    live.shutdown()?;
    Ok(())
}
