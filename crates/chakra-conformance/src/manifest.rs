//! Data-driven per-language scenario manifest (`manifest.json`).

use std::path::Path;

use chakra_domain::symbol::SymbolKind;
use serde::Deserialize;

use crate::{Check, ensure, failure};

/// One declared scenario: stable id, human description, and the capability
/// ids from `docs/language-parity-contract.md` it evidences.
#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioSpec {
    pub id: String,
    pub description: String,
    pub capability_ids: Vec<String>,
}

/// A symbol the fixture must declare with an exact qualified name and kind.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedSymbol {
    pub qualified_name: String,
    pub kind: SymbolKind,
}

/// Per-language values the shared scenarios assert against.
#[derive(Debug, Clone, Deserialize)]
pub struct Expectations {
    /// A production-role source file.
    pub production_file: String,
    /// A test-role source file.
    pub test_file: String,
    /// File the syntax-error scenario breaks and repairs at runtime.
    pub breakable_file: String,
    /// Simple name declared by exactly `ambiguous_qualified.len()` symbols.
    pub ambiguous_name: String,
    /// Qualified names of the colliding declarations.
    pub ambiguous_qualified: Vec<String>,
    /// Namespace prefix that must contain `nested_symbol`.
    pub nested_prefix: String,
    /// Container symbols (modules/classes) that must exist.
    pub nested_containers: Vec<ExpectedSymbol>,
    /// Symbol nested inside every `nested_containers` entry.
    pub nested_symbol: ExpectedSymbol,
    /// Import alias recorded as an import fact.
    pub import_alias: String,
    /// Qualified name of the unique caller of `callee`.
    pub caller: String,
    /// Simple name of `caller` (used by the provider double).
    pub caller_simple: String,
    /// Qualified name of a uniquely named callee.
    pub callee: String,
    /// Qualified name of a symbol classified as a test.
    pub test_symbol: String,
    /// Text needle present in exactly `text_needle_file`.
    pub text_needle: String,
    pub text_needle_file: String,
    /// Qualified name of the high-degree callee.
    pub fan_in_target: String,
    /// Exact number of distinct call sites of `fan_in_target`.
    pub fan_in_callers: usize,
    /// File modified in the second commit of the diff scenario.
    pub diff_second_commit_file: String,
    /// File left modified-but-uncommitted in the diff scenario.
    pub diff_worktree_file: String,
    /// File created/modified/renamed/deleted by the lifecycle scenario.
    pub lifecycle_file: String,
    pub lifecycle_renamed_file: String,
    /// Unique simple-name prefix for lifecycle symbols (`{prefix}_one`, ...).
    pub lifecycle_symbol_prefix: String,
    /// Declaration template parts for a whole new file:
    /// content is `{prefix}{name}{suffix}`.
    pub lifecycle_decl_prefix: String,
    pub lifecycle_decl_suffix: String,
    /// Declaration template parts appended inside an existing file.
    pub snippet_prefix: String,
    pub snippet_suffix: String,
    /// Source written to `breakable_file` by the syntax-error scenario.
    pub broken_content: String,
    /// Symbol that must survive in `breakable_file` while it is broken.
    pub retained_symbol: String,
}

impl Expectations {
    /// Renders a one-function source file for the lifecycle scenario.
    pub fn declaration(&self, simple_name: &str) -> String {
        format!(
            "{}{}{}",
            self.lifecycle_decl_prefix, simple_name, self.lifecycle_decl_suffix
        )
    }

    /// Renders a declaration appended to an existing file (diff scenario).
    pub fn snippet(&self, simple_name: &str) -> String {
        format!(
            "\n{}{}{}",
            self.snippet_prefix, simple_name, self.snippet_suffix
        )
    }
}

/// The parsed `manifest.json` of one fixture language.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub language: String,
    pub scenarios: Vec<ScenarioSpec>,
    pub expectations: Expectations,
}

impl Manifest {
    /// Loads and structurally validates the manifest stored in `directory`.
    pub fn load(directory: &Path) -> Check<Self> {
        let path = directory.join("manifest.json");
        let raw = std::fs::read_to_string(&path)
            .map_err(|error| failure(format!("cannot read {}: {error}", path.display())))?;
        let manifest: Self = serde_json::from_str(&raw)
            .map_err(|error| failure(format!("invalid {}: {error}", path.display())))?;
        ensure(!manifest.language.is_empty(), "manifest language is empty")?;
        ensure(
            !manifest.scenarios.is_empty(),
            "manifest declares no scenarios",
        )?;
        let mut ids: Vec<&str> = manifest
            .scenarios
            .iter()
            .map(|scenario| scenario.id.as_str())
            .collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        ensure(ids.len() == before, "manifest has duplicate scenario ids")?;
        for scenario in &manifest.scenarios {
            ensure(
                !scenario.id.is_empty() && !scenario.description.is_empty(),
                "scenario entries need id and description",
            )?;
            ensure(
                !scenario.capability_ids.is_empty(),
                format!("scenario {} declares no capability ids", scenario.id),
            )?;
        }
        ensure(
            manifest.expectations.ambiguous_qualified.len() >= 2,
            "ambiguity expectations need at least two candidates",
        )?;
        Ok(manifest)
    }

    /// Looks up a declared scenario by id.
    pub fn scenario(&self, id: &str) -> Option<&ScenarioSpec> {
        self.scenarios.iter().find(|scenario| scenario.id == id)
    }
}
