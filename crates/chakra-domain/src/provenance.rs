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
    /// Confirmed by a precise provider such as rust-analyzer, or by a
    /// Chakra-owned resolver whose evidence rule is precise-tier (ADR-0030).
    Precise,
}

/// Where a fact came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// rust-analyzer (live precise provider).
    RustAnalyzer,
    /// vtsls (live precise provider for TypeScript and JavaScript,
    /// ADR-0027/0032).
    Vtsls,
    /// pyright (live precise provider for Python, ADR-0027).
    Pyright,
    /// jdtls (live precise provider for Java, ADR-0027/0036).
    Jdtls,
    /// csharp-ls (live precise provider for C#, ADR-0027/0037).
    CsharpLs,
    /// bash-language-server (live reference provider for Shell, ADR-0027).
    BashLanguageServer,
    /// clangd (live precise provider for C/C++, ADR-0027).
    Clangd,
    /// terraform-ls (live precise reference provider for HCL/Terraform,
    /// ADR-0027).
    TerraformLs,
    /// gopls (live precise provider for Go, ADR-0027).
    Gopls,
    /// Chakra-owned static resolver producing precise-tier facts from an
    /// explicit, deterministic evidence rule (ADR-0030).
    ChakraResolver,
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
        assert_eq!(
            serde_json::to_string(&Provenance::ChakraResolver)?,
            "\"chakra_resolver\""
        );
        assert_eq!(
            serde_json::from_str::<Provenance>("\"chakra_resolver\"")?,
            Provenance::ChakraResolver
        );
        assert_eq!(serde_json::to_string(&Provenance::Vtsls)?, "\"vtsls\"");
        assert_eq!(
            serde_json::from_str::<Provenance>("\"vtsls\"")?,
            Provenance::Vtsls
        );
        assert_eq!(serde_json::to_string(&Provenance::Pyright)?, "\"pyright\"");
        assert_eq!(
            serde_json::from_str::<Provenance>("\"pyright\"")?,
            Provenance::Pyright
        );
        assert_eq!(serde_json::to_string(&Provenance::Jdtls)?, "\"jdtls\"");
        assert_eq!(
            serde_json::from_str::<Provenance>("\"jdtls\"")?,
            Provenance::Jdtls
        );
        assert_eq!(
            serde_json::to_string(&Provenance::CsharpLs)?,
            "\"csharp_ls\""
        );
        assert_eq!(
            serde_json::from_str::<Provenance>("\"csharp_ls\"")?,
            Provenance::CsharpLs
        );
        assert_eq!(
            serde_json::to_string(&Provenance::BashLanguageServer)?,
            "\"bash_language_server\""
        );
        assert_eq!(serde_json::to_string(&Provenance::Clangd)?, "\"clangd\"");
        assert_eq!(
            serde_json::to_string(&Provenance::TerraformLs)?,
            "\"terraform_ls\""
        );
        assert_eq!(serde_json::to_string(&Provenance::Gopls)?, "\"gopls\"");
        Ok(())
    }
}
