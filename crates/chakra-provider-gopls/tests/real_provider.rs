//! Explicit real-provider smoke test. It is ignored by default so the normal
//! suite never depends on a developer-global gopls installation.

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
use chakra_provider_gopls::{GoplsCommand, GoplsConfig, GoplsProvider};

fn request(
    repository_root: &std::path::Path,
    path: RepoRelativePath,
    source: Arc<str>,
    revision: Revision,
) -> Result<PreciseQueryRequest, Box<dyn Error>> {
    Ok(PreciseQueryRequest {
        workspace: ProviderWorkspace::from_documents(
            fs::canonicalize(repository_root)?,
            revision,
            vec![ProviderDocument {
                path: path.clone(),
                source,
                language: Language::Go,
            }],
        ),
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                path,
                TextPosition::new(3, 1)?,
                TextPosition::new(3, 17)?,
            )?,
            language: Language::Go,
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
#[ignore = "requires gopls on PATH or CHAKRA_GOPLS"]
fn current_gopls_returns_precise_incoming_calls_across_revisions() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    fs::write(
        repository.path().join("go.mod"),
        "module example.com/sample\n\ngo 1.25\n",
    )?;
    let path = RepoRelativePath::new("service.go")?;
    let source: Arc<str> =
        Arc::from("package sample\n\nfunc target() {}\nfunc caller() { target() }\n");
    fs::write(repository.path().join(path.as_str()), source.as_ref())?;
    let initial = request(repository.path(), path.clone(), source, Revision(1))?;
    let command = std::env::var_os("CHAKRA_GOPLS")
        .map_or_else(GoplsCommand::discover, |path| {
            Some(GoplsCommand::stdio(path))
        })
        .ok_or("gopls not found")?;
    let provider = GoplsProvider::start(
        initial.workspace.clone(),
        GoplsConfig {
            command,
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(15),
            barrier_timeout: Duration::from_secs(5),
            query_wait_timeout: Duration::from_secs(30),
            ..GoplsConfig::default()
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

    let changed_source: Arc<str> = Arc::from(
        "package sample\n\nfunc target() {}\nfunc caller() { target() }\nfunc callerTwo() { target() }\n",
    );
    fs::write(
        repository.path().join(path.as_str()),
        changed_source.as_ref(),
    )?;
    let changed_started = Instant::now();
    let changed = provider.enrich(request(
        repository.path(),
        path,
        changed_source,
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
            .any(|relation| relation.name == "callerTwo"),
        "incoming after edit: {:?}",
        changed.incoming
    );
    eprintln!(
        "gopls_enrichment: initial={initial_elapsed:?}, after_edit={changed_elapsed:?}, initial_incoming={}, changed_incoming={}",
        result.incoming.len(),
        changed.incoming.len(),
    );
    provider.shutdown()?;
    Ok(())
}
