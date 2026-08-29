//! Versioned per-file syntax fact schema (issue #39).
//!
//! The schema mirrors the parser draft types of the language adapters: it
//! captures exactly the materialization-independent facts of one source file
//! — declarations, call candidates, relation drafts, and diagnostics — so a
//! restore can rebuild the adapter index without re-parsing. Entity ids are
//! deliberately absent: they are revision-scoped and assigned only while a
//! complete immutable graph is materialized. Source text is absent too: the
//! restore path re-reads it from the worktree during content validation.
//!
//! Precise live-provider facts (rust-analyzer and friends) are never stored
//! here; every fact in this schema is re-derivable from the source bytes
//! alone. Provenance and precision are absent because materialization assigns
//! them deterministically (Tree-sitter/syntax, plus the Chakra-owned resolver
//! tiers recomputed from the same facts).

use chakra_domain::diagnostic::SyntaxDiagnostic;
use chakra_domain::location::{RepoRelativePath, SourceRange};
use chakra_domain::symbol::{CallForm, CallTargetKind, EdgeKind, ReceiverTypeSource, SymbolKind};

/// One declared symbol, in per-file declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolFact {
    pub qualified_name: String,
    pub container: Option<String>,
    pub kind: SymbolKind,
    /// Range inside [`FileSyntaxFacts::path`]; the file is implied and never
    /// stored per range.
    pub location: SourceRange,
    pub signature: Option<String>,
    pub parent: Option<usize>,
    /// C# extension-method marker (`false` for every other language).
    pub is_extension_method: bool,
}

/// One syntactic call candidate attributed to a caller symbol index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallFact {
    pub caller: usize,
    pub form: CallForm,
    pub target_kind: CallTargetKind,
    pub name: String,
    pub qualifier: Option<String>,
    pub receiver_type: Option<String>,
    pub receiver_type_source: Option<ReceiverTypeSource>,
    pub receiver_hint: Option<String>,
    /// C++ method-tier promotion marker (`false` for every other language).
    pub promoted: bool,
    pub location: SourceRange,
}

/// One named relation draft with ordered resolution candidates. PHP's
/// single-target relations are stored as one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedRelationFact {
    pub from: usize,
    pub candidates: Vec<String>,
    pub target_kinds: Vec<SymbolKind>,
    pub kind: EdgeKind,
}

/// PHP typed relation kind (trait use, extends, implements).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeRelationKindFact {
    Trait,
    Extends,
    Implements,
}

/// One PHP typed relation draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRelationFact {
    pub from: usize,
    pub target: String,
    pub kind: TypeRelationKindFact,
}

/// One Rust impl-block draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplFact {
    pub symbol: usize,
    pub module_path: Vec<String>,
    pub target_lookup: Option<String>,
    pub trait_lookup: Option<String>,
}

/// All materialization-independent syntax facts of one indexed source file.
/// Fields that a language never produces stay empty; the per-language
/// extractor version in the compatibility key guards interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSyntaxFacts {
    pub path: RepoRelativePath,
    pub byte_len: u64,
    pub module_path: Vec<String>,
    /// C# extension scopes (`empty` for every other language).
    pub extension_scopes: Vec<String>,
    pub symbols: Vec<SymbolFact>,
    pub calls: Vec<CallFact>,
    pub named_relations: Vec<NamedRelationFact>,
    pub type_relations: Vec<TypeRelationFact>,
    pub implementations: Vec<ImplFact>,
    pub has_errors: bool,
    pub diagnostics: Vec<SyntaxDiagnostic>,
    pub diagnostic_count: u64,
}
