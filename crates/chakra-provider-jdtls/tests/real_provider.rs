//! Explicit real-provider smoke test. It is ignored by default so the normal
//! suite never depends on a developer-global jdtls installation.

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
use chakra_provider_jdtls::{JdtlsCommand, JdtlsConfig, JdtlsProvider};

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
                language: Language::Java,
            }],
        ),
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                path,
                TextPosition::new(3, 5)?,
                TextPosition::new(3, 35)?,
            )?,
            language: Language::Java,
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
#[ignore = "requires jdtls on PATH or CHAKRA_JDTLS"]
fn current_jdtls_returns_precise_incoming_calls_across_revisions() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    fs::write(
        repository.path().join("pom.xml"),
        "<project xmlns=\"http://maven.apache.org/POM/4.0.0\"><modelVersion>4.0.0</modelVersion><groupId>sample</groupId><artifactId>sample</artifactId><version>1</version><properties><maven.compiler.release>21</maven.compiler.release></properties></project>\n",
    )?;
    fs::create_dir_all(repository.path().join("src/main/java/sample"))?;
    let path = RepoRelativePath::new("src/main/java/sample/Main.java")?;
    let source: Arc<str> = Arc::from(
        "package sample;\npublic class Main {\n    public static void target() {}\n    public static void caller() { target(); }\n}\n",
    );
    fs::write(repository.path().join(path.as_str()), source.as_ref())?;
    let initial = request(repository.path(), path.clone(), source, Revision(1))?;
    let data = tempfile::tempdir()?;
    let command = std::env::var_os("CHAKRA_JDTLS")
        .map_or_else(
            || JdtlsCommand::discover(data.path()),
            |path| Some(JdtlsCommand::stdio(path, data.path().as_os_str())),
        )
        .ok_or("jdtls not found")?;
    let provider = JdtlsProvider::start(
        initial.workspace.clone(),
        JdtlsConfig {
            command,
            startup_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(60),
            barrier_timeout: Duration::from_secs(20),
            query_wait_timeout: Duration::from_secs(150),
            ..JdtlsConfig::default()
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
            .any(|relation| relation.name.split('(').next() == Some("caller")),
        "incoming: {:?}",
        result.incoming
    );

    assert_eq!(result.revision, Revision(1));
    assert!(
        result
            .incoming
            .iter()
            .all(|relation| relation.provenance == Provenance::Jdtls)
    );

    let changed_source: Arc<str> = Arc::from(
        "package sample;\npublic class Main {\n    public static void target() {}\n    public static void caller() { target(); }\n    public static void callerTwo() { target(); }\n}\n",
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
            .any(|relation| relation.name.split('(').next() == Some("callerTwo")),
        "incoming after edit: {:?}",
        changed.incoming
    );
    assert!(
        changed
            .incoming
            .iter()
            .all(|relation| relation.provenance == Provenance::Jdtls)
    );
    eprintln!(
        "jdtls_enrichment: initial={initial_elapsed:?}, after_edit={changed_elapsed:?}, initial_incoming={}, changed_incoming={}",
        result.incoming.len(),
        changed.incoming.len(),
    );
    provider.shutdown()?;
    Ok(())
}
