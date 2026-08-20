//! Explicit real-provider smoke test. It is ignored by default so the normal
//! suite never depends on a developer-global terraform-ls installation.

use std::error::Error;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{
    CallHierarchyDirections, PreciseProvider, PreciseQueryRequest, ProviderDocument,
    ProviderSymbol, ProviderWorkspace,
};
use chakra_provider_terraform_ls::{TerraformLsCommand, TerraformLsConfig, TerraformLsProvider};

fn request(
    repository_root: &std::path::Path,
    target_source: Arc<str>,
    caller_source: Arc<str>,
    revision: Revision,
) -> Result<PreciseQueryRequest, Box<dyn Error>> {
    let target_path = RepoRelativePath::new("target.tf")?;
    Ok(PreciseQueryRequest {
        workspace: ProviderWorkspace::from_documents(
            fs::canonicalize(repository_root)?,
            revision,
            vec![
                ProviderDocument {
                    path: target_path.clone(),
                    source: target_source,
                    language: Language::Hcl,
                },
                ProviderDocument {
                    path: RepoRelativePath::new("caller.tf")?,
                    source: caller_source,
                    language: Language::Hcl,
                },
            ],
        ),
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                target_path,
                TextPosition::new(1, 1)?,
                TextPosition::new(4, 2)?,
            )?,
            language: Language::Hcl,
        },
        directions: CallHierarchyDirections {
            incoming: true,
            outgoing: false,
        },
        limit: 20,
        priority: chakra_engine::ProviderRequestPriority::Normal,
    })
}

#[test]
#[ignore = "requires terraform-ls on PATH or CHAKRA_TERRAFORM_LS"]
fn current_terraform_ls_returns_precise_references_across_revisions() -> Result<(), Box<dyn Error>>
{
    let repository = tempfile::tempdir()?;
    let target_source: Arc<str> =
        Arc::from("variable \"target\" {\n  type    = string\n  default = \"first\"\n}\n");
    let caller_source: Arc<str> = Arc::from("output \"caller\" {\n  value = var.target\n}\n");
    fs::write(repository.path().join("target.tf"), target_source.as_ref())?;
    fs::write(repository.path().join("caller.tf"), caller_source.as_ref())?;
    let initial = request(
        repository.path(),
        target_source.clone(),
        caller_source,
        Revision(1),
    )?;
    let command = std::env::var_os("CHAKRA_TERRAFORM_LS")
        .map_or_else(TerraformLsCommand::discover, |path| {
            Some(TerraformLsCommand::start(path))
        })
        .ok_or("terraform-ls not found")?;
    let provider = TerraformLsProvider::start(
        initial.workspace.clone(),
        TerraformLsConfig {
            command,
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(20),
            barrier_timeout: Duration::from_secs(5),
            query_wait_timeout: Duration::from_secs(30),
            ..TerraformLsConfig::default()
        },
    )?;

    let initial_started = Instant::now();
    let result = provider.enrich(initial);
    let initial_elapsed = initial_started.elapsed();
    assert_eq!(
        result.state,
        ProviderState::Ready,
        "provider error: {:?}",
        provider.last_error()
    );
    assert!(
        result
            .incoming
            .iter()
            .any(|relation| relation.name == "caller"),
        "incoming: {:?}",
        result.incoming
    );
    let metrics = provider.metrics().ok_or("provider metrics unavailable")?;
    assert_eq!(metrics.document_sync.opened_documents, 2);
    assert_eq!(metrics.document_sync.total_text_documents_sent, 2);

    let changed_caller_source: Arc<str> = Arc::from(
        "output \"caller\" {\n  value = var.target\n}\n\noutput \"caller_two\" {\n  value = var.target\n}\n",
    );
    fs::write(
        repository.path().join("caller.tf"),
        changed_caller_source.as_ref(),
    )?;
    let changed_started = Instant::now();
    let changed = provider.enrich(request(
        repository.path(),
        target_source,
        changed_caller_source,
        Revision(2),
    )?);
    let changed_elapsed = changed_started.elapsed();
    assert_eq!(
        changed.state,
        ProviderState::Ready,
        "provider error after edit: {:?}",
        provider.last_error()
    );
    assert_eq!(changed.revision, Revision(2));
    assert!(
        changed
            .incoming
            .iter()
            .any(|relation| relation.name == "caller_two"),
        "incoming after edit: {:?}",
        changed.incoming
    );
    eprintln!(
        "terraform_ls_enrichment: initial={initial_elapsed:?}, after_edit={changed_elapsed:?}, initial_incoming={}, changed_incoming={}",
        result.incoming.len(),
        changed.incoming.len(),
    );
    provider.shutdown()?;
    Ok(())
}
