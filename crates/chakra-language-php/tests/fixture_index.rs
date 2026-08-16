use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{EdgeKind, Language, SymbolKind};
use chakra_language_php::index_repository;
use tempfile::TempDir;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures/php/controller-service-provider")
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&destination)?;
            copy_tree(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

#[test]
fn realistic_php_fixture_exposes_bounded_syntax_intelligence() -> Result<(), Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    copy_tree(&fixture_root(), repository.path())?;

    let report = index_repository(repository.path())?;
    eprintln!(
        "php_fixture_index: files={}, symbols={}, edges={}, elapsed={:?}",
        report.metrics.parsed_files,
        report.metrics.symbols,
        report.metrics.edges,
        report.metrics.elapsed
    );
    assert_eq!(report.metrics.parsed_files, 4);
    assert_eq!(report.metrics.syntax_error_files, 0);
    assert!(report.graph.symbols().iter().all(|symbol| {
        symbol.key.language == Language::Php
            && symbol.provenance == Provenance::TreeSitter
            && symbol.precision == Precision::Syntax
    }));
    assert!(report.graph.symbols().iter().any(|symbol| {
        symbol.key.qualified_name == "ChakraFixture::Service::PaymentService"
            && symbol.key.kind == SymbolKind::Class
    }));
    assert!(report.graph.symbols().iter().any(|symbol| {
        symbol.key.qualified_name
            == "ChakraFixture::Tests::PaymentServiceTest::testRefundDelegatesToProvider"
            && symbol.key.kind == SymbolKind::Test
    }));

    let refund = report
        .graph
        .resolve_name("ChakraFixture::Service::PaymentService::refund");
    assert_eq!(refund.len(), 1);
    let callers: Vec<_> = report
        .graph
        .incoming_edges(refund[0])
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Calls)
        .collect();
    assert!(!callers.is_empty());
    assert!(callers.iter().all(|edge| {
        edge.provenance == Provenance::TreeSitter && edge.precision == Precision::Heuristic
    }));
    Ok(())
}
