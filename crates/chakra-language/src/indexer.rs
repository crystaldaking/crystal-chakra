//! Composition of independently parsed language indexes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::location::RepoRelativePath;
use chakra_engine::{GraphError, SymbolGraph};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSources {
    pub rust: BTreeMap<RepoRelativePath, Arc<str>>,
    pub php: BTreeMap<RepoRelativePath, Arc<str>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileMetrics {
    pub scanned_files: u64,
    pub unchanged_files: u64,
    pub reparsed_files: u64,
    pub created_files: u64,
    pub modified_files: u64,
    pub deleted_files: u64,
    pub relationship_files_recomputed: u64,
    pub syntax_error_files: u64,
    pub truncated_call_sites: u64,
}

#[derive(Debug)]
pub struct ReconcileReport {
    pub graph: Option<SymbolGraph>,
    pub metrics: ReconcileMetrics,
    pub next_index: Option<WorkspaceSyntaxIndex>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexMetrics {
    pub discovered_files: u64,
    pub parsed_files: u64,
    pub syntax_error_files: u64,
    pub truncated_call_sites: u64,
    pub symbols: u64,
    pub edges: u64,
    pub call_sites: u64,
    pub ambiguous_call_sites: u64,
    pub unresolved_call_sites: u64,
    pub rust_files: u64,
    pub php_files: u64,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub struct IndexReport {
    pub repository_root: PathBuf,
    pub graph: SymbolGraph,
    pub metrics: IndexMetrics,
    pub syntax_index: WorkspaceSyntaxIndex,
}

#[derive(Debug, Error)]
pub enum WorkspaceIndexError {
    #[error(transparent)]
    Rust(#[from] chakra_language_rust::RustIndexError),
    #[error(transparent)]
    Php(#[from] chakra_language_php::PhpIndexError),
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error("language adapters resolved different repository roots: {rust} and {php}")]
    RootMismatch { rust: String, php: String },
    #[error("workspace syntax index update failed: {0}")]
    Update(String),
}

#[derive(Debug, Clone)]
pub struct WorkspaceSyntaxIndex {
    rust: chakra_language_rust::RustSyntaxIndex,
    php: chakra_language_php::PhpSyntaxIndex,
    rust_graph: Arc<SymbolGraph>,
    php_graph: Arc<SymbolGraph>,
}

impl WorkspaceSyntaxIndex {
    pub fn paths(&self) -> Vec<RepoRelativePath> {
        let mut paths = self.rust.paths();
        paths.extend(self.php.paths());
        paths.sort();
        paths.dedup();
        paths
    }

    pub fn reconcile_sources(
        &self,
        sources: WorkspaceSources,
    ) -> Result<ReconcileReport, WorkspaceIndexError> {
        let rust = self.rust.reconcile_sources(sources.rust)?;
        let php = self.php.reconcile_sources(sources.php)?;
        let metrics = combine_reconcile_metrics(rust.metrics, php.metrics);
        if rust.graph.is_none() && php.graph.is_none() {
            return Ok(ReconcileReport {
                graph: None,
                metrics,
                next_index: None,
            });
        }

        let rust_graph = rust
            .graph
            .map(Arc::new)
            .unwrap_or_else(|| self.rust_graph.clone());
        let php_graph = php
            .graph
            .map(Arc::new)
            .unwrap_or_else(|| self.php_graph.clone());
        let next = Self {
            rust: rust.next_index.unwrap_or_else(|| self.rust.clone()),
            php: php.next_index.unwrap_or_else(|| self.php.clone()),
            rust_graph,
            php_graph,
        };
        let graph = next.materialize_graph()?;
        Ok(ReconcileReport {
            graph: Some(graph),
            metrics,
            next_index: Some(next),
        })
    }

    fn materialize_graph(&self) -> Result<SymbolGraph, WorkspaceIndexError> {
        Ok(SymbolGraph::merge([
            self.rust_graph.as_ref().clone(),
            self.php_graph.as_ref().clone(),
        ])?)
    }
}

fn combine_reconcile_metrics(
    rust: chakra_language_rust::ReconcileMetrics,
    php: chakra_language_php::ReconcileMetrics,
) -> ReconcileMetrics {
    ReconcileMetrics {
        scanned_files: rust.scanned_files + php.scanned_files,
        unchanged_files: rust.unchanged_files + php.unchanged_files,
        reparsed_files: rust.reparsed_files + php.reparsed_files,
        created_files: rust.created_files + php.created_files,
        modified_files: rust.modified_files + php.modified_files,
        deleted_files: rust.deleted_files + php.deleted_files,
        relationship_files_recomputed: rust.relationship_files_recomputed
            + php.relationship_files_recomputed,
        syntax_error_files: rust.syntax_error_files + php.syntax_error_files,
        truncated_call_sites: rust.truncated_call_sites + php.truncated_call_sites,
    }
}

pub fn index_repository(root: &Path) -> Result<IndexReport, WorkspaceIndexError> {
    let started = Instant::now();
    let rust = chakra_language_rust::index_repository(root)?;
    let php = chakra_language_php::index_repository(root)?;
    if rust.repository_root != php.repository_root {
        return Err(WorkspaceIndexError::RootMismatch {
            rust: rust.repository_root.display().to_string(),
            php: php.repository_root.display().to_string(),
        });
    }
    let repository_root = rust.repository_root;
    let rust_metrics = rust.metrics;
    let php_metrics = php.metrics;
    let syntax_index = WorkspaceSyntaxIndex {
        rust: rust.syntax_index,
        php: php.syntax_index,
        rust_graph: Arc::new(rust.graph),
        php_graph: Arc::new(php.graph),
    };
    let graph = syntax_index.materialize_graph()?;
    let metrics = IndexMetrics {
        discovered_files: rust_metrics.discovered_files + php_metrics.discovered_files,
        parsed_files: rust_metrics.parsed_files + php_metrics.parsed_files,
        syntax_error_files: rust_metrics.syntax_error_files + php_metrics.syntax_error_files,
        truncated_call_sites: rust_metrics.truncated_call_sites + php_metrics.truncated_call_sites,
        symbols: graph.symbol_count(),
        edges: graph.edge_count(),
        call_sites: graph.call_site_count(),
        ambiguous_call_sites: graph.ambiguous_call_site_count(),
        unresolved_call_sites: graph.unresolved_call_site_count(),
        rust_files: rust_metrics.parsed_files,
        php_files: php_metrics.parsed_files,
        elapsed: started.elapsed(),
    };
    Ok(IndexReport {
        repository_root,
        graph,
        metrics,
        syntax_index,
    })
}

pub fn scan_repository_sources(
    repository_root: &Path,
) -> Result<WorkspaceSources, WorkspaceIndexError> {
    Ok(WorkspaceSources {
        rust: chakra_language_rust::scan_repository_sources(repository_root)?,
        php: chakra_language_php::scan_repository_sources(repository_root)?,
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::process::Command;

    use chakra_domain::symbol::{EdgeKind, Language};
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn combines_rust_and_php_without_cross_language_call_edges() -> Result<(), Box<dyn Error>> {
        let repository = TempDir::new()?;
        let status = Command::new("git")
            .current_dir(repository.path())
            .args(["init", "--quiet"])
            .status()?;
        if !status.success() {
            return Err("git init failed".into());
        }
        fs::write(
            repository.path().join("lib.rs"),
            "pub fn rust_caller() { shared(); }\npub fn shared() {}\n",
        )?;
        fs::write(
            repository.path().join("service.php"),
            "<?php function php_caller(): void { shared(); } function shared(): void {}\n",
        )?;

        let report = index_repository(repository.path())?;
        let matches = report.graph.resolve_name("shared");
        assert_eq!(matches.len(), 2);
        let mut languages: Vec<_> = matches
            .iter()
            .filter_map(|id| report.graph.symbol(*id).map(|symbol| symbol.key.language))
            .collect();
        languages.sort();
        assert_eq!(languages, [Language::Rust, Language::Php]);
        for symbol in report.graph.symbols() {
            for edge in report.graph.outgoing_edges(symbol.id) {
                if edge.kind != EdgeKind::Calls {
                    continue;
                }
                let target = report.graph.symbol(edge.to).ok_or("call target missing")?;
                assert_eq!(symbol.key.language, target.key.language);
            }
        }
        assert_eq!(report.metrics.rust_files, 1);
        assert_eq!(report.metrics.php_files, 1);
        Ok(())
    }
}
