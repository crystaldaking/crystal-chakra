//! Tree-sitter C/C++ extraction into language-neutral Chakra drafts.
//!
//! The syntax tier records translation-unit modules, namespaces, C++ types,
//! functions/methods, fields, aliases, includes, base-type relations, common
//! test macros, diagnostics, and bounded static call candidates. Semantic
//! overload resolution remains clangd's responsibility.

use std::num::TryFromIntError;
use std::sync::Arc;

use chakra_domain::diagnostic::{
    MAX_SYNTAX_DIAGNOSTICS_PER_FILE, SyntaxDiagnostic, SyntaxDiagnosticCause, SyntaxDiagnosticKind,
};
use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{
    CallForm, CallTargetKind, EdgeKind, Language, MAX_RECEIVER_HINT_CHARS, SymbolKey, SymbolKind,
};
use thiserror::Error;
use tree_sitter::{Node, Parser, Point};

const MAX_SIGNATURE_CHARS: usize = 512;

#[derive(Debug, Error)]
pub(crate) enum ParseError {
    #[error("failed to load the Tree-sitter C++ grammar: {0}")]
    Language(String),
    #[error("Tree-sitter returned no syntax tree for {0}")]
    NoTree(RepoRelativePath),
    #[error("source position in {path} exceeds Chakra's range: {source}")]
    PositionInteger {
        path: RepoRelativePath,
        #[source]
        source: TryFromIntError,
    },
    #[error("Tree-sitter returned an invalid point {row}:{column} for {path}")]
    InvalidPoint {
        path: RepoRelativePath,
        row: usize,
        column: usize,
    },
    #[error("failed to construct a source range for {path}: {message}")]
    Range {
        path: RepoRelativePath,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedFile {
    pub source: Arc<str>,
    pub module_path: Vec<String>,
    pub symbols: Vec<SymbolDraft>,
    pub calls: Vec<CallDraft>,
    pub named_relations: Vec<NamedRelationDraft>,
    pub has_errors: bool,
    pub diagnostics: Vec<SyntaxDiagnostic>,
    pub diagnostic_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymbolDraft {
    pub key: SymbolKey,
    pub location: SourceRange,
    pub signature: Option<String>,
    pub parent: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallDraft {
    pub caller: usize,
    pub form: CallForm,
    pub target_kind: CallTargetKind,
    pub name: String,
    pub qualifier: Option<String>,
    pub receiver_hint: Option<String>,
    pub location: SourceRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedRelationDraft {
    pub from: usize,
    pub candidates: Vec<String>,
    pub target_kinds: Vec<SymbolKind>,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone)]
struct Context {
    prefix: Vec<String>,
    container: Option<String>,
    parent: Option<usize>,
    callable: Option<usize>,
    in_type: bool,
}

#[derive(Debug)]
struct Extraction<'a> {
    path: RepoRelativePath,
    source: &'a str,
    line_starts: Vec<usize>,
    symbols: Vec<SymbolDraft>,
    calls: Vec<CallDraft>,
    named_relations: Vec<NamedRelationDraft>,
    import_ordinal: usize,
}

impl Extraction<'_> {
    fn text(&self, node: Node<'_>) -> Option<&str> {
        self.source.get(node.byte_range())
    }

    fn position(&self, point: Point) -> Result<TextPosition, ParseError> {
        let line_start =
            *self
                .line_starts
                .get(point.row)
                .ok_or_else(|| ParseError::InvalidPoint {
                    path: self.path.clone(),
                    row: point.row,
                    column: point.column,
                })?;
        let line_end = self.source[line_start..]
            .find('\n')
            .map_or(self.source.len(), |offset| line_start + offset);
        let line =
            self.source
                .get(line_start..line_end)
                .ok_or_else(|| ParseError::InvalidPoint {
                    path: self.path.clone(),
                    row: point.row,
                    column: point.column,
                })?;
        if point.column > line.len() || !line.is_char_boundary(point.column) {
            return Err(ParseError::InvalidPoint {
                path: self.path.clone(),
                row: point.row,
                column: point.column,
            });
        }
        let line_number =
            u32::try_from(point.row + 1).map_err(|source| ParseError::PositionInteger {
                path: self.path.clone(),
                source,
            })?;
        let column_number =
            u32::try_from(line[..point.column].chars().count() + 1).map_err(|source| {
                ParseError::PositionInteger {
                    path: self.path.clone(),
                    source,
                }
            })?;
        TextPosition::new(line_number, column_number).map_err(|error| ParseError::Range {
            path: self.path.clone(),
            message: error.to_string(),
        })
    }

    fn range(&self, node: Node<'_>) -> Result<SourceRange, ParseError> {
        SourceRange::new(
            self.path.clone(),
            self.position(node.start_position())?,
            self.position(node.end_position())?,
        )
        .map_err(|error| ParseError::Range {
            path: self.path.clone(),
            message: error.to_string(),
        })
    }

    fn diagnostics(&self, root: Node<'_>) -> Result<(Vec<SyntaxDiagnostic>, u64), ParseError> {
        if !root.has_error() {
            return Ok((Vec::new(), 0));
        }
        let mut diagnostics = Vec::new();
        let mut total = 0_u64;
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            let kind = if node.is_error() {
                Some(SyntaxDiagnosticKind::Error)
            } else if node.is_missing() {
                Some(SyntaxDiagnosticKind::Missing)
            } else {
                None
            };
            if let Some(kind) = kind {
                total = total.saturating_add(1);
                if diagnostics.len() < MAX_SYNTAX_DIAGNOSTICS_PER_FILE {
                    diagnostics.push(SyntaxDiagnostic {
                        language: Language::Cpp,
                        range: self.range(node)?,
                        kind,
                        provenance: Provenance::TreeSitter,
                        precision: Precision::Syntax,
                        cause: SyntaxDiagnosticCause::ParseRecovery,
                        node_kind: node.kind().to_owned(),
                    });
                }
            }
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    if total == 0 {
                        diagnostics.push(SyntaxDiagnostic {
                            language: Language::Cpp,
                            range: self.range(root)?,
                            kind: SyntaxDiagnosticKind::Error,
                            provenance: Provenance::TreeSitter,
                            precision: Precision::Syntax,
                            cause: SyntaxDiagnosticCause::ParseRecovery,
                            node_kind: "<unlocated-error>".to_owned(),
                        });
                        total = 1;
                    }
                    return Ok((diagnostics, total));
                }
            }
        }
    }

    fn qualified(prefix: &[String], name: &str) -> String {
        if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{}::{name}", prefix.join("::"))
        }
    }

    fn signature(&self, node: Node<'_>) -> Option<String> {
        let end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |body| body.start_byte());
        let raw = self.source.get(node.start_byte()..end)?.trim();
        if raw.is_empty() {
            return None;
        }
        let mut signature = String::with_capacity(raw.len().min(MAX_SIGNATURE_CHARS));
        let mut chars = 0_usize;
        let mut truncated = false;
        'words: for word in raw.split_whitespace() {
            if !signature.is_empty() {
                if chars == MAX_SIGNATURE_CHARS {
                    truncated = true;
                    break;
                }
                signature.push(' ');
                chars += 1;
            }
            for character in word.chars() {
                if chars == MAX_SIGNATURE_CHARS {
                    truncated = true;
                    break 'words;
                }
                signature.push(character);
                chars += 1;
            }
        }
        if truncated {
            if let Some((last, _)) = signature.char_indices().next_back() {
                signature.truncate(last);
            }
            signature.push('…');
        }
        Some(signature)
    }

    fn add_symbol(
        &mut self,
        context: &Context,
        name: &str,
        kind: SymbolKind,
        node: Node<'_>,
        signature: Option<String>,
    ) -> Result<usize, ParseError> {
        let index = self.symbols.len();
        self.symbols.push(SymbolDraft {
            key: SymbolKey {
                language: Language::Cpp,
                qualified_name: Self::qualified(&context.prefix, name),
                container: context.container.clone(),
                kind,
                path: self.path.clone(),
            },
            location: self.range(node)?,
            signature,
            parent: context.parent,
        });
        Ok(index)
    }

    fn walk(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        match node.kind() {
            "namespace_definition" => self.namespace(node, context),
            "class_specifier" => self.type_declaration(node, context, SymbolKind::Class),
            "struct_specifier" | "union_specifier" => {
                self.type_declaration(node, context, SymbolKind::Struct)
            }
            "enum_specifier" => self.enum_declaration(node, context),
            "function_definition" => self.function(node, context),
            "declaration" | "field_declaration" => self.declaration(node, context),
            "alias_declaration" | "type_definition" | "concept_definition" => {
                self.alias(node, context)
            }
            "preproc_include" => self.include(node, context),
            "call_expression" => {
                self.call(node, context)?;
                self.walk_children(node, context)
            }
            _ => self.walk_children(node, context),
        }
    }

    fn walk_children(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child, context)?;
        }
        Ok(())
    }

    fn namespace(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name_node) = node.child_by_field_name("name") else {
            return self.walk_children(node, context);
        };
        let Some(name) = self.scoped_name(name_node) else {
            return self.walk_children(node, context);
        };
        let symbol = self.add_symbol(
            context,
            &name,
            SymbolKind::Module,
            name_node,
            self.signature(node),
        )?;
        let mut prefix = context.prefix.clone();
        prefix.extend(name.split("::").map(str::to_owned));
        let nested = Context {
            container: Some(prefix.join("::")),
            prefix,
            parent: Some(symbol),
            callable: None,
            in_type: false,
        };
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, &nested)
        } else {
            Ok(())
        }
    }

    fn type_declaration(
        &mut self,
        node: Node<'_>,
        context: &Context,
        kind: SymbolKind,
    ) -> Result<(), ParseError> {
        let Some(name_node) = node.child_by_field_name("name") else {
            return self.walk_children(node, context);
        };
        let Some(name) = self.scoped_name(name_node) else {
            return self.walk_children(node, context);
        };
        let simple = name.rsplit("::").next().unwrap_or(&name).to_owned();
        let symbol = self.add_symbol(context, &name, kind, name_node, self.signature(node))?;
        self.base_relations(node, context, symbol);
        let mut prefix = context.prefix.clone();
        prefix.extend(name.split("::").map(str::to_owned));
        let nested = Context {
            container: Some(Self::qualified(&context.prefix, &simple)),
            prefix,
            parent: Some(symbol),
            callable: None,
            in_type: true,
        };
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, &nested)
        } else {
            Ok(())
        }
    }

    fn base_relations(&mut self, node: Node<'_>, context: &Context, from: usize) {
        let mut cursor = node.walk();
        let Some(clause) = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "base_class_clause")
        else {
            return;
        };
        let mut bases = clause.walk();
        for base in clause.named_children(&mut bases) {
            let Some(name) = self.scoped_name(base) else {
                continue;
            };
            let mut candidates = Vec::new();
            if !context.prefix.is_empty() {
                candidates.push(Self::qualified(&context.prefix, &name));
            }
            if !candidates.contains(&name) {
                candidates.push(name);
            }
            self.named_relations.push(NamedRelationDraft {
                from,
                candidates,
                target_kinds: vec![SymbolKind::Class, SymbolKind::Struct],
                kind: EdgeKind::Extends,
            });
        }
    }

    fn enum_declaration(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name_node) = node.child_by_field_name("name") else {
            return self.walk_children(node, context);
        };
        let Some(name) = self.scoped_name(name_node) else {
            return self.walk_children(node, context);
        };
        let symbol = self.add_symbol(
            context,
            &name,
            SymbolKind::Enum,
            name_node,
            self.signature(node),
        )?;
        let mut prefix = context.prefix.clone();
        prefix.extend(name.split("::").map(str::to_owned));
        let nested = Context {
            container: Some(prefix.join("::")),
            prefix,
            parent: Some(symbol),
            callable: None,
            in_type: true,
        };
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for enumerator in body
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "enumerator")
            {
                if let Some(name_node) = enumerator.child_by_field_name("name")
                    && let Some(name) = self.text(name_node).map(str::trim)
                {
                    let name = name.to_owned();
                    self.add_symbol(
                        &nested,
                        &name,
                        SymbolKind::Constant,
                        name_node,
                        self.signature(enumerator),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn function(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(declarator) = node.child_by_field_name("declarator") else {
            return self.walk_children(node, context);
        };
        let Some(name_node) = declarator_name(declarator) else {
            return self.walk_children(node, context);
        };
        let Some(raw_name) = self.scoped_name(name_node) else {
            return self.walk_children(node, context);
        };
        let (name, test_macro) = test_name(&raw_name, declarator, self.source);
        let qualified_name = if raw_name.contains("::") && !test_macro {
            raw_name.clone()
        } else {
            name
        };
        let kind = if test_macro || is_test_function(&self.path, &qualified_name) {
            SymbolKind::Test
        } else if context.in_type || qualified_name.contains("::") {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };
        let symbol = self.add_symbol(
            context,
            &qualified_name,
            kind,
            name_node,
            self.signature(node),
        )?;
        let full = Self::qualified(&context.prefix, &qualified_name);
        let nested = Context {
            prefix: full.split("::").map(str::to_owned).collect(),
            container: Some(full),
            parent: Some(symbol),
            callable: Some(symbol),
            in_type: false,
        };
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, &nested)
        } else {
            Ok(())
        }
    }

    fn declaration(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        if context.callable.is_some() {
            return self.walk_children(node, context);
        }
        let Some(declarator) = node.child_by_field_name("declarator") else {
            return self.walk_children(node, context);
        };
        if contains_kind(declarator, "function_declarator") {
            if let Some(name_node) = declarator_name(declarator)
                && let Some(name) = self.scoped_name(name_node)
            {
                let kind = if context.in_type || name.contains("::") {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                self.add_symbol(context, &name, kind, name_node, self.signature(node))?;
            }
            return Ok(());
        }
        if context.in_type {
            for name_node in declarator_names(declarator) {
                if let Some(name) = self.scoped_name(name_node) {
                    self.add_symbol(
                        context,
                        &name,
                        SymbolKind::Field,
                        name_node,
                        self.signature(node),
                    )?;
                }
            }
        }
        self.walk_children(node, context)
    }

    fn alias(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let name_node = node.child_by_field_name("name").or_else(|| {
            node.child_by_field_name("declarator")
                .and_then(declarator_name)
        });
        if let Some(name_node) = name_node
            && let Some(name) = self.scoped_name(name_node)
        {
            self.add_symbol(
                context,
                &name,
                SymbolKind::TypeAlias,
                name_node,
                self.signature(node),
            )?;
        }
        Ok(())
    }

    fn include(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(path_node) = node.child_by_field_name("path") else {
            return Ok(());
        };
        let Some(path) = self.text(path_node).map(str::trim) else {
            return Ok(());
        };
        let normalized = path
            .trim_matches(|character| matches!(character, '<' | '>' | '"'))
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '_' {
                    character
                } else {
                    '_'
                }
            })
            .collect::<String>();
        if normalized.is_empty() {
            return Ok(());
        }
        let ordinal = self.import_ordinal;
        self.import_ordinal = self.import_ordinal.saturating_add(1);
        self.add_symbol(
            context,
            &format!("include_{normalized}_{ordinal}"),
            SymbolKind::Import,
            path_node,
            self.signature(node),
        )?;
        Ok(())
    }

    fn call(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(caller) = context.callable else {
            return Ok(());
        };
        let Some(function) = node.child_by_field_name("function") else {
            return Ok(());
        };
        let Some(mut target) = self.call_target(function) else {
            return Ok(());
        };
        if target.name.trim().is_empty() {
            return Ok(());
        }
        if target.form == CallForm::Function
            && target.target_kind == CallTargetKind::Function
            && self
                .symbols
                .get(caller)
                .is_some_and(|symbol| symbol.key.kind == SymbolKind::Method)
        {
            target.target_kind = CallTargetKind::Method;
        }
        self.calls.push(CallDraft {
            caller,
            form: target.form,
            target_kind: target.target_kind,
            name: target.name,
            qualifier: target.qualifier,
            receiver_hint: target.receiver_hint,
            location: self.range(target.location)?,
        });
        Ok(())
    }

    fn call_target<'tree>(&self, node: Node<'tree>) -> Option<CallTarget<'tree>> {
        match node.kind() {
            "identifier" | "field_identifier" => {
                let name = self.text(node)?.trim();
                if name.is_empty() {
                    return None;
                }
                Some(CallTarget {
                    form: CallForm::Function,
                    target_kind: CallTargetKind::Function,
                    name: name.to_owned(),
                    qualifier: None,
                    receiver_hint: None,
                    location: node,
                })
            }
            "qualified_identifier" => {
                let raw = self.scoped_name(node)?;
                let (qualifier, name) = raw.rsplit_once("::")?;
                Some(CallTarget {
                    form: CallForm::Function,
                    target_kind: CallTargetKind::Function,
                    name: name.to_owned(),
                    qualifier: Some(qualifier.to_owned()),
                    receiver_hint: None,
                    location: declarator_name(node).unwrap_or(node),
                })
            }
            "field_expression" => {
                let field = node.child_by_field_name("field")?;
                let object = node.child_by_field_name("argument")?;
                let name = self.text(field)?.trim();
                if name.is_empty() {
                    return None;
                }
                Some(CallTarget {
                    form: CallForm::Member,
                    target_kind: CallTargetKind::Method,
                    name: name.to_owned(),
                    qualifier: None,
                    receiver_hint: self.text(object).and_then(bounded_receiver_hint),
                    location: field,
                })
            }
            "template_function" | "template_method" => {
                let name = node
                    .child_by_field_name("name")
                    .or_else(|| node.named_child(0))?;
                self.call_target(name)
            }
            "parenthesized_expression" => node
                .named_child(0)
                .and_then(|inner| self.call_target(inner)),
            _ => None,
        }
    }

    fn scoped_name(&self, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "identifier"
            | "field_identifier"
            | "namespace_identifier"
            | "type_identifier"
            | "statement_identifier"
            | "destructor_name"
            | "operator_name" => self
                .text(node)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            "qualified_identifier" | "nested_namespace_specifier" => self
                .text(node)
                .map(str::trim)
                .and_then(normalize_scoped_name),
            "template_type" | "template_function" | "template_method" => node
                .child_by_field_name("name")
                .or_else(|| node.named_child(0))
                .and_then(|name| self.scoped_name(name)),
            _ => self
                .text(node)
                .map(str::trim)
                .and_then(normalize_scoped_name),
        }
    }
}

struct CallTarget<'tree> {
    form: CallForm,
    target_kind: CallTargetKind,
    name: String,
    qualifier: Option<String>,
    receiver_hint: Option<String>,
    location: Node<'tree>,
}

fn contains_kind(node: Node<'_>, kind: &str) -> bool {
    if node.kind() == kind {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| contains_kind(child, kind))
}

fn declarator_name(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "identifier"
        | "field_identifier"
        | "type_identifier"
        | "statement_identifier"
        | "destructor_name"
        | "operator_name"
        | "qualified_identifier" => Some(node),
        "function_declarator"
        | "pointer_declarator"
        | "reference_declarator"
        | "parenthesized_declarator"
        | "attributed_declarator"
        | "init_declarator"
        | "array_declarator" => node
            .child_by_field_name("declarator")
            .or_else(|| node.named_child(0))
            .and_then(declarator_name),
        "template_function" | "template_method" | "template_type" => node
            .child_by_field_name("name")
            .or_else(|| node.named_child(0))
            .and_then(declarator_name),
        _ => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).find_map(declarator_name)
        }
    }
}

fn declarator_names(node: Node<'_>) -> Vec<Node<'_>> {
    let mut names = Vec::new();
    collect_declarator_names(node, &mut names);
    names
}

fn collect_declarator_names<'tree>(node: Node<'tree>, names: &mut Vec<Node<'tree>>) {
    if matches!(
        node.kind(),
        "identifier" | "field_identifier" | "type_identifier"
    ) {
        names.push(node);
        return;
    }
    if node.kind() == "function_declarator" {
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_declarator_names(child, names);
    }
}

fn normalize_scoped_name(raw: &str) -> Option<String> {
    let mut normalized = String::new();
    let mut template_depth = 0_u32;
    let mut chars = raw.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '<' => template_depth = template_depth.saturating_add(1),
            '>' => template_depth = template_depth.saturating_sub(1),
            ':' if template_depth == 0 && chars.peek() == Some(&':') => {
                chars.next();
                if !normalized.ends_with("::") && !normalized.is_empty() {
                    normalized.push_str("::");
                }
            }
            character
                if template_depth == 0 && (character.is_alphanumeric() || character == '_') =>
            {
                normalized.push(character)
            }
            '~' if template_depth == 0 => normalized.push('~'),
            _ => {}
        }
    }
    let normalized = normalized.trim_matches(':');
    (!normalized.is_empty()).then(|| normalized.to_owned())
}

fn bounded_receiver_hint(raw: &str) -> Option<String> {
    let hint = raw
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .rfind(|part| !part.is_empty())?;
    (hint.chars().count() <= MAX_RECEIVER_HINT_CHARS).then(|| hint.to_owned())
}

fn test_name(raw_name: &str, declarator: Node<'_>, source: &str) -> (String, bool) {
    let macro_name = raw_name.rsplit("::").next().unwrap_or(raw_name);
    if !matches!(
        macro_name,
        "TEST" | "TEST_F" | "TEST_P" | "TYPED_TEST" | "TYPED_TEST_P" | "TEST_CASE"
    ) {
        return (raw_name.to_owned(), false);
    }
    let parameters = find_kind(declarator, "parameter_list")
        .and_then(|node| source.get(node.byte_range()))
        .unwrap_or_default();
    let detail = parameters
        .trim_matches(|character| matches!(character, '(' | ')' | '"' | ' '))
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_owned();
    let name = if detail.is_empty() {
        macro_name.to_ascii_lowercase()
    } else {
        format!("{}_{detail}", macro_name.to_ascii_lowercase())
    };
    (name, true)
}

fn find_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    if node.kind() == kind {
        return Some(node);
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find_map(|child| find_kind(child, kind))
}

fn is_test_function(path: &RepoRelativePath, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let named = lower.starts_with("test_")
        || lower.ends_with("_test")
        || lower.starts_with("spec_")
        || lower.ends_with("_spec");
    let test_path = path.as_str().split('/').any(|component| {
        matches!(
            component.to_ascii_lowercase().as_str(),
            "test" | "tests" | "spec"
        )
    });
    named || (test_path && lower.contains("test"))
}

pub(crate) fn module_path(path: &RepoRelativePath) -> Vec<String> {
    let mut components: Vec<String> = path
        .as_str()
        .split('/')
        .filter(|component| !component.is_empty())
        .map(str::to_owned)
        .collect();
    if let Some(last) = components.last_mut()
        && let Some((stem, _)) = last.rsplit_once('.')
    {
        *last = stem.to_owned();
    }
    if components.is_empty() {
        components.push("translation_unit".to_owned());
    }
    components
}

pub(crate) struct CppParser {
    parser: Parser,
}

impl CppParser {
    pub(crate) fn new() -> Result<Self, ParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .map_err(|error| ParseError::Language(error.to_string()))?;
        Ok(Self { parser })
    }

    pub(crate) fn parse(
        &mut self,
        path: RepoRelativePath,
        source: impl Into<Arc<str>>,
    ) -> Result<ParsedFile, ParseError> {
        let source = source.into();
        let tree = self
            .parser
            .parse(source.as_ref(), None)
            .ok_or_else(|| ParseError::NoTree(path.clone()))?;
        let root = tree.root_node();
        let module_path = module_path(&path);
        let mut extraction = Extraction {
            path,
            source: source.as_ref(),
            line_starts: std::iter::once(0)
                .chain(source.match_indices('\n').map(|(index, _)| index + 1))
                .collect(),
            symbols: Vec::new(),
            calls: Vec::new(),
            named_relations: Vec::new(),
            import_ordinal: 0,
        };
        let module = extraction.symbols.len();
        extraction.symbols.push(SymbolDraft {
            key: SymbolKey {
                language: Language::Cpp,
                qualified_name: module_path.join("::"),
                container: module_path
                    .split_last()
                    .and_then(|(_, parent)| (!parent.is_empty()).then(|| parent.join("::"))),
                kind: SymbolKind::Module,
                path: extraction.path.clone(),
            },
            location: extraction.range(root)?,
            signature: Some("C/C++ translation unit".to_owned()),
            parent: None,
        });
        let context = Context {
            prefix: Vec::new(),
            container: Some(module_path.join("::")),
            parent: Some(module),
            callable: None,
            in_type: false,
        };
        extraction.walk(root, &context)?;
        let (diagnostics, diagnostic_count) = extraction.diagnostics(root)?;
        Ok(ParsedFile {
            source: source.clone(),
            module_path,
            symbols: extraction.symbols,
            calls: extraction.calls,
            named_relations: extraction.named_relations,
            has_errors: root.has_error(),
            diagnostics,
            diagnostic_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_name(symbol: &SymbolDraft) -> &str {
        symbol
            .key
            .qualified_name
            .rsplit("::")
            .next()
            .unwrap_or(&symbol.key.qualified_name)
    }

    #[test]
    fn parses_namespaces_templates_includes_types_tests_and_calls()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = RepoRelativePath::new("src/payment_service.cpp")?;
        let source: Arc<str> = Arc::from(
            r#"#include <string>
#include "shared.hpp"

namespace chakra::payments {
template <typename T>
class Service : public BaseService {
 public:
  T refund(T value) { return normalize(value); }
  int count = 0;
};

int normalize(int value) { return value; }
TEST(PaymentService, Refunds) { normalize(1); }
}
"#,
        );
        let parsed = CppParser::new()?.parse(path, source)?;
        assert!(!parsed.has_errors, "diagnostics: {:?}", parsed.diagnostics);
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.key.kind == SymbolKind::Class
                    && simple_name(symbol) == "Service")
        );
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.key.kind == SymbolKind::Method
                    && simple_name(symbol) == "refund")
        );
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.key.kind == SymbolKind::Function
                    && simple_name(symbol) == "normalize")
        );
        assert_eq!(
            parsed
                .symbols
                .iter()
                .filter(|symbol| symbol.key.kind == SymbolKind::Import)
                .count(),
            2
        );
        assert!(parsed.calls.iter().any(|call| call.name == "normalize"));
        assert!(!parsed.named_relations.is_empty());
        Ok(())
    }

    #[test]
    fn malformed_cpp_retains_valid_symbols_and_reports_diagnostics()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = RepoRelativePath::new("src/broken.cpp")?;
        let parsed = CppParser::new()?.parse(
            path,
            Arc::<str>::from(
                "int retained_marker() { return 1; }\nvoid broken() { (); target.(); }\nclass Broken {\n",
            ),
        )?;
        assert!(parsed.has_errors);
        assert!(parsed.diagnostic_count > 0);
        assert!(parsed.diagnostics.len() <= MAX_SYNTAX_DIAGNOSTICS_PER_FILE);
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| simple_name(symbol) == "retained_marker")
        );
        assert!(parsed.calls.iter().all(|call| !call.name.trim().is_empty()));
        Ok(())
    }
}
