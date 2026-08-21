//! Explicit real-provider smoke test. It is ignored by default so the normal
//! suite never depends on a developer-global clangd installation.

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
use chakra_provider_clangd::{ClangdConfig, ClangdProvider};

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
                language: Language::Cpp,
            }],
        ),
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                path,
                TextPosition::new(1, 1)?,
                TextPosition::new(1, 17)?,
            )?,
            language: Language::Cpp,
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
#[ignore = "requires clangd on PATH"]
fn current_clangd_returns_precise_incoming_calls_across_revisions() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    fs::create_dir(repository.path().join("src"))?;
    fs::write(repository.path().join("compile_flags.txt"), "-std=c++20\n")?;
    let path = RepoRelativePath::new("src/index.cpp")?;
    let source: Arc<str> = Arc::from("void target() {}\nvoid caller() { target(); }\n");
    fs::write(repository.path().join(path.as_str()), source.as_ref())?;
    let initial = request(repository.path(), path.clone(), source, Revision(1))?;
    let provider = ClangdProvider::start(
        initial.workspace.clone(),
        ClangdConfig {
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(15),
            barrier_timeout: Duration::from_secs(5),
            query_wait_timeout: Duration::from_secs(30),
            ..ClangdConfig::default()
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
        "void target() {}\nvoid caller() { target(); }\nvoid caller_two() { target(); }\n",
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
            .any(|relation| relation.name == "caller_two"),
        "incoming after edit: {:?}",
        changed.incoming
    );
    eprintln!(
        "clangd_enrichment: initial={initial_elapsed:?}, after_edit={changed_elapsed:?}, initial_incoming={}, changed_incoming={}",
        result.incoming.len(),
        changed.incoming.len(),
    );
    provider.shutdown()?;
    Ok(())
}
