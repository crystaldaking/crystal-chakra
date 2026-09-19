//! Explicit real-provider smoke test. It is ignored by default so the normal
//! suite never depends on a developer-global bash-language-server installation.

use std::error::Error;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::Provenance;
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{
    CallHierarchyDirections, PreciseProvider, PreciseQueryRequest, ProviderDocument,
    ProviderSymbol, ProviderWorkspace,
};
use chakra_provider_bash_language_server::{
    BashLanguageServerCommand, BashLanguageServerConfig, BashLanguageServerProvider,
};

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
                language: Language::Shell,
            }],
        ),
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                path,
                TextPosition::new(2, 1)?,
                TextPosition::new(2, 16)?,
            )?,
            language: Language::Shell,
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
#[ignore = "requires bash-language-server on PATH or CHAKRA_BASH_LANGUAGE_SERVER"]
fn current_bash_language_server_returns_precise_incoming_calls_across_revisions()
-> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;

    let path = RepoRelativePath::new("main.sh")?;
    let source: Arc<str> = Arc::from("#!/bin/bash\ntarget() { :; }\ncaller() { target; }\n");
    fs::write(repository.path().join(path.as_str()), source.as_ref())?;
    let initial = request(repository.path(), path.clone(), source, Revision(1))?;
    let command = std::env::var_os("CHAKRA_BASH_LANGUAGE_SERVER")
        .map_or_else(BashLanguageServerCommand::discover, |path| {
            Some(BashLanguageServerCommand::start(path))
        })
        .ok_or("bash-language-server not found")?;
    let provider = BashLanguageServerProvider::start(
        initial.workspace.clone(),
        BashLanguageServerConfig {
            command,
            startup_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(60),
            barrier_timeout: Duration::from_secs(20),
            query_wait_timeout: Duration::from_secs(150),
            ..BashLanguageServerConfig::default()
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

    assert_eq!(result.revision, Revision(1));
    assert!(
        result
            .incoming
            .iter()
            .all(|relation| relation.provenance == Provenance::BashLanguageServer)
    );

    let changed_source: Arc<str> =
        Arc::from("#!/bin/bash\ntarget() { :; }\ncaller() { target; }\ncallerTwo() { target; }\n");
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
    assert!(
        changed
            .incoming
            .iter()
            .all(|relation| relation.provenance == Provenance::BashLanguageServer)
    );
    eprintln!(
        "bash_language_server_enrichment: initial={initial_elapsed:?}, after_edit={changed_elapsed:?}, initial_incoming={}, changed_incoming={}",
        result.incoming.len(),
        changed.incoming.len(),
    );
    provider.shutdown()?;
    Ok(())
}
