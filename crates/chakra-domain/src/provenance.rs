//! Provenance and precision of facts (SPEC §7).
//!
//! Every fact whose reliability depends on its source carries both. A text
//! match must never be labeled `precise`; a syntax call candidate must never
//! be labeled a precise rust-analyzer call.

use serde::{Deserialize, Serialize};

/// How much trust a fact deserves, ordered from least to most precise.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Precision {
    /// Pure text match.
    Textual,
    /// Deterministic heuristic.
    Heuristic,
    /// Derived from syntax (Tree-sitter); no type checking.
    Syntax,
    /// Confirmed by a precise provider such as rust-analyzer.
    Precise,
}

/// Where a fact came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// rust-analyzer (live precise provider).
    RustAnalyzer,
    /// Tree-sitter syntax analysis.
    TreeSitter,
    /// Git metadata or diff state.
    Git,
    /// Plain text search.
    TextSearch,
    /// Deterministic heuristic.
    Heuristic,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precision_is_ordered_from_textual_to_precise() {
        assert!(Precision::Textual < Precision::Heuristic);
        assert!(Precision::Heuristic < Precision::Syntax);
        assert!(Precision::Syntax < Precision::Precise);
    }

    #[test]
    fn serde_names_match_spec_examples() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(serde_json::to_string(&Precision::Syntax)?, "\"syntax\"");
        assert_eq!(
            serde_json::to_string(&Provenance::RustAnalyzer)?,
            "\"rust_analyzer\""
        );
        Ok(())
    }
}
