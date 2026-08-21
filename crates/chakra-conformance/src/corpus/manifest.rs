//! Typed views of `docs/support/corpus/manifest.json` and
//! `docs/support/corpus/budgets.json`.
//!
//! The manifest is the selection authority: every evaluated checkout must
//! match its pinned SHA. Budgets are per-language starting points sized from
//! a real local run; tightening them requires review (see
//! `docs/support/corpus/README.md`).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::Check;

/// Top-level corpus manifest (`docs/support/corpus/manifest.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct CorpusManifest {
    pub schema_version: u32,
    pub selected_at: String,
    #[serde(default)]
    pub languages: BTreeMap<String, CorpusLanguage>,
}

/// All repositories selected for one language.
#[derive(Debug, Clone, Deserialize)]
pub struct CorpusLanguage {
    #[serde(default)]
    pub repositories: Vec<CorpusRepository>,
}

/// One pinned public repository.
#[derive(Debug, Clone, Deserialize)]
pub struct CorpusRepository {
    pub name: String,
    pub url: String,
    pub branch: String,
    pub sha: String,
    pub license: String,
    pub size_kb: u64,
    #[serde(default)]
    pub rationale: String,
}

impl CorpusRepository {
    /// Cache directory and result-file slug (`owner/repo` -> `owner__repo`).
    pub fn slug(&self) -> String {
        self.name.replace('/', "__")
    }
}

impl CorpusManifest {
    /// Loads and parses the manifest from `path`.
    pub fn load(path: &Path) -> Check<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Sorted language names declared by the manifest.
    pub fn language_names(&self) -> Vec<String> {
        self.languages.keys().cloned().collect()
    }
}

/// Per-language evaluation budgets (`docs/support/corpus/budgets.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct CorpusBudgets {
    pub schema_version: u32,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub languages: BTreeMap<String, LanguageBudgets>,
}

/// Budgets for one language. Values are generous starting points measured on
/// the maintainer machine that produced the committed artifacts, not SLOs.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct LanguageBudgets {
    /// Wall-time budget for the `cold-index` scenario.
    pub cold_index_wall_micros: u64,
    /// Peak-RSS budget for the `cold-index` scenario (phase-boundary samples).
    pub cold_index_peak_rss_bytes: u64,
    /// Wall-time budget for the `warm-noop` scenario barrier.
    pub warm_noop_wall_micros: u64,
}

impl CorpusBudgets {
    /// Loads and parses the budgets file from `path`.
    pub fn load(path: &Path) -> Check<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Budgets for `language`, when configured.
    pub fn for_language(&self, language: &str) -> Option<LanguageBudgets> {
        self.languages.get(language).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_committed_manifest() -> Check<()> {
        let manifest = CorpusManifest::load(&crate::corpus::default_manifest_path())?;
        assert!(manifest.schema_version == 1);
        let rust = manifest
            .languages
            .get("rust")
            .ok_or("manifest is missing rust")?;
        assert!(
            rust.repositories
                .iter()
                .any(|repo| repo.name == "tokio-rs/tokio")
        );
        let tokio = &rust.repositories[0];
        assert_eq!(tokio.slug(), "tokio-rs__tokio");
        assert_eq!(tokio.sha.len(), 40);
        Ok(())
    }

    #[test]
    fn loads_the_committed_budgets() -> Check<()> {
        let budgets = CorpusBudgets::load(&crate::corpus::default_budgets_path())?;
        assert!(budgets.for_language("rust").is_some());
        assert!(budgets.for_language("php").is_some());
        Ok(())
    }
}
