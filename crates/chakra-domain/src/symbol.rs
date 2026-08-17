//! Symbol and relation model (SPEC §8, §10).
//!
//! Symbols are typed through [`SymbolKind`] rather than one domain type per
//! language construct. Identity is split per SPEC §10: [`EntityId`] is
//! strict identity within one graph revision; [`SymbolKey`] is a
//! language-aware lookup key. `SymbolFingerprint` and lineage are
//! deliberately not defined yet — they arrive with cross-revision mapping
//! in a later phase.

use serde::{Deserialize, Serialize};

use crate::location::SourceRange;
use crate::provenance::{Precision, Provenance};

/// Maximum number of Unicode scalar values retained for a syntactic receiver
/// hint. Receiver expressions are evidence, not declarations, so keeping an
/// arbitrarily large expression would add memory without improving identity.
pub const MAX_RECEIVER_HINT_CHARS: usize = 128;

/// Programming language of a symbol indexed by Chakra v0.1.
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
pub enum Language {
    Rust,
    Php,
}

/// Kind of a code entity (SPEC §8 plus the impl-block/import facts v0.1 §7
/// requires).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Module,
    Function,
    Method,
    Struct,
    Class,
    Enum,
    Trait,
    Interface,
    Constant,
    Field,
    Property,
    /// An `impl` block entity (container for methods).
    ImplBlock,
    /// A `use` declaration.
    Import,
    /// A function identified as a test.
    Test,
}

/// Strict identity within one specific graph revision (SPEC §10).
///
/// Never stable across revisions; use [`SymbolKey`] for lookup and expect
/// re-resolution after any update.
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
pub struct EntityId(pub u64);

/// Language-aware lookup key (SPEC §10). Not globally stable across
/// arbitrary refactors; scoped to a revision for exact resolution.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SymbolKey {
    pub language: Language,
    /// Qualified name, e.g. `api::controller::PaymentController::refund`.
    pub qualified_name: String,
    /// Containing entity name where relevant (e.g. the type of a method).
    pub container: Option<String>,
    pub kind: SymbolKind,
    /// Declaring file, repository-relative. Must equal the symbol's
    /// `location.file`; the engine enforces this at construction.
    pub path: crate::location::RepoRelativePath,
}

/// A code entity within one revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Symbol {
    pub id: EntityId,
    pub key: SymbolKey,
    pub location: SourceRange,
    /// Source-level signature line(s), when the indexer extracted them.
    pub signature: Option<String>,
    pub provenance: Provenance,
    pub precision: Precision,
}

impl Symbol {
    /// Simple (last-segment) name, e.g. `refund`.
    pub fn name(&self) -> &str {
        self.key
            .qualified_name
            .rsplit("::")
            .next()
            .unwrap_or(&self.key.qualified_name)
    }
}

/// Typed relation between symbols (SPEC §8). Deliberately excludes vague
/// relations such as `RELATED_TO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeKind {
    Contains,
    Defines,
    References,
    Calls,
    Imports,
    Implements,
    Extends,
    Tests,
    DependsOn,
    ModifiedBy,
}

/// A directed edge between two symbols of one revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Edge {
    pub kind: EdgeKind,
    pub from: EntityId,
    pub to: EntityId,
    pub provenance: Provenance,
    pub precision: Precision,
    /// Source range of the relation itself when known (e.g. the call-site
    /// range for `Calls`).
    pub location: Option<SourceRange>,
}

/// Syntactic shape of a source call expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallForm {
    Function,
    Member,
    NullsafeMember,
    Scoped,
}

/// Declaration domain considered for a syntax call.
///
/// Keeping these domains separate prevents a same-name free function, method,
/// or test declaration from becoming a candidate solely because its text
/// matches the call token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallTargetKind {
    Function,
    Method,
    Test,
}

/// Revision-local resolution state of one compact syntax call site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallResolution {
    Resolved { target: EntityId },
    Ambiguous { candidates: u64 },
    Unresolved,
}

/// A call expression retained separately from materialized graph edges.
///
/// Ambiguous and unresolved calls remain queryable evidence without creating
/// one graph edge per same-name declaration. Only `Resolved` call sites have
/// a corresponding syntax `CALLS` edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallSite {
    pub caller: EntityId,
    pub form: CallForm,
    pub target_kind: CallTargetKind,
    pub name: String,
    /// Normalized namespace/type path when syntax provides a usable target
    /// qualifier.
    pub qualifier: Option<String>,
    /// Bounded syntactic receiver token when available, even when Chakra
    /// cannot infer its type.
    pub receiver_hint: Option<String>,
    pub location: SourceRange,
    pub resolution: CallResolution,
    pub provenance: Provenance,
    pub precision: Precision,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::{RepoRelativePath, TextPosition};

    fn sample_symbol(qualified_name: &str) -> Result<Symbol, Box<dyn std::error::Error>> {
        let file = RepoRelativePath::new("src/lib.rs")?;
        let position = TextPosition::new(1, 1)?;
        let location = SourceRange::new(file.clone(), position, position)?;
        Ok(Symbol {
            id: EntityId(0),
            key: SymbolKey {
                language: Language::Rust,
                qualified_name: qualified_name.to_owned(),
                container: None,
                kind: SymbolKind::Function,
                path: file,
            },
            location,
            signature: None,
            provenance: Provenance::TreeSitter,
            precision: Precision::Syntax,
        })
    }

    #[test]
    fn name_is_the_last_qualified_segment() -> Result<(), Box<dyn std::error::Error>> {
        let symbol = sample_symbol("api::controller::PaymentController::refund")?;
        assert_eq!(symbol.name(), "refund");
        let flat = sample_symbol("refund")?;
        assert_eq!(flat.name(), "refund");
        Ok(())
    }
}
