//! Typed language hooks for the shared indexing driver (issue #94). Hooks
//! carry exactly the language-specific seams: the parser, Git-aware
//! discovery, worker naming, and an optional post-parse evidence pass.
//! Scheduling, limits, metrics, relationship materialization, and graph
//! publication stay language-neutral in the driver.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::symbol::Language;
use chakra_git::ClassifiedSource;

use crate::error::LanguageIndexError;
use crate::facts::ParsedFile;

/// A Tree-sitter parser adapter owned by one worker thread.
pub trait LanguageParser: Send {
    fn parse(
        &mut self,
        path: RepoRelativePath,
        source: Arc<str>,
    ) -> Result<ParsedFile, LanguageIndexError>;
}

/// Language-specific seams of one syntax indexing adapter.
pub trait LanguageHooks: Send + Sync + 'static {
    type Parser: LanguageParser;

    /// Short name used for worker threads, spans, and diagnostics
    /// (for example `"go"`).
    const WORKER_NAME: &'static str;

    /// Language whose work this adapter measures; phase metrics are
    /// attributed to it.
    fn language() -> Language;

    fn new_parser() -> Result<Self::Parser, LanguageIndexError>;

    /// Git-aware discovery of this language's classified sources.
    fn discover_sources(root: &Path) -> Result<Vec<ClassifiedSource>, LanguageIndexError>;

    /// Optional evidence-driven pass over freshly parsed or reconciled
    /// drafts (C++ qualified-callable reclassification and promoted-call
    /// resolution, issues #83/#84). Runs in memory over retained drafts;
    /// implementations must not re-read sources.
    fn post_parse(_files: &mut BTreeMap<RepoRelativePath, Arc<ParsedFile>>) {}
}
