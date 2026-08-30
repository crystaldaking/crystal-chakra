//! Dependency-aware invalidation regressions (issue #40) over hermetic
//! temporary Git repositories: manifest/config edits invalidate exactly the
//! affected derived facts, ordinary one-file edits never escalate to
//! repository-wide invalidation, and every incremental result matches a full
//! rebuild fingerprint.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::location::RepoRelativePath;
use chakra_domain::query::{QueryService, SymbolSearchRequest};
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_domain::symbol::CallResolution;
use chakra_engine::{SymbolGraph, WorkspaceEngine};
use chakra_language::{ReconcileReport, WorkspaceSyntaxIndex, index_repository, start_live_index};
use tempfile::TempDir;

fn write(root: &Path, path: &str, contents: &str) -> Result<(), Box<dyn Error>> {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    Ok(())
}

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

fn repository() -> Result<TempDir, Box<dyn Error>> {
    let repository = TempDir::new()?;
    command(repository.path(), "git", &["init", "--quiet"])?;
    command(
        repository.path(),
        "git",
        &["config", "user.email", "tests@example.invalid"],
    )?;
    command(
        repository.path(),
        "git",
        &["config", "user.name", "Chakra Tests"],
    )?;
    Ok(repository)
}

fn reconcile(root: &Path, index: &WorkspaceSyntaxIndex) -> Result<ReconcileReport, Box<dyn Error>> {
    let scan = index.scan_repository(root)?;
    Ok(index.reconcile_sources(scan)?)
}

/// Content-stable fingerprint: entity ids are revision-local, so they are
/// mapped to symbol keys before comparison. A dependency-scoped incremental
/// reconcile must produce exactly the full-rebuild fingerprint.
fn graph_fingerprint(graph: &SymbolGraph) -> String {
    let mut out = String::new();
    for summary in graph.file_summaries() {
        out.push_str(&format!("{summary:?};"));
    }
    let key_of = |id| {
        graph
            .symbol(id)
            .map(|symbol| format!("{:?}", symbol.key))
            .unwrap_or_else(|| "<missing>".to_owned())
    };
    for symbol in graph.symbols() {
        out.push_str(&format!(
            "{:?}{:?}{:?};",
            symbol.key, symbol.location, symbol.signature
        ));
        let mut edges: Vec<String> = graph
            .outgoing_edges(symbol.id)
            .iter()
            .map(|edge| {
                format!(
                    "{:?}:{:?}->{:?}:{:?};",
                    edge.kind,
                    edge.location,
                    key_of(edge.from),
                    key_of(edge.to)
                )
            })
            .collect();
        edges.sort();
        for edge in edges {
            out.push_str(&edge);
        }
        for call in graph.call_sites_from(symbol.id) {
            let resolution = match &call.resolution {
                CallResolution::Resolved { target } => key_of(*target),
                other => format!("{other:?}"),
            };
            out.push_str(&format!(
                "call:{:?}:{:?}:{}:{:?}:{:?}:{resolution};",
                call.form, call.target_kind, call.name, call.qualifier, call.location
            ));
        }
    }
    out
}

fn assert_matches_full_rebuild(
    root: &Path,
    reconciled: &SymbolGraph,
) -> Result<(), Box<dyn Error>> {
    let rebuilt = index_repository(root)?;
    assert_eq!(
        graph_fingerprint(reconciled),
        graph_fingerprint(&rebuilt.graph),
        "incremental dependency invalidation diverged from a full rebuild"
    );
    reconciled.validate_consistency()?;
    Ok(())
}

#[test]
fn one_file_source_edit_never_escalates_beyond_its_file() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    write(root, "src/a.rs", "pub fn alpha() { beta(); }\n")?;
    write(root, "src/b.rs", "pub fn beta() {}\n")?;
    write(root, "app/Service.php", "<?php class Service {}\n")?;
    let report = index_repository(root)?;

    write(root, "src/a.rs", "pub fn alpha() { beta(); beta(); }\n")?;
    let reconciled = reconcile(root, &report.syntax_index)?;
    let metrics = reconciled.metrics;
    assert_eq!(metrics.reparsed_files, 1, "only the edited file reparses");
    assert_eq!(metrics.modified_files, 1);
    assert_eq!(metrics.created_files, 0);
    assert_eq!(metrics.deleted_files, 0);
    assert_eq!(metrics.metadata_files_recomputed, 0);
    assert_eq!(metrics.framework_config_changes, 0);
    assert_eq!(
        metrics.dependency_impact,
        Default::default(),
        "a source edit records no project-unit invalidation"
    );
    assert!(reconciled.dependency_impact.is_none());
    assert!(reconciled.project_model.is_none());
    assert!(
        metrics.publication.structurally_incremental,
        "a one-file edit must stay a structural delta"
    );
    assert_eq!(metrics.publication.rebuilt_files, 1);
    let graph = reconciled.graph.ok_or("edited graph missing")?;
    assert_matches_full_rebuild(root, &graph)?;
    Ok(())
}

#[test]
fn metadata_only_composer_edit_invalidates_nothing() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    write(
        root,
        "composer.json",
        r#"{"name":"acme/app","description":"before","autoload":{"psr-4":{"Acme\\":"src/"}}}"#,
    )?;
    write(
        root,
        "src/Service.php",
        "<?php namespace Acme; class Service {}\n",
    )?;
    let report = index_repository(root)?;

    // A description-only edit changes no modeled unit and no classified
    // file metadata.
    write(
        root,
        "composer.json",
        r#"{"name":"acme/app","description":"after","autoload":{"psr-4":{"Acme\\":"src/"}}}"#,
    )?;
    let reconciled = reconcile(root, &report.syntax_index)?;
    assert_eq!(reconciled.metrics.reparsed_files, 0);
    assert_eq!(reconciled.metrics.metadata_files_recomputed, 0);
    assert_eq!(reconciled.metrics.dependency_impact, Default::default());
    assert!(reconciled.dependency_impact.is_none());
    assert!(reconciled.project_model.is_none());
    assert!(
        reconciled.graph.is_none(),
        "a metadata-only manifest edit must not rebuild any graph"
    );
    Ok(())
}

#[test]
fn metadata_only_cargo_edit_invalidates_nothing() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/core\"]\nresolver = \"3\"\n",
    )?;
    write(
        root,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\ndescription = \"before\"\n",
    )?;
    write(root, "crates/core/src/lib.rs", "pub fn core() {}\n")?;
    command(root, "cargo", &["generate-lockfile", "--offline"])?;
    let report = index_repository(root)?;
    assert!(
        report.project_model.issues.is_empty(),
        "issues: {:?}",
        report.project_model.issues
    );

    write(
        root,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\ndescription = \"after\"\n",
    )?;
    let reconciled = reconcile(root, &report.syntax_index)?;
    assert_eq!(reconciled.metrics.reparsed_files, 0);
    assert_eq!(reconciled.metrics.metadata_files_recomputed, 0);
    assert_eq!(reconciled.metrics.dependency_impact, Default::default());
    assert!(reconciled.dependency_impact.is_none());
    assert!(reconciled.graph.is_none());
    Ok(())
}

#[test]
fn composer_autoload_change_invalidates_only_newly_claimed_files() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    write(
        root,
        "composer.json",
        r#"{"name":"acme/app","autoload":{"psr-4":{"Acme\\":"src/"}}}"#,
    )?;
    write(
        root,
        "src/Service.php",
        "<?php namespace Acme; class Service {}\n",
    )?;
    write(
        root,
        "lib/Helper.php",
        "<?php namespace Acme\\Lib; class Helper {}\n",
    )?;
    let report = index_repository(root)?;
    let helper = RepoRelativePath::new("lib/Helper.php")?;
    assert_eq!(
        report
            .graph
            .file_metadata(&helper)
            .and_then(|metadata| metadata.package.as_ref()),
        None,
        "lib/ starts outside every PSR-4 root"
    );

    write(
        root,
        "composer.json",
        r#"{"name":"acme/app","autoload":{"psr-4":{"Acme\\":"src/","Acme\\Lib\\":"lib/"}}}"#,
    )?;
    let reconciled = reconcile(root, &report.syntax_index)?;
    let metrics = reconciled.metrics;
    assert_eq!(metrics.reparsed_files, 0, "autoload edits never reparse");
    assert_eq!(
        metrics.metadata_files_recomputed, 1,
        "exactly the newly claimed file is re-materialized"
    );
    assert!(metrics.publication.structurally_incremental);
    let impact = reconciled.dependency_impact.ok_or("impact missing")?;
    let counts = impact.counts();
    assert_eq!(
        counts.source_roots_changed, 1,
        "autoload edits read as typed source-root changes"
    );
    // The deterministic path-fallback unit that grouped lib/ disappears as
    // the Composer unit claims it: the impact is exactly these two units.
    assert_eq!(counts.removed, 1);
    assert_eq!(impact.changes.len(), 2);
    let graph = reconciled.graph.ok_or("autoload graph missing")?;
    assert_eq!(
        graph
            .file_metadata(&helper)
            .and_then(|metadata| metadata.package.as_ref())
            .map(|package| package.name.as_str()),
        Some("acme/app")
    );
    assert_matches_full_rebuild(root, &graph)?;
    Ok(())
}

#[test]
fn cargo_membership_change_invalidates_only_the_joining_crate() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/core\"]\nresolver = \"3\"\n",
    )?;
    write(
        root,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    write(root, "crates/core/src/lib.rs", "pub fn core() {}\n")?;
    write(
        root,
        "crates/extra/Cargo.toml",
        "[package]\nname = \"extra\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    write(root, "crates/extra/src/lib.rs", "pub fn extra() {}\n")?;
    command(root, "cargo", &["generate-lockfile", "--offline"])?;
    let report = index_repository(root)?;
    let extra = RepoRelativePath::new("crates/extra/src/lib.rs")?;
    let core = RepoRelativePath::new("crates/core/src/lib.rs")?;
    // A non-member crate inside the workspace tree cannot be probed, so its
    // manifest records a typed issue and its files degrade to path fallback.
    assert!(
        report
            .project_model
            .issues
            .iter()
            .any(|issue| issue.manifest.as_str() == "crates/extra/Cargo.toml"),
        "issues: {:?}",
        report.project_model.issues
    );
    assert_eq!(
        report
            .graph
            .file_metadata(&extra)
            .and_then(|metadata| metadata.package.as_ref()),
        None
    );
    assert!(
        report
            .graph
            .file_metadata(&core)
            .and_then(|metadata| metadata.package.as_ref())
            .is_some(),
        "core is claimed by its Cargo package"
    );

    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/core\", \"crates/extra\"]\nresolver = \"3\"\n",
    )?;
    command(root, "cargo", &["generate-lockfile", "--offline"])?;
    let reconciled = reconcile(root, &report.syntax_index)?;
    let metrics = reconciled.metrics;
    assert_eq!(metrics.reparsed_files, 0, "membership edits never reparse");
    assert_eq!(
        metrics.metadata_files_recomputed, 1,
        "exactly the joining crate's files are re-materialized"
    );
    let impact = reconciled.dependency_impact.ok_or("impact missing")?;
    let counts = impact.counts();
    assert_eq!(
        counts.added, 1,
        "the joining crate reads as one typed unit addition: {impact:?}"
    );
    assert_eq!(
        impact.manifest_issue_changes.len(),
        1,
        "the cleared probe failure is recorded: {impact:?}"
    );
    let graph = reconciled.graph.ok_or("membership graph missing")?;
    assert_eq!(
        graph
            .file_metadata(&extra)
            .and_then(|metadata| metadata.package.as_ref())
            .map(|package| package.name.as_str()),
        Some("extra")
    );
    assert_matches_full_rebuild(root, &graph)?;
    Ok(())
}

#[test]
fn composer_package_move_scopes_invalidation_to_the_moved_package() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    for package in ["one", "two"] {
        write(
            root,
            &format!("packages/{package}/composer.json"),
            &format!(
                r#"{{"name":"acme/{package}","autoload":{{"psr-4":{{"Acme\\{package_title}\\":"src/"}}}}}}"#,
                package_title = match package {
                    "one" => "One",
                    _ => "Two",
                },
            ),
        )?;
        write(
            root,
            &format!("packages/{package}/src/Thing.php"),
            &format!("<?php class {package}Thing {{}}\n"),
        )?;
    }
    let report = index_repository(root)?;

    fs::rename(root.join("packages/two"), root.join("packages/renamed"))?;
    let reconciled = reconcile(root, &report.syntax_index)?;
    let metrics = reconciled.metrics;
    assert_eq!(metrics.created_files, 1);
    assert_eq!(metrics.deleted_files, 1);
    assert_eq!(
        metrics.reparsed_files, 1,
        "only the moved file is parsed at its new path"
    );
    assert_eq!(
        metrics.metadata_files_recomputed, 0,
        "no retained file's metadata changed: the move is a delete+create pair"
    );
    assert_eq!(
        metrics.unchanged_files, 1,
        "the untouched package is reused"
    );
    let impact = reconciled.dependency_impact.ok_or("impact missing")?;
    let counts = impact.counts();
    assert_eq!(counts.removed, 1);
    assert_eq!(counts.added, 1);
    let graph = reconciled.graph.ok_or("moved graph missing")?;
    assert_matches_full_rebuild(root, &graph)?;
    Ok(())
}

#[test]
fn composer_manifest_delete_scopes_invalidation_to_the_orphaned_files() -> Result<(), Box<dyn Error>>
{
    let repository = repository()?;
    let root = repository.path();
    write(
        root,
        "packages/one/composer.json",
        r#"{"name":"acme/one","autoload":{"psr-4":{"Acme\\One\\":"src/"}}}"#,
    )?;
    write(root, "packages/one/src/One.php", "<?php class One {}\n")?;
    write(
        root,
        "packages/two/composer.json",
        r#"{"name":"acme/two","autoload":{"psr-4":{"Acme\\Two\\":"src/"}}}"#,
    )?;
    write(root, "packages/two/src/Two.php", "<?php class Two {}\n")?;
    let report = index_repository(root)?;

    fs::remove_file(root.join("packages/two/composer.json"))?;
    let reconciled = reconcile(root, &report.syntax_index)?;
    let metrics = reconciled.metrics;
    assert_eq!(metrics.reparsed_files, 0);
    assert_eq!(
        metrics.metadata_files_recomputed, 1,
        "exactly the orphaned package's file falls back to path metadata"
    );
    let impact = reconciled.dependency_impact.ok_or("impact missing")?;
    assert_eq!(impact.counts().removed, 1);
    let graph = reconciled.graph.ok_or("delete graph missing")?;
    let two = RepoRelativePath::new("packages/two/src/Two.php")?;
    assert_eq!(
        graph
            .file_metadata(&two)
            .and_then(|metadata| metadata.package.as_ref()),
        None
    );
    assert_matches_full_rebuild(root, &graph)?;
    Ok(())
}

#[test]
fn laravel_dependency_edit_toggles_framework_facts_for_php_only() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    write(
        root,
        "composer.json",
        r#"{"name":"acme/app","autoload":{"psr-4":{"Acme\\":"src/"}}}"#,
    )?;
    write(
        root,
        "routes/web.php",
        "<?php\nuse Illuminate\\Support\\Facades\\Route;\nfinal class Controller { public function __invoke(): void {} }\nRoute::get('/users', Controller::class);\n",
    )?;
    write(root, "src/lib.rs", "pub fn untouched_rust() {}\n")?;
    let report = index_repository(root)?;
    let baseline_symbols = report.graph.symbol_count();
    let baseline_edges = report.graph.edge_count();

    write(
        root,
        "composer.json",
        r#"{"name":"acme/app","autoload":{"psr-4":{"Acme\\":"src/"}},"require":{"laravel/framework":"^11.0"}}"#,
    )?;
    let reconciled = reconcile(root, &report.syntax_index)?;
    let metrics = reconciled.metrics;
    assert_eq!(metrics.reparsed_files, 0);
    assert_eq!(metrics.framework_config_changes, 1);
    assert_eq!(
        metrics.framework_files_reparsed, 1,
        "only the PHP route file's framework facts are re-derived"
    );
    let impact = reconciled.dependency_impact.ok_or("impact missing")?;
    assert_eq!(impact.counts().dependencies_changed, 1);
    let graph = reconciled.graph.ok_or("laravel graph missing")?;
    assert!(
        graph.symbol_count() > baseline_symbols,
        "framework endpoints must appear after the opt-in"
    );
    assert_matches_full_rebuild(root, &graph)?;

    // Toggling back off removes exactly the framework facts again.
    write(
        root,
        "composer.json",
        r#"{"name":"acme/app","autoload":{"psr-4":{"Acme\\":"src/"}}}"#,
    )?;
    let next_index = reconciled.next_index.ok_or("next index missing")?;
    let reconciled = reconcile(root, &next_index)?;
    assert_eq!(reconciled.metrics.framework_config_changes, 1);
    let graph = reconciled.graph.ok_or("laravel-off graph missing")?;
    assert_eq!(graph.symbol_count(), baseline_symbols);
    assert_eq!(graph.edge_count(), baseline_edges);
    assert_matches_full_rebuild(root, &graph)?;
    Ok(())
}

#[test]
fn live_one_file_edit_and_manifest_edit_stay_dependency_scoped() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let root = repository.path();
    write(
        root,
        "composer.json",
        r#"{"name":"acme/app","autoload":{"psr-4":{"Acme\\":"src/"}}}"#,
    )?;
    write(
        root,
        "src/Service.php",
        "<?php namespace Acme; class Service {}\n",
    )?;
    write(
        root,
        "lib/Helper.php",
        "<?php namespace Acme\\Lib; class Helper {}\n",
    )?;
    let report = index_repository(root)?;
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
    let live = start_live_index(
        report.repository_root.clone(),
        report.syntax_index,
        engine.clone(),
    )?;
    let root = report.repository_root.clone();

    // Drain the mandatory startup reconciliation.
    let _ = engine.symbol_search(SymbolSearchRequest {
        query: "Service".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    let baseline = live.metrics();

    write(
        root.as_path(),
        "src/Service.php",
        "<?php namespace Acme; class Service { public function run(): void {} }\n",
    )?;
    let found = engine.symbol_search(SymbolSearchRequest {
        query: "run".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(found.data.candidates.len(), 1);
    let after_edit = live.metrics();
    assert_eq!(
        after_edit.files_reparsed - baseline.files_reparsed,
        1,
        "an ordinary edit reparses exactly its file"
    );
    assert_eq!(
        after_edit.full_reconciliations, baseline.full_reconciliations,
        "an ordinary edit never forces a full reconciliation"
    );
    assert_eq!(
        after_edit.metadata_files_recomputed,
        baseline.metadata_files_recomputed
    );

    write(
        root.as_path(),
        "composer.json",
        r#"{"name":"acme/app","autoload":{"psr-4":{"Acme\\":"src/","Acme\\Lib\\":"lib/"}}}"#,
    )?;
    let snapshot_query = engine.symbol_search(SymbolSearchRequest {
        query: "Helper".to_owned(),
        ..SymbolSearchRequest::default()
    })?;
    assert_eq!(snapshot_query.data.candidates.len(), 1);
    let after_manifest = live.metrics();
    assert_eq!(
        after_manifest.files_reparsed, after_edit.files_reparsed,
        "an autoload edit reparses nothing"
    );
    assert_eq!(
        after_manifest.metadata_files_recomputed - after_edit.metadata_files_recomputed,
        1,
        "an autoload edit re-materializes exactly the newly claimed file"
    );
    assert_eq!(
        after_manifest.full_reconciliations, after_edit.full_reconciliations,
        "a manifest edit stays a targeted reconciliation"
    );
    assert!(
        after_manifest.dependency_impact.impacted_units > 0,
        "the typed unit impact is recorded"
    );
    assert!(
        after_manifest
            .dependency_impact
            .unit_changes
            .source_roots_changed
            > 0,
        "the autoload change reason is recorded"
    );
    // The published revision is atomic: a fresh query observes the updated
    // project model and the updated file metadata together.
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.freshness(), Freshness::Fresh);
    let helper = RepoRelativePath::new("lib/Helper.php")?;
    assert_eq!(
        snapshot
            .graph()
            .file_metadata(&helper)
            .and_then(|metadata| metadata.package.as_ref())
            .map(|package| package.name.as_str()),
        Some("acme/app")
    );
    let metrics = live.shutdown_with_metrics()?;
    assert_eq!(metrics.reconciliation_failures, 0);
    Ok(())
}
