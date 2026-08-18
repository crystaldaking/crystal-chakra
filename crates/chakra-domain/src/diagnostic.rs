//! Bounded, revision-scoped syntax diagnostics emitted by language adapters.

use serde::{Deserialize, Serialize};

use crate::location::SourceRange;
use crate::provenance::{Precision, Provenance};
use crate::symbol::Language;

/// Maximum number of actionable Tree-sitter nodes retained for one file.
pub const MAX_SYNTAX_DIAGNOSTICS_PER_FILE: usize = 64;

/// Tree-sitter recovery node represented by one diagnostic.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SyntaxDiagnosticKind {
    Error,
    Missing,
}

/// Maintained grammar limitation confirmed against valid source syntax.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum KnownSyntaxGrammarGap {
    /// `tree-sitter-php` 0.24.2 rejects a typed class constant whose name is
    /// the otherwise permitted keyword `DEFAULT`.
    PhpTypedClassConstantNamedDefault,
    /// `tree-sitter-rust` 0.24.2 rejects a valid trait object whose lifetime
    /// bound precedes its trait bounds, for example `dyn 'static + Send`.
    RustLifetimeFirstTraitObject,
    /// `tree-sitter-rust` 0.24.2 rejects attributes on fields of a struct
    /// pattern even though rustc accepts them.
    RustAttributeOnPatternField,
}

/// Whether a recovery node is generic parse recovery or a known grammar
/// coverage limitation. `ParseRecovery` deliberately does not claim that the
/// source is invalid: an unknown valid construct can still reach this state.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "category", content = "gap", rename_all = "snake_case")]
pub enum SyntaxDiagnosticCause {
    ParseRecovery,
    KnownGrammarGap(KnownSyntaxGrammarGap),
}

/// Actionable syntax fact captured from the same parse as the graph revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SyntaxDiagnostic {
    pub language: Language,
    pub range: SourceRange,
    pub kind: SyntaxDiagnosticKind,
    pub provenance: Provenance,
    pub precision: Precision,
    pub cause: SyntaxDiagnosticCause,
    /// Tree-sitter grammar symbol for the `ERROR` node or expected missing
    /// node, useful when distinguishing broken source from grammar coverage.
    pub node_kind: String,
}

/// Why the status response omits one or more diagnostic nodes.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticTruncationCause {
    PerFileLimit,
    StatusLimit,
    ResponseByteLimit,
}
