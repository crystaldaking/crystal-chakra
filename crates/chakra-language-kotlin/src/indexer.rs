//! Deterministic Kotlin syntax indexing on the shared language-neutral
//! driver (ADR-0056). This module keeps only the Kotlin seams: the
//! Tree-sitter parser hook, Git-aware discovery, and worker naming.

use std::path::Path;
use std::sync::Arc;

use chakra_domain::location::RepoRelativePath;
use chakra_git::ClassifiedSource;
use chakra_language_index::{LanguageHooks, LanguageParser};

use crate::discovery::discover_kotlin_sources;
use crate::parser::KotlinParser;

pub use chakra_language_index::{
    IndexMetrics, LanguageBuildMetrics, ReconcileMetrics, SyntaxFactCounts,
};

/// Failure to discover, read, parse, or validate the Kotlin syntax index.
pub type KotlinIndexError = chakra_language_index::LanguageIndexError;
/// Latest Kotlin source text plus role/package metadata from the same scan.
pub type KotlinSources = chakra_language_index::LanguageSources;
/// Reusable per-file Kotlin syntax facts and per-owner relationship
/// contributions.
pub type KotlinSyntaxIndex = chakra_language_index::LanguageSyntaxIndex<KotlinHooks>;
/// Complete private initial Kotlin index, ready for atomic publication.
pub type IndexReport = chakra_language_index::IndexReport<KotlinHooks>;
/// Reconcile outcome for the Kotlin syntax index.
pub type ReconcileReport = chakra_language_index::ReconcileReport<KotlinHooks>;

/// Kotlin seams of the shared indexing driver.
#[derive(Debug, Clone, Copy, Default)]
pub struct KotlinHooks;

impl LanguageHooks for KotlinHooks {
    type Parser = KotlinParser;

    const WORKER_NAME: &'static str = "kotlin";

    fn language() -> chakra_domain::symbol::Language {
        chakra_domain::symbol::Language::Kotlin
    }

    fn new_parser() -> Result<Self::Parser, KotlinIndexError> {
        KotlinParser::new().map_err(|error| KotlinIndexError::Parse(error.to_string()))
    }

    fn discover_sources(root: &Path) -> Result<Vec<ClassifiedSource>, KotlinIndexError> {
        Ok(discover_kotlin_sources(root)?)
    }
}

impl LanguageParser for KotlinParser {
    fn parse(
        &mut self,
        path: RepoRelativePath,
        source: Arc<str>,
    ) -> Result<chakra_language_index::ParsedFile, KotlinIndexError> {
        KotlinParser::parse(self, path, source)
            .map_err(|error| KotlinIndexError::Parse(error.to_string()))
    }
}

/// Builds a complete Kotlin syntax index from the actual materialized Git
/// worktree. The caller owns atomic publication into `WorkspaceEngine`.
pub fn index_repository(root: &Path) -> Result<IndexReport, KotlinIndexError> {
    chakra_language_index::index_repository::<KotlinHooks>(root)
}

/// Reads the latest Git-aware Kotlin file inventory and exact contents.
pub fn scan_repository_sources(repository_root: &Path) -> Result<KotlinSources, KotlinIndexError> {
    chakra_language_index::scan_repository_sources::<KotlinHooks>(repository_root)
}
