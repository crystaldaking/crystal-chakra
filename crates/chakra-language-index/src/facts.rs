//! Per-file syntax fact types shared by language adapters (issue #94).
//! Parsers produce these drafts; the indexing driver consumes them. Entity
//! ids are intentionally absent: they are revision-scoped and assigned only
//! while a complete immutable graph is materialized.

use std::sync::Arc;

use chakra_domain::diagnostic::SyntaxDiagnostic;
use chakra_domain::location::SourceRange;
use chakra_domain::symbol::{CallForm, CallTargetKind, EdgeKind, SymbolKey, SymbolKind};
use serde::{Deserialize, Serialize};

/// One extracted declaration before revision-local entity ids exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolDraft {
    pub key: SymbolKey,
    pub location: SourceRange,
    pub signature: Option<String>,
    pub parent: Option<usize>,
}

/// One extracted call expression before revision-local entity ids exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallDraft {
    pub caller: usize,
    pub form: CallForm,
    pub target_kind: CallTargetKind,
    pub name: String,
    pub qualifier: Option<String>,
    pub receiver_hint: Option<String>,
    /// The parser promoted this function-form call to the method tier only
    /// because its caller looked like a method (C++, issue #83). Languages
    /// without that promotion always store `false`; the flag makes the
    /// indexer's evidence-driven re-evaluation reversible across reconciles.
    pub promoted: bool,
    pub location: SourceRange,
}

/// One extracted named relation (for example a base-type or trait
/// relationship) with bounded candidate spellings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedRelationDraft {
    pub from: usize,
    pub candidates: Vec<String>,
    pub target_kinds: Vec<SymbolKind>,
    pub kind: EdgeKind,
}

/// All syntax facts extracted from one source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedFile {
    pub source: Arc<str>,
    pub module_path: Vec<String>,
    pub symbols: Vec<SymbolDraft>,
    pub calls: Vec<CallDraft>,
    pub named_relations: Vec<NamedRelationDraft>,
    pub has_errors: bool,
    pub diagnostics: Vec<SyntaxDiagnostic>,
    pub diagnostic_count: u64,
}
