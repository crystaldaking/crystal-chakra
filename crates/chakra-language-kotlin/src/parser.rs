//! Tree-sitter Kotlin extraction into language-neutral Chakra drafts.
//!
//! The syntax tier records file/package containers, imports and aliases,
//! named types (classes, interfaces, objects, companions, enums), enum
//! entries, properties, constructors, type aliases, extension receivers,
//! annotations, inheritance delegation, Kotlin/JUnit test hints,
//! diagnostics, and bounded static call candidates. Overload resolution,
//! nullable/safe-call semantics, generics, and coroutine dispatch are the
//! precise provider's responsibility; unresolved candidates stay explicit
//! (ADR-0056).

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
pub(crate) use chakra_language_index::facts::{
    CallDraft, NamedRelationDraft, ParsedFile, SymbolDraft,
};
use thiserror::Error;
use tree_sitter::{Node, Parser, Point};

const MAX_SIGNATURE_CHARS: usize = 512;

fn nested_container(parent: Option<&str>, name: &str) -> String {
    match parent {
        Some(parent) => format!("{parent}::{name}"),
        None => name.to_owned(),
    }
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("failed to load the Tree-sitter Kotlin grammar: {0}")]
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

#[derive(Debug)]
struct Extraction<'a> {
    path: RepoRelativePath,
    source: &'a str,
    line_starts: Vec<usize>,
    package: String,
    module: usize,
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
        let line = u32::try_from(point.row + 1).map_err(|source| ParseError::PositionInteger {
            path: self.path.clone(),
            source,
        })?;
        let column = u32::try_from(
            self.source[line_start..line_start + point.column]
                .chars()
                .count()
                + 1,
        )
        .map_err(|source| ParseError::PositionInteger {
            path: self.path.clone(),
            source,
        })?;
        TextPosition::new(line, column).map_err(|error| ParseError::Range {
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
                        language: Language::Kotlin,
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
                        total = 1;
                        diagnostics.push(SyntaxDiagnostic {
                            language: Language::Kotlin,
                            range: self.range(root)?,
                            kind: SyntaxDiagnosticKind::Error,
                            provenance: Provenance::TreeSitter,
                            precision: Precision::Syntax,
                            cause: SyntaxDiagnosticCause::ParseRecovery,
                            node_kind: "<unlocated-error>".to_owned(),
                        });
                    }
                    return Ok((diagnostics, total));
                }
            }
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
        let mut signature = String::new();
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

    fn qualified(&self, container: Option<&str>, name: &str) -> String {
        match container {
            Some(container) => format!("{}::{container}::{name}", self.package),
            None => format!("{}::{name}", self.package),
        }
    }

    fn add_symbol(
        &mut self,
        name: &str,
        container: Option<&str>,
        kind: SymbolKind,
        node: Node<'_>,
        parent: Option<usize>,
    ) -> Result<usize, ParseError> {
        let index = self.symbols.len();
        self.symbols.push(SymbolDraft {
            key: SymbolKey {
                language: Language::Kotlin,
                qualified_name: self.qualified(container, name),
                container: container
                    .map(|value| format!("{}::{value}", self.package))
                    .or_else(|| Some(self.package.clone())),
                kind,
                path: self.path.clone(),
            },
            location: self.range(node)?,
            signature: self.signature(node),
            parent: parent.or(Some(self.module)),
        });
        Ok(index)
    }

    fn record_imports(&mut self, root: Node<'_>) -> Result<(), ParseError> {
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() != "import" {
                continue;
            }
            let mut path = String::new();
            let mut alias = String::new();
            let mut walker = child.walk();
            for part in child.named_children(&mut walker) {
                match part.kind() {
                    "qualified_identifier" => {
                        path = self.text(part).unwrap_or_default().to_owned();
                    }
                    "identifier" => {
                        alias = self.text(part).unwrap_or_default().to_owned();
                    }
                    _ => {}
                }
            }
            if path.is_empty() {
                continue;
            }
            let name = if alias.is_empty() {
                format!("import::{:04}::{path}", self.import_ordinal)
            } else {
                format!("import::{:04}::{alias}::{path}", self.import_ordinal)
            };
            self.import_ordinal += 1;
            self.add_symbol(&name, None, SymbolKind::Import, child, None)?;
        }
        Ok(())
    }

    /// Annotations used on one declaration, as reference candidates with
    /// explicit syntax precision (ADR-0056 extraction contract).
    fn record_annotations(&mut self, node: Node<'_>, from: usize) -> Result<(), ParseError> {
        let mut walker = node.walk();
        for child in node.named_children(&mut walker) {
            if child.kind() != "modifiers" {
                continue;
            }
            let mut modifiers = child.walk();
            for modifier in child.named_children(&mut modifiers) {
                if modifier.kind() != "annotation" {
                    continue;
                }
                if let Some(name) = annotation_name(modifier, self.source) {
                    self.named_relations.push(NamedRelationDraft {
                        from,
                        candidates: vec![name],
                        target_kinds: vec![SymbolKind::Class, SymbolKind::Interface],
                        kind: EdgeKind::References,
                    });
                }
            }
        }
        Ok(())
    }

    fn has_test_annotation(&self, node: Node<'_>) -> bool {
        let mut walker = node.walk();
        node.named_children(&mut walker)
            .filter(|child| child.kind() == "modifiers")
            .flat_map(|modifiers| {
                let mut cursor = modifiers.walk();
                let annotations: Vec<Node<'_>> = modifiers
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "annotation")
                    .collect();
                annotations
            })
            .any(|annotation| {
                annotation_name(annotation, self.source)
                    .map(|name| name == "Test")
                    .unwrap_or(false)
            })
    }

    fn visit_declaration(
        &mut self,
        node: Node<'_>,
        container: Option<&str>,
        parent: usize,
    ) -> Result<(), ParseError> {
        match node.kind() {
            "class_declaration" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return Ok(());
                };
                let Some(name) = self.text(name_node).map(str::to_owned) else {
                    return Ok(());
                };
                let kind = class_kind(node, self.source);
                let index = self.add_symbol(&name, container, kind, node, Some(parent))?;
                self.record_annotations(node, index)?;
                self.record_delegation(node, index)?;
                self.visit_members(node, &nested_container(container, &name), index)
            }
            "object_declaration" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return Ok(());
                };
                let Some(name) = self.text(name_node).map(str::to_owned) else {
                    return Ok(());
                };
                let index =
                    self.add_symbol(&name, container, SymbolKind::Class, node, Some(parent))?;
                self.record_annotations(node, index)?;
                self.record_delegation(node, index)?;
                self.visit_members(node, &nested_container(container, &name), index)
            }
            "companion_object" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|name| self.text(name))
                    .unwrap_or("Companion")
                    .to_owned();
                let index =
                    self.add_symbol(&name, container, SymbolKind::Class, node, Some(parent))?;
                self.visit_members(node, &nested_container(container, &name), index)
            }
            "enum_entry" => {
                let mut walker = node.walk();
                let Some(name) = node
                    .named_children(&mut walker)
                    .find(|child| child.kind() == "identifier")
                    .and_then(|identifier| self.text(identifier))
                    .map(str::to_owned)
                else {
                    return Ok(());
                };
                self.add_symbol(&name, container, SymbolKind::Constant, node, Some(parent))?;
                Ok(())
            }
            "property_declaration" => self.visit_property(node, container, parent),
            "function_declaration" => self.visit_function(node, container, parent),
            "secondary_constructor" => {
                let name = container
                    .and_then(|name| name.rsplit("::").next())
                    .unwrap_or("constructor")
                    .to_owned();
                let index =
                    self.add_symbol(&name, container, SymbolKind::Method, node, Some(parent))?;
                self.record_annotations(node, index)?;
                self.collect_calls(node, index)
            }
            "type_alias" => {
                let Some(name) = type_alias_name(node, self.source) else {
                    return Ok(());
                };
                let index =
                    self.add_symbol(&name, container, SymbolKind::TypeAlias, node, Some(parent))?;
                self.record_annotations(node, index)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn visit_members(
        &mut self,
        node: Node<'_>,
        container: &str,
        parent: usize,
    ) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if !matches!(child.kind(), "class_body" | "enum_class_body") {
                continue;
            }
            let mut members = child.walk();
            for member in child.named_children(&mut members) {
                let declaration = if member.kind() == "class_member_declaration" {
                    let mut inner = member.walk();
                    member.named_children(&mut inner).next()
                } else {
                    Some(member)
                };
                if let Some(declaration) = declaration {
                    self.visit_declaration(declaration, Some(container), parent)?;
                }
            }
        }
        Ok(())
    }

    fn visit_property(
        &mut self,
        node: Node<'_>,
        container: Option<&str>,
        parent: usize,
    ) -> Result<(), ParseError> {
        let mut walker = node.walk();
        let name = node
            .named_children(&mut walker)
            .find(|child| child.kind() == "variable_declaration")
            .and_then(|declaration| {
                let mut inner = declaration.walk();
                declaration
                    .named_children(&mut inner)
                    .find(|child| child.kind() == "identifier")
            })
            .and_then(|identifier| self.text(identifier))
            .map(str::to_owned);
        let Some(name) = name else {
            return Ok(());
        };
        let is_const = node
            .named_children(&mut node.walk())
            .filter(|child| child.kind() == "modifiers")
            .any(|modifiers| {
                self.text(modifiers)
                    .is_some_and(|text| text.contains("const"))
            });
        let kind = if is_const {
            SymbolKind::Constant
        } else {
            SymbolKind::Property
        };
        let index = self.add_symbol(&name, container, kind, node, Some(parent))?;
        self.record_annotations(node, index)?;
        self.collect_calls(node, index)
    }

    fn visit_function(
        &mut self,
        node: Node<'_>,
        container: Option<&str>,
        parent: usize,
    ) -> Result<(), ParseError> {
        let Some(name_node) = node.child_by_field_name("name") else {
            return Ok(());
        };
        let Some(name) = self.text(name_node).map(str::to_owned) else {
            return Ok(());
        };
        let kind = if self.has_test_annotation(node) {
            SymbolKind::Test
        } else if container.is_some() {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };
        let index = self.add_symbol(&name, container, kind, node, Some(parent))?;
        self.record_annotations(node, index)?;
        self.collect_calls(node, index)
    }

    /// Inheritance from `delegation_specifiers`: a `constructor_invocation`
    /// is the superclass (Extends); every other specifier is an interface
    /// (Implements). Resolution stays syntax-tier and explicit.
    fn record_delegation(&mut self, node: Node<'_>, from: usize) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() != "delegation_specifiers" {
                continue;
            }
            let mut specifiers = child.walk();
            for specifier in child.named_children(&mut specifiers) {
                if specifier.kind() != "delegation_specifier" {
                    continue;
                }
                let mut extends = false;
                let mut walker = specifier.walk();
                for part in specifier.named_children(&mut walker) {
                    match part.kind() {
                        "constructor_invocation" => {
                            extends = true;
                            if let Some(name) = first_type_text(part, self.source) {
                                self.named_relations.push(NamedRelationDraft {
                                    from,
                                    candidates: vec![name],
                                    target_kinds: vec![SymbolKind::Class],
                                    kind: EdgeKind::Extends,
                                });
                            }
                        }
                        "explicit_delegation" | "type" | "user_type" => {
                            if let Some(name) = first_type_text(part, self.source) {
                                self.named_relations.push(NamedRelationDraft {
                                    from,
                                    candidates: vec![name],
                                    target_kinds: vec![SymbolKind::Interface, SymbolKind::Class],
                                    kind: EdgeKind::Implements,
                                });
                            }
                        }
                        _ => {}
                    }
                }
                let _ = extends;
            }
        }
        Ok(())
    }

    /// Bounded lazy call candidates inside one callable or initializer.
    fn collect_calls(&mut self, node: Node<'_>, caller: usize) -> Result<(), ParseError> {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if current.kind() == "call_expression"
                && let Some(draft) = self.call_draft(current, caller)?
            {
                self.calls.push(draft);
            }
            let mut cursor = current.walk();
            for child in current.named_children(&mut cursor) {
                stack.push(child);
            }
        }
        Ok(())
    }

    fn call_draft(&self, node: Node<'_>, caller: usize) -> Result<Option<CallDraft>, ParseError> {
        let mut cursor = node.walk();
        let Some(callee) = node.named_children(&mut cursor).next() else {
            return Ok(None);
        };
        let location = self.range(node)?;
        let draft = match callee.kind() {
            "identifier" => CallDraft {
                caller,
                form: CallForm::Function,
                target_kind: CallTargetKind::FunctionOrMethod,
                name: self.text(callee).unwrap_or_default().to_owned(),
                qualifier: None,
                receiver_hint: None,
                promoted: false,
                location,
            },
            "navigation_expression" => {
                let (receiver, name, nullsafe) = navigation_parts(callee, self.source);
                let Some(name) = name else {
                    return Ok(None);
                };
                CallDraft {
                    caller,
                    form: if nullsafe {
                        CallForm::NullsafeMember
                    } else {
                        CallForm::Member
                    },
                    target_kind: CallTargetKind::Method,
                    name,
                    qualifier: receiver.clone(),
                    receiver_hint: receiver
                        .map(|hint| hint.chars().take(MAX_RECEIVER_HINT_CHARS).collect()),
                    promoted: false,
                    location,
                }
            }
            _ => return Ok(None),
        };
        if draft.name.is_empty() {
            return Ok(None);
        }
        Ok(Some(draft))
    }
}

/// Keyword-driven class kind: `interface` and `enum` markers win over the
/// class default; everything else (data, sealed, value, annotation) remains
/// a class at the syntax tier.
fn class_kind(node: Node<'_>, source: &str) -> SymbolKind {
    let text = node_text_prefix(node, source, 64);
    if text.contains("interface") {
        return SymbolKind::Interface;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "class_modifier"
            && source
                .get(child.byte_range())
                .is_some_and(|modifier| modifier == "enum")
        {
            return SymbolKind::Enum;
        }
    }
    if text.starts_with("enum ") || text.contains(" enum ") {
        return SymbolKind::Enum;
    }
    SymbolKind::Class
}

fn node_text_prefix(node: Node<'_>, source: &str, max: usize) -> String {
    source
        .get(node.start_byte()..node.end_byte().min(node.start_byte() + max))
        .unwrap_or_default()
        .to_owned()
}

/// Simple name of an annotation (`@Foo` / `@com.example.Foo` -> `Foo`).
fn annotation_name(node: Node<'_>, source: &str) -> Option<String> {
    let raw = source.get(node.byte_range())?.trim_start_matches('@');
    let name = raw
        .split(['(', ' ', '\t', '\n'])
        .next()?
        .rsplit('.')
        .next()?
        .trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// First `type`/`user_type` text inside a delegation part.
fn first_type_text(node: Node<'_>, source: &str) -> Option<String> {
    if matches!(node.kind(), "type" | "user_type") {
        return source
            .get(node.byte_range())
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty());
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = first_type_text(child, source) {
            return Some(found);
        }
    }
    None
}

/// The alias name: the `type` field of a `type_alias` is the declared
/// identifier (grammar: `field('type', $.identifier)`); the target type is a
/// following bare `type` child.
fn type_alias_name(node: Node<'_>, source: &str) -> Option<String> {
    let name = node.child_by_field_name("type")?;
    source
        .get(name.byte_range())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// Split a navigation expression into (receiver text, member name,
/// nullsafe). The member name is the trailing `identifier`; the receiver is
/// everything before the final navigation operator.
fn navigation_parts(node: Node<'_>, source: &str) -> (Option<String>, Option<String>, bool) {
    let raw = source.get(node.byte_range()).unwrap_or_default();
    let nullsafe = raw.contains("?.");
    let mut cursor = node.walk();
    let mut identifier = None;
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier" {
            identifier = source.get(child.byte_range()).map(str::to_owned);
        }
    }
    let receiver = identifier.as_ref().and_then(|name| {
        raw.rsplit_once(name.as_str())
            .map(|(receiver, _)| receiver.trim_end_matches(['.', '?']).trim().to_owned())
            .filter(|receiver| !receiver.is_empty())
    });
    (receiver, identifier, nullsafe)
}

fn package_name(root: Node<'_>, source: &str) -> String {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|child| child.kind() == "package_header")
        .and_then(|header| {
            let mut children = header.walk();
            header
                .named_children(&mut children)
                .find(|child| child.kind() == "qualified_identifier")
        })
        .and_then(|node| source.get(node.byte_range()))
        .unwrap_or("root")
        .to_owned()
}

/// Repository-rooted module path: package segments plus the file stem.
pub(crate) fn module_path(path: &RepoRelativePath, package: &str) -> Vec<String> {
    let stem = path
        .as_str()
        .rsplit('/')
        .next()
        .map(|file| {
            file.strip_suffix(".kts")
                .or_else(|| file.strip_suffix(".kt"))
                .unwrap_or(file)
        })
        .filter(|stem| !stem.is_empty())
        .unwrap_or("source");
    let mut segments: Vec<String> = package
        .split('.')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    segments.push(stem.to_owned());
    segments
}

pub struct KotlinParser {
    parser: Parser,
}

impl KotlinParser {
    pub fn new() -> Result<Self, ParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .map_err(|error| ParseError::Language(error.to_string()))?;
        Ok(Self { parser })
    }

    pub fn parse(
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
        let package = package_name(root, source.as_ref());
        let module_path = module_path(&path, &package);
        let mut extraction = Extraction {
            path,
            source: source.as_ref(),
            line_starts: std::iter::once(0)
                .chain(source.match_indices('\n').map(|(index, _)| index + 1))
                .collect(),
            package: package.clone(),
            module: 0,
            symbols: Vec::new(),
            calls: Vec::new(),
            named_relations: Vec::new(),
            import_ordinal: 0,
        };
        extraction.symbols.push(SymbolDraft {
            key: SymbolKey {
                language: Language::Kotlin,
                qualified_name: module_path.join("::"),
                container: Some(package.clone()),
                kind: SymbolKind::Module,
                path: extraction.path.clone(),
            },
            location: extraction.range(root)?,
            signature: Some(format!("package {package}")),
            parent: None,
        });
        extraction.record_imports(root)?;
        let module = extraction.module;
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            extraction.visit_declaration(child, None, module)?;
        }
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

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn parse(source: &str) -> Result<ParsedFile, Box<dyn std::error::Error>> {
        let mut parser = KotlinParser::new()?;
        Ok(parser.parse(RepoRelativePath::new("src/main/kotlin/App.kt")?, source)?)
    }

    fn simple_name(symbol: &SymbolDraft) -> &str {
        symbol
            .key
            .qualified_name
            .rsplit("::")
            .next()
            .unwrap_or(&symbol.key.qualified_name)
    }

    fn names(parsed: &ParsedFile, kind: SymbolKind) -> Vec<String> {
        let mut found: Vec<String> = parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.key.kind == kind)
            .map(|symbol| simple_name(symbol).to_owned())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn parses_package_imports_types_and_functions() -> TestResult {
        let parsed = parse(
            r#"package com.example.app

import kotlin.collections.List
import java.io.File as IoFile

interface Greeter {
    fun greet(name: String): String
}

class App : Greeter {
    override fun greet(name: String): String = "hi $name"

    companion object {
        const val VERSION: String = "1.0"
    }
}

object Registry

enum class Level { LOW, HIGH }

typealias Handler = (String) -> Unit

fun main() {
    App().greet("world")
}
"#,
        )?;
        assert!(!parsed.has_errors, "{:?}", parsed.diagnostics);
        assert_eq!(names(&parsed, SymbolKind::Interface), ["Greeter"]);
        assert_eq!(
            names(&parsed, SymbolKind::Class),
            ["App", "Companion", "Registry"]
        );
        assert_eq!(names(&parsed, SymbolKind::Enum), ["Level"]);
        assert_eq!(
            names(&parsed, SymbolKind::Constant),
            ["HIGH", "LOW", "VERSION"]
        );
        assert_eq!(names(&parsed, SymbolKind::TypeAlias), ["Handler"]);
        // One declaration in Greeter, one override in App.
        assert_eq!(names(&parsed, SymbolKind::Method), ["greet", "greet"]);
        assert_eq!(names(&parsed, SymbolKind::Function), ["main"]);
        assert_eq!(names(&parsed, SymbolKind::Module), ["App"]);
        let imports: Vec<&str> = parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.key.kind == SymbolKind::Import)
            .map(|symbol| symbol.key.qualified_name.as_str())
            .collect();
        assert_eq!(imports.len(), 2, "{imports:?}");
        assert!(
            imports
                .iter()
                .any(|name| name.contains("kotlin.collections.List")),
            "{imports:?}"
        );
        assert!(
            imports.iter().any(|name| name.contains("IoFile")),
            "{imports:?}"
        );
        let qualified = parsed
            .symbols
            .iter()
            .find(|symbol| simple_name(symbol) == "Greeter")
            .ok_or("Greeter missing")?;
        assert_eq!(qualified.key.qualified_name, "com.example.app::Greeter");
        Ok(())
    }

    #[test]
    fn nested_members_keep_full_container_chains_and_parents() -> TestResult {
        let parsed = parse(
            r#"package review
class Alpha {
    class Inner {
        fun work() {}
        companion object {
            fun create() {}
        }
    }
}
class Beta {
    class Inner {
        fun work() {}
    }
    object Nested {
        fun work() {}
    }
}
"#,
        )?;
        assert!(!parsed.has_errors, "{:?}", parsed.diagnostics);
        let expected = [
            ("review::Alpha::Inner::work", "review::Alpha::Inner"),
            (
                "review::Alpha::Inner::Companion::create",
                "review::Alpha::Inner::Companion",
            ),
            ("review::Beta::Inner::work", "review::Beta::Inner"),
            ("review::Beta::Nested::work", "review::Beta::Nested"),
        ];
        for (name, parent) in expected {
            let symbol = parsed
                .symbols
                .iter()
                .find(|symbol| symbol.key.qualified_name == name)
                .ok_or_else(|| format!("missing {name}"))?;
            assert_eq!(symbol.key.container.as_deref(), Some(parent));
            assert_eq!(
                parsed.symbols[symbol.parent.ok_or("parent")?]
                    .key
                    .qualified_name,
                parent
            );
        }
        Ok(())
    }

    #[test]
    fn extension_functions_properties_and_delegation() -> TestResult {
        let parsed = parse(
            r#"package ext

open class Base
interface Named

class Child : Base(), Named {
    var title: String = "child"
    val answer: Int
        get() = 42
}

fun String.shout(): String = this.uppercase()
"#,
        )?;
        assert!(!parsed.has_errors, "{:?}", parsed.diagnostics);
        assert_eq!(names(&parsed, SymbolKind::Class), ["Base", "Child"]);
        assert_eq!(names(&parsed, SymbolKind::Interface), ["Named"]);
        assert_eq!(names(&parsed, SymbolKind::Property), ["answer", "title"]);
        assert_eq!(names(&parsed, SymbolKind::Function), ["shout"]);
        let relations: Vec<(&[String], EdgeKind)> = parsed
            .named_relations
            .iter()
            .map(|relation| (relation.candidates.as_slice(), relation.kind))
            .collect();
        assert!(
            relations
                .iter()
                .any(|(candidates, kind)| candidates.contains(&"Base".to_owned())
                    && *kind == EdgeKind::Extends),
            "{relations:?}"
        );
        assert!(
            relations.iter().any(
                |(candidates, kind)| candidates.contains(&"Named".to_owned())
                    && *kind == EdgeKind::Implements
            ),
            "{relations:?}"
        );
        Ok(())
    }

    #[test]
    fn annotations_and_test_hints_are_recorded() -> TestResult {
        let parsed = parse(
            r#"package tests

import kotlin.test.Test

class Suite {
    @Test
    fun verifiesBehavior() {
        helper()
    }

    fun helper() {}
}
"#,
        )?;
        assert!(!parsed.has_errors, "{:?}", parsed.diagnostics);
        assert_eq!(names(&parsed, SymbolKind::Test), ["verifiesBehavior"]);
        assert_eq!(names(&parsed, SymbolKind::Method), ["helper"]);
        assert!(
            parsed
                .named_relations
                .iter()
                .any(|relation| relation.candidates.contains(&"Test".to_owned())
                    && relation.kind == EdgeKind::References),
            "{:?}",
            parsed.named_relations
        );
        Ok(())
    }

    #[test]
    fn member_and_nullsafe_calls_are_bounded_candidates() -> TestResult {
        let parsed = parse(
            r#"package calls

class Service {
    fun find(): Service? = null
}

fun top() {}

fun run(service: Service?) {
    top()
    service?.find()
    Service().hashCode()
}
"#,
        )?;
        assert!(!parsed.has_errors, "{:?}", parsed.diagnostics);
        let by_name = |name: &str| parsed.calls.iter().find(|call| call.name == name).cloned();
        let top = by_name("top").ok_or("top call missing")?;
        assert_eq!(top.form, CallForm::Function);
        let find = by_name("find").ok_or("find call missing")?;
        assert_eq!(find.form, CallForm::NullsafeMember);
        assert_eq!(find.target_kind, CallTargetKind::Method);
        let hash = by_name("hashCode").ok_or("hashCode call missing")?;
        assert_eq!(hash.form, CallForm::Member);
        Ok(())
    }

    #[test]
    fn scripts_and_unicode_and_incomplete_source_are_handled() -> TestResult {
        let mut parser = KotlinParser::new()?;
        let script = parser.parse(
            RepoRelativePath::new("build.gradle.kts")?,
            r#"plugins { kotlin("jvm") version "2.0.0" }
val имя = "привет"
"#,
        )?;
        assert_eq!(names(&script, SymbolKind::Property), ["имя"]);
        let broken = parse("class Broken { fun oops( \n")?;
        assert!(broken.has_errors);
        assert!(broken.diagnostic_count >= 1);
        assert!(!broken.diagnostics.is_empty());
        Ok(())
    }

    #[test]
    fn kts_scripts_keep_their_file_stem_in_the_module_path() -> TestResult {
        let mut parser = KotlinParser::new()?;
        let build = parser.parse(
            RepoRelativePath::new("build.gradle.kts")?,
            "plugins { kotlin(\"jvm\") }\n",
        )?;
        let settings = parser.parse(
            RepoRelativePath::new("settings.gradle.kts")?,
            "rootProject.name = \"sample\"\n",
        )?;
        let build_module = build
            .symbols
            .iter()
            .find(|symbol| symbol.key.kind == SymbolKind::Module)
            .ok_or("build script module missing")?;
        let settings_module = settings
            .symbols
            .iter()
            .find(|symbol| symbol.key.kind == SymbolKind::Module)
            .ok_or("settings script module missing")?;
        assert_eq!(build_module.key.qualified_name, "root::build.gradle");
        assert_eq!(settings_module.key.qualified_name, "root::settings.gradle");
        assert_ne!(
            build_module.key.qualified_name, settings_module.key.qualified_name,
            "distinct .kts scripts must not share a module identity"
        );
        Ok(())
    }
}
