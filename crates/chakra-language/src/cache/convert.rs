//! Conversions between the cache's language-neutral fact schema and the
//! parser draft types of each language adapter family (issue #39).
//!
//! Export projects one adapter's per-file drafts into [`FileSyntaxFacts`];
//! import rebuilds the drafts for a content-validated restore. Imports are
//! defensive: a decoded fact whose indices do not line up with its own
//! symbol list is rejected (`None`), and the caller reparses that file —
//! corruption never compromises the restore.

use std::sync::Arc;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::symbol::{Language, SymbolKey};

use super::facts::{
    CallFact, FileSyntaxFacts, ImplFact, NamedRelationFact, SymbolFact, TypeRelationFact,
    TypeRelationKindFact,
};

fn import_symbol_key(language: Language, path: &RepoRelativePath, fact: &SymbolFact) -> SymbolKey {
    SymbolKey {
        language,
        qualified_name: fact.qualified_name.clone(),
        container: fact.container.clone(),
        kind: fact.kind,
        path: path.clone(),
    }
}

/// Per-file fact indices must reference symbols of the same file; anything
/// else means the payload does not describe this source.
fn indices_valid(facts: &FileSyntaxFacts) -> bool {
    let symbols = facts.symbols.len();
    facts
        .symbols
        .iter()
        .all(|symbol| symbol.parent.is_none_or(|parent| parent < symbols))
        && facts.calls.iter().all(|call| call.caller < symbols)
        && facts
            .named_relations
            .iter()
            .all(|relation| relation.from < symbols)
        && facts
            .type_relations
            .iter()
            .all(|relation| relation.from < symbols)
        && facts
            .implementations
            .iter()
            .all(|implementation| implementation.symbol < symbols)
}

// ---------------------------------------------------------------------------
// Shared driver languages (TypeScript, Python, JavaScript, Java, Shell,
// C++, HCL, Go)
// ---------------------------------------------------------------------------

pub fn export_shared(
    files: &std::collections::BTreeMap<RepoRelativePath, Arc<chakra_language_index::ParsedFile>>,
) -> Vec<FileSyntaxFacts> {
    files
        .iter()
        .map(|(path, file)| FileSyntaxFacts {
            path: path.clone(),
            byte_len: file.source.len() as u64,
            module_path: file.module_path.clone(),
            extension_scopes: Vec::new(),
            symbols: file
                .symbols
                .iter()
                .map(|symbol| SymbolFact {
                    qualified_name: symbol.key.qualified_name.clone(),
                    container: symbol.key.container.clone(),
                    kind: symbol.key.kind,
                    location: symbol.location.clone(),
                    signature: symbol.signature.clone(),
                    parent: symbol.parent,
                    is_extension_method: false,
                })
                .collect(),
            calls: file
                .calls
                .iter()
                .map(|call| CallFact {
                    caller: call.caller,
                    form: call.form,
                    target_kind: call.target_kind,
                    name: call.name.clone(),
                    qualifier: call.qualifier.clone(),
                    receiver_type: None,
                    receiver_type_source: None,
                    receiver_hint: call.receiver_hint.clone(),
                    promoted: call.promoted,
                    location: call.location.clone(),
                })
                .collect(),
            named_relations: file
                .named_relations
                .iter()
                .map(|relation| NamedRelationFact {
                    from: relation.from,
                    candidates: relation.candidates.clone(),
                    target_kinds: relation.target_kinds.clone(),
                    kind: relation.kind,
                })
                .collect(),
            type_relations: Vec::new(),
            implementations: Vec::new(),
            has_errors: file.has_errors,
            diagnostics: file.diagnostics.clone(),
            diagnostic_count: file.diagnostic_count,
        })
        .collect()
}

pub fn import_shared(
    facts: &FileSyntaxFacts,
    source: Arc<str>,
    language: Language,
) -> Option<chakra_language_index::ParsedFile> {
    if !indices_valid(facts) {
        return None;
    }
    Some(chakra_language_index::ParsedFile {
        source,
        module_path: facts.module_path.clone(),
        symbols: facts
            .symbols
            .iter()
            .map(|symbol| chakra_language_index::SymbolDraft {
                key: import_symbol_key(language, &facts.path, symbol),
                location: symbol.location.clone(),
                signature: symbol.signature.clone(),
                parent: symbol.parent,
            })
            .collect(),
        calls: facts
            .calls
            .iter()
            .map(|call| chakra_language_index::CallDraft {
                caller: call.caller,
                form: call.form,
                target_kind: call.target_kind,
                name: call.name.clone(),
                qualifier: call.qualifier.clone(),
                receiver_hint: call.receiver_hint.clone(),
                promoted: call.promoted,
                location: call.location.clone(),
            })
            .collect(),
        named_relations: facts
            .named_relations
            .iter()
            .map(|relation| chakra_language_index::NamedRelationDraft {
                from: relation.from,
                candidates: relation.candidates.clone(),
                target_kinds: relation.target_kinds.clone(),
                kind: relation.kind,
            })
            .collect(),
        has_errors: facts.has_errors,
        diagnostics: facts.diagnostics.clone(),
        diagnostic_count: facts.diagnostic_count,
    })
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

pub fn export_rust(
    files: &std::collections::BTreeMap<RepoRelativePath, Arc<chakra_language_rust::ParsedFile>>,
) -> Vec<FileSyntaxFacts> {
    files
        .iter()
        .map(|(path, file)| FileSyntaxFacts {
            path: path.clone(),
            byte_len: file.source.len() as u64,
            module_path: file.module_path.clone(),
            extension_scopes: Vec::new(),
            symbols: file
                .symbols
                .iter()
                .map(|symbol| SymbolFact {
                    qualified_name: symbol.key.qualified_name.clone(),
                    container: symbol.key.container.clone(),
                    kind: symbol.key.kind,
                    location: symbol.location.clone(),
                    signature: symbol.signature.clone(),
                    parent: symbol.parent,
                    is_extension_method: false,
                })
                .collect(),
            calls: file
                .calls
                .iter()
                .map(|call| CallFact {
                    caller: call.caller,
                    form: call.form,
                    target_kind: call.target_kind,
                    name: call.name.clone(),
                    qualifier: call.qualifier.clone(),
                    receiver_type: None,
                    receiver_type_source: None,
                    receiver_hint: call.receiver_hint.clone(),
                    promoted: false,
                    location: call.location.clone(),
                })
                .collect(),
            named_relations: Vec::new(),
            type_relations: Vec::new(),
            implementations: file
                .implementations
                .iter()
                .map(|implementation| ImplFact {
                    symbol: implementation.symbol,
                    module_path: implementation.module_path.clone(),
                    target_lookup: implementation.target_lookup.clone(),
                    trait_lookup: implementation.trait_lookup.clone(),
                })
                .collect(),
            has_errors: file.has_errors,
            diagnostics: file.diagnostics.clone(),
            diagnostic_count: file.diagnostic_count,
        })
        .collect()
}

pub fn import_rust(
    facts: &FileSyntaxFacts,
    source: Arc<str>,
) -> Option<chakra_language_rust::ParsedFile> {
    if !indices_valid(facts) {
        return None;
    }
    Some(chakra_language_rust::ParsedFile {
        source,
        module_path: facts.module_path.clone(),
        symbols: facts
            .symbols
            .iter()
            .map(|symbol| chakra_language_rust::SymbolDraft {
                key: import_symbol_key(Language::Rust, &facts.path, symbol),
                location: symbol.location.clone(),
                signature: symbol.signature.clone(),
                parent: symbol.parent,
            })
            .collect(),
        calls: facts
            .calls
            .iter()
            .map(|call| chakra_language_rust::CallDraft {
                caller: call.caller,
                form: call.form,
                target_kind: call.target_kind,
                name: call.name.clone(),
                qualifier: call.qualifier.clone(),
                receiver_hint: call.receiver_hint.clone(),
                location: call.location.clone(),
            })
            .collect(),
        implementations: facts
            .implementations
            .iter()
            .map(|implementation| chakra_language_rust::ImplDraft {
                symbol: implementation.symbol,
                module_path: implementation.module_path.clone(),
                target_lookup: implementation.target_lookup.clone(),
                trait_lookup: implementation.trait_lookup.clone(),
            })
            .collect(),
        has_errors: facts.has_errors,
        diagnostics: facts.diagnostics.clone(),
        diagnostic_count: facts.diagnostic_count,
    })
}

// ---------------------------------------------------------------------------
// PHP
// ---------------------------------------------------------------------------

pub fn export_php(
    files: &std::collections::BTreeMap<RepoRelativePath, Arc<chakra_language_php::ParsedFile>>,
) -> Vec<FileSyntaxFacts> {
    files
        .iter()
        .map(|(path, file)| FileSyntaxFacts {
            path: path.clone(),
            byte_len: file.source.len() as u64,
            module_path: Vec::new(),
            extension_scopes: Vec::new(),
            symbols: file
                .symbols
                .iter()
                .map(|symbol| SymbolFact {
                    qualified_name: symbol.key.qualified_name.clone(),
                    container: symbol.key.container.clone(),
                    kind: symbol.key.kind,
                    location: symbol.location.clone(),
                    signature: symbol.signature.clone(),
                    parent: symbol.parent,
                    is_extension_method: false,
                })
                .collect(),
            calls: file
                .calls
                .iter()
                .map(|call| CallFact {
                    caller: call.caller,
                    form: call.form,
                    target_kind: call.target_kind,
                    name: call.name.clone(),
                    qualifier: call.qualifier.clone(),
                    receiver_type: call.receiver_type.clone(),
                    receiver_type_source: call.receiver_type_source,
                    receiver_hint: call.receiver_hint.clone(),
                    promoted: false,
                    location: call.location.clone(),
                })
                .collect(),
            named_relations: file
                .named_relations
                .iter()
                .map(|relation| NamedRelationFact {
                    from: relation.from,
                    candidates: vec![relation.target.clone()],
                    target_kinds: relation.target_kinds.clone(),
                    kind: relation.kind,
                })
                .collect(),
            type_relations: file
                .type_relations
                .iter()
                .map(|relation| TypeRelationFact {
                    from: relation.from,
                    target: relation.target.clone(),
                    kind: match relation.kind {
                        chakra_language_php::TypeRelationKind::Trait => TypeRelationKindFact::Trait,
                        chakra_language_php::TypeRelationKind::Extends => {
                            TypeRelationKindFact::Extends
                        }
                        chakra_language_php::TypeRelationKind::Implements => {
                            TypeRelationKindFact::Implements
                        }
                    },
                })
                .collect(),
            implementations: Vec::new(),
            has_errors: file.has_errors,
            diagnostics: file.diagnostics.clone(),
            diagnostic_count: file.diagnostic_count,
        })
        .collect()
}

pub fn import_php(
    facts: &FileSyntaxFacts,
    source: Arc<str>,
) -> Option<chakra_language_php::ParsedFile> {
    if !indices_valid(facts) {
        return None;
    }
    let mut named_relations = Vec::with_capacity(facts.named_relations.len());
    for relation in &facts.named_relations {
        // PHP relations carry exactly one target; anything else is not a
        // PHP payload.
        if relation.candidates.len() != 1 {
            return None;
        }
        named_relations.push(chakra_language_php::NamedRelationDraft {
            from: relation.from,
            target: relation.candidates[0].clone(),
            target_kinds: relation.target_kinds.clone(),
            kind: relation.kind,
        });
    }
    Some(chakra_language_php::ParsedFile {
        source,
        symbols: facts
            .symbols
            .iter()
            .map(|symbol| chakra_language_php::SymbolDraft {
                key: import_symbol_key(Language::Php, &facts.path, symbol),
                location: symbol.location.clone(),
                signature: symbol.signature.clone(),
                parent: symbol.parent,
            })
            .collect(),
        calls: facts
            .calls
            .iter()
            .map(|call| chakra_language_php::CallDraft {
                caller: call.caller,
                form: call.form,
                target_kind: call.target_kind,
                name: call.name.clone(),
                qualifier: call.qualifier.clone(),
                receiver_type: call.receiver_type.clone(),
                receiver_type_source: call.receiver_type_source,
                receiver_hint: call.receiver_hint.clone(),
                location: call.location.clone(),
            })
            .collect(),
        named_relations,
        type_relations: facts
            .type_relations
            .iter()
            .map(|relation| chakra_language_php::TypeRelationDraft {
                from: relation.from,
                target: relation.target.clone(),
                kind: match relation.kind {
                    TypeRelationKindFact::Trait => chakra_language_php::TypeRelationKind::Trait,
                    TypeRelationKindFact::Extends => chakra_language_php::TypeRelationKind::Extends,
                    TypeRelationKindFact::Implements => {
                        chakra_language_php::TypeRelationKind::Implements
                    }
                },
            })
            .collect(),
        has_errors: facts.has_errors,
        diagnostics: facts.diagnostics.clone(),
        diagnostic_count: facts.diagnostic_count,
    })
}

// ---------------------------------------------------------------------------
// C#
// ---------------------------------------------------------------------------

pub fn export_csharp(
    files: &std::collections::BTreeMap<RepoRelativePath, Arc<chakra_language_csharp::ParsedFile>>,
) -> Vec<FileSyntaxFacts> {
    files
        .iter()
        .map(|(path, file)| FileSyntaxFacts {
            path: path.clone(),
            byte_len: file.source.len() as u64,
            module_path: file.module_path.clone(),
            extension_scopes: file.extension_scopes.clone(),
            symbols: file
                .symbols
                .iter()
                .map(|symbol| SymbolFact {
                    qualified_name: symbol.key.qualified_name.clone(),
                    container: symbol.key.container.clone(),
                    kind: symbol.key.kind,
                    location: symbol.location.clone(),
                    signature: symbol.signature.clone(),
                    parent: symbol.parent,
                    is_extension_method: symbol.is_extension_method,
                })
                .collect(),
            calls: file
                .calls
                .iter()
                .map(|call| CallFact {
                    caller: call.caller,
                    form: call.form,
                    target_kind: call.target_kind,
                    name: call.name.clone(),
                    qualifier: call.qualifier.clone(),
                    receiver_type: None,
                    receiver_type_source: None,
                    receiver_hint: call.receiver_hint.clone(),
                    promoted: false,
                    location: call.location.clone(),
                })
                .collect(),
            named_relations: file
                .named_relations
                .iter()
                .map(|relation| NamedRelationFact {
                    from: relation.from,
                    candidates: relation.candidates.clone(),
                    target_kinds: relation.target_kinds.clone(),
                    kind: relation.kind,
                })
                .collect(),
            type_relations: Vec::new(),
            implementations: Vec::new(),
            has_errors: file.has_errors,
            diagnostics: file.diagnostics.clone(),
            diagnostic_count: file.diagnostic_count,
        })
        .collect()
}

pub fn import_csharp(
    facts: &FileSyntaxFacts,
    source: Arc<str>,
) -> Option<chakra_language_csharp::ParsedFile> {
    if !indices_valid(facts) {
        return None;
    }
    Some(chakra_language_csharp::ParsedFile {
        source,
        module_path: facts.module_path.clone(),
        extension_scopes: facts.extension_scopes.clone(),
        symbols: facts
            .symbols
            .iter()
            .map(|symbol| chakra_language_csharp::SymbolDraft {
                key: import_symbol_key(Language::CSharp, &facts.path, symbol),
                location: symbol.location.clone(),
                signature: symbol.signature.clone(),
                parent: symbol.parent,
                is_extension_method: symbol.is_extension_method,
            })
            .collect(),
        calls: facts
            .calls
            .iter()
            .map(|call| chakra_language_csharp::CallDraft {
                caller: call.caller,
                form: call.form,
                target_kind: call.target_kind,
                name: call.name.clone(),
                qualifier: call.qualifier.clone(),
                receiver_hint: call.receiver_hint.clone(),
                location: call.location.clone(),
            })
            .collect(),
        named_relations: facts
            .named_relations
            .iter()
            .map(|relation| chakra_language_csharp::NamedRelationDraft {
                from: relation.from,
                candidates: relation.candidates.clone(),
                target_kinds: relation.target_kinds.clone(),
                kind: relation.kind,
            })
            .collect(),
        has_errors: facts.has_errors,
        diagnostics: facts.diagnostics.clone(),
        diagnostic_count: facts.diagnostic_count,
    })
}
