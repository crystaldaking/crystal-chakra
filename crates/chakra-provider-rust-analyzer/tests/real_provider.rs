//! Explicit real-provider smoke test. It is ignored by default so the normal
//! suite never depends on a developer-global rust-analyzer installation.

use std::error::Error;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_engine::{
    CallHierarchyDirections, PreciseProvider, PreciseQueryRequest, ProviderDocument,
    ProviderSymbol, ProviderWorkspace,
};
use chakra_provider_rust_analyzer::{RustAnalyzerConfig, RustAnalyzerProvider};

#[test]
#[ignore = "requires rust-analyzer on PATH"]
fn current_rust_analyzer_returns_precise_incoming_call() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    fs::create_dir(repository.path().join("src"))?;
    fs::write(
        repository.path().join("Cargo.toml"),
        "[package]\nname = \"ra-smoke\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    let source: Arc<str> = Arc::from("pub fn target() {}\npub fn caller() {\n    target();\n}\n");
    fs::write(repository.path().join("src/lib.rs"), source.as_ref())?;
    let repository_root = fs::canonicalize(repository.path())?;
    let path = RepoRelativePath::new("src/lib.rs")?;
    let workspace = ProviderWorkspace::from_documents(
        repository_root,
        Revision(1),
        vec![ProviderDocument {
            path: path.clone(),
            source,
            language: chakra_domain::symbol::Language::Rust,
        }],
    );
    let provider = RustAnalyzerProvider::start(
        workspace.clone(),
        RustAnalyzerConfig {
            barrier_timeout: Duration::from_secs(5),
            query_wait_timeout: Duration::from_secs(15),
            ..RustAnalyzerConfig::default()
        },
    )?;
    let initial_started = Instant::now();
    let result = provider.enrich(PreciseQueryRequest {
        workspace,
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                path.clone(),
                TextPosition::new(1, 1)?,
                TextPosition::new(1, 19)?,
            )?,
            language: chakra_domain::symbol::Language::Rust,
        },
        directions: CallHierarchyDirections {
            incoming: true,
            outgoing: false,
        },
        limit: 20,
        priority: chakra_engine::ProviderRequestPriority::Normal,
    });
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
        "pub fn target() {}\npub fn caller() {\n    target();\n}\npub fn caller_two() {\n    target();\n}\n",
    );
    fs::write(
        repository.path().join("src/lib.rs"),
        changed_source.as_ref(),
    )?;
    let changed_started = Instant::now();
    let changed = provider.enrich(PreciseQueryRequest {
        workspace: ProviderWorkspace::from_documents(
            fs::canonicalize(repository.path())?,
            Revision(2),
            vec![ProviderDocument {
                path: path.clone(),
                source: changed_source,
                language: chakra_domain::symbol::Language::Rust,
            }],
        ),
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                path,
                TextPosition::new(1, 1)?,
                TextPosition::new(1, 19)?,
            )?,
            language: chakra_domain::symbol::Language::Rust,
        },
        directions: CallHierarchyDirections {
            incoming: true,
            outgoing: false,
        },
        limit: 20,
        priority: chakra_engine::ProviderRequestPriority::Normal,
    });
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
        "rust_analyzer_enrichment: initial={initial_elapsed:?}, after_edit={changed_elapsed:?}, initial_incoming={}, changed_incoming={}",
        result.incoming.len(),
        changed.incoming.len(),
    );
    provider.shutdown()?;
    Ok(())
}
