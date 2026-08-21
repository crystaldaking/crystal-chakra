//! Tree-sitter Java extraction into language-neutral Chakra drafts.
//!
//! Grammar coverage follows ADR-0027: one Java grammar parses `.java`
//! sources. Extraction is deliberately syntactic: heritage and call
//! resolution only follow names resolvable from the source text (same
//! package, single-type and static imports, and wildcard imports) and never
//! invent targets they cannot name. Constructors are recorded as methods
//! named `constructor` (the JavaScript adapter shape), so `new X()` call
//! candidates resolve against them.

use std::collections::HashMap;
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
    #[error("failed to load the Tree-sitter Java grammar: {0}")]
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

struct CallTarget<'tree> {
    form: CallForm,
    target_kind: CallTargetKind,
    name: String,
    qualifier: Option<String>,
    receiver_hint: Option<String>,
    location: Node<'tree>,
}

/// An `extends`/`implements` relation with ordered syntactic resolution
/// candidates (nested class, same package, then import targets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedRelationDraft {
    pub from: usize,
    pub candidates: Vec<String>,
    pub target_kinds: Vec<SymbolKind>,
    pub kind: EdgeKind,
}

/// Import bindings resolvable from the source text: `import a.b.C` binds the
/// type name `C`, `import static a.b.C.m` binds the member name `m`;
/// wildcard imports contribute package/type prefixes heritage and scoped
/// calls resolve against without enumerating members.
#[derive(Debug, Clone, Default)]
struct Imports {
    named: HashMap<String, String>,
    static_members: HashMap<String, String>,
    type_wildcards: Vec<String>,
    static_wildcards: Vec<String>,
}

#[derive(Debug, Clone)]
struct Context {
    prefix: Vec<String>,
    container: Option<String>,
    parent: Option<usize>,
}

#[derive(Debug)]
struct Extraction<'a> {
    path: RepoRelativePath,
    source: &'a str,
    line_starts: Vec<usize>,
    package: Vec<String>,
    imports: Imports,
    symbols: Vec<SymbolDraft>,
    calls: Vec<CallDraft>,
    named_relations: Vec<NamedRelationDraft>,
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
                        language: Language::Java,
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
                            language: Language::Java,
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
            // The ellipsis is part of the response budget rather than an
            // extra character after it.
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
                language: Language::Java,
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

    fn node_name(&self, node: Node<'_>) -> Option<&str> {
        node.child_by_field_name("name")
            .and_then(|name| self.text(name))
    }

    /// Dotted type or package text (`a.b.C`) normalized to `a::b::C`.
    /// Accepts `scoped_identifier`, `scoped_type_identifier`, `generic_type`,
    /// and plain `identifier`/`type_identifier` nodes.
    fn scoped_name(&self, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "identifier" | "type_identifier" => self.text(node).map(str::to_owned),
            "scoped_identifier" => {
                let scope = node.child_by_field_name("scope")?;
                let name = node.child_by_field_name("name")?;
                Some(format!(
                    "{}::{}",
                    self.scoped_name(scope)?,
                    self.text(name)?
                ))
            }
            "scoped_type_identifier" => {
                // `a.b.C` — the trailing `type_identifier` is the name; the
                // leading nodes form the scope.
                let mut parts: Vec<String> = Vec::new();
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        "type_identifier" => parts.push(self.text(child)?.to_owned()),
                        "scoped_type_identifier" | "generic_type" => {
                            parts.push(self.scoped_name(child)?);
                        }
                        _ => {}
                    }
                }
                if parts.is_empty() {
                    return None;
                }
                Some(parts.join("::"))
            }
            "generic_type" => {
                // `List<String>` — the raw type is the first named child.
                let mut cursor = node.walk();
                let raw = node.named_children(&mut cursor).find(|child| {
                    matches!(
                        child.kind(),
                        "type_identifier" | "scoped_type_identifier" | "generic_type"
                    )
                })?;
                self.scoped_name(raw)
            }
            _ => None,
        }
    }

    /// Records one `import` declaration as an import fact named by its
    /// statement text and fills the binding maps:
    ///
    /// - `import a.b.C;` binds the type name `C` to `a::b::C`;
    /// - `import static a.b.C.m;` binds the member name `m` to
    ///   `a::b::C::m`;
    /// - `import a.b.*;` / `import static a.b.C.*;` record the fact and
    ///   contribute a wildcard prefix without enumerating members.
    fn record_import(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        if let Some(signature) = self.signature(node) {
            let name = signature.trim_end_matches(';').trim().to_owned();
            if !name.is_empty() {
                self.add_symbol(context, &name, SymbolKind::Import, node, Some(signature))?;
            }
        }
        self.collect_import_bindings(node);
        Ok(())
    }

    /// Fills the import binding maps from one `import` declaration without
    /// emitting any symbol (the hoisted pre-pass; the main visit owns the
    /// import fact).
    fn collect_import_bindings(&mut self, node: Node<'_>) {
        let is_static = {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .any(|child| !child.is_named() && self.text(child) == Some("static"))
        };
        let mut cursor = node.walk();
        let name_node = node
            .named_children(&mut cursor)
            .find(|child| matches!(child.kind(), "identifier" | "scoped_identifier"));
        let wildcard = {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .any(|child| child.kind() == "asterisk")
        };
        let Some(name_node) = name_node else {
            return;
        };
        let Some(qualified) = self.scoped_name(name_node) else {
            return;
        };
        if wildcard {
            if is_static {
                self.imports.static_wildcards.push(qualified);
            } else {
                self.imports.type_wildcards.push(qualified);
            }
            return;
        }
        let Some((container, simple)) = qualified.rsplit_once("::") else {
            return;
        };
        if is_static {
            self.imports
                .static_members
                .insert(simple.to_owned(), qualified);
        } else {
            self.imports
                .named
                .insert(simple.to_owned(), format!("{container}::{simple}"));
        }
    }

    /// A declaration carries `@Test` when its modifiers contain a marker or
    /// valued annotation whose simple name is `Test` (JUnit 4 and JUnit 5
    /// share the annotation name).
    fn has_test_annotation(&self, node: Node<'_>) -> bool {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .filter(|child| child.kind() == "modifiers")
            .flat_map(|modifiers| {
                let mut cursor = modifiers.walk();
                modifiers
                    .named_children(&mut cursor)
                    .filter(|child| matches!(child.kind(), "marker_annotation" | "annotation"))
                    .collect::<Vec<_>>()
                    .into_iter()
            })
            .any(|annotation| {
                annotation
                    .child_by_field_name("name")
                    .and_then(|name| self.scoped_name(name))
                    .is_some_and(|name| name.rsplit("::").next() == Some("Test"))
            })
    }

    fn visit(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        match node.kind() {
            "class_declaration" => self.visit_type(node, context, SymbolKind::Class),
            "interface_declaration" => self.visit_type(node, context, SymbolKind::Interface),
            "enum_declaration" => self.visit_type(node, context, SymbolKind::Enum),
            // Records and annotation types are class-shaped containers.
            "record_declaration" => self.visit_type(node, context, SymbolKind::Class),
            "annotation_type_declaration" => self.visit_type(node, context, SymbolKind::Interface),
            "import_declaration" => self.record_import(node, context),
            _ => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.visit(child, context)?;
                }
                Ok(())
            }
        }
    }

    fn visit_type(
        &mut self,
        node: Node<'_>,
        context: &Context,
        kind: SymbolKind,
    ) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let parent = self.add_symbol(context, &name, kind, node, self.signature(node))?;
        self.collect_heritage(node, parent, context);
        let Some(body) = node.child_by_field_name("body") else {
            return Ok(());
        };
        let child_context = Context {
            prefix: {
                let mut prefix = context.prefix.clone();
                prefix.push(name.clone());
                prefix
            },
            container: Some(name),
            parent: Some(parent),
        };
        self.visit_type_body(body, &child_context)
    }

    fn visit_type_body(&mut self, body: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "method_declaration" | "annotation_type_element_declaration" => {
                    self.visit_method(child, context)?;
                }
                "constructor_declaration" | "compact_constructor_declaration" => {
                    self.visit_constructor(child, context)?;
                }
                "field_declaration" | "constant_declaration" => {
                    self.visit_field(child, context)?;
                }
                "enum_constant" => self.visit_enum_constant(child, context)?,
                // Static/instance initializer blocks own no symbol; their
                // calls stay unattributed rather than guessed.
                _ => self.visit(child, context)?,
            }
        }
        Ok(())
    }

    fn visit_method(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let kind = if self.has_test_annotation(node) {
            SymbolKind::Test
        } else {
            SymbolKind::Method
        };
        let caller = self.add_symbol(context, &name, kind, node, self.signature(node))?;
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(body, caller, context.container.as_deref())?;
            // Local classes/interfaces declared inside the body nest under
            // the method.
            let mut prefix = context.prefix.clone();
            prefix.push(name.clone());
            self.visit(
                body,
                &Context {
                    container: Some(name),
                    prefix,
                    parent: Some(caller),
                },
            )?;
        }
        Ok(())
    }

    /// Constructors are recorded as methods named `constructor` inside the
    /// class container, so `new X()` candidates resolve against them.
    fn visit_constructor(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let caller = self.add_symbol(
            context,
            "constructor",
            SymbolKind::Method,
            node,
            self.signature(node),
        )?;
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(body, caller, context.container.as_deref())?;
        }
        Ok(())
    }

    fn visit_field(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        for declarator in node.named_children(&mut cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let Some(name) = declarator
                .child_by_field_name("name")
                .and_then(|name| self.text(name))
                .map(str::to_owned)
            else {
                continue;
            };
            self.add_symbol(
                context,
                &name,
                SymbolKind::Field,
                declarator,
                self.signature(declarator),
            )?;
        }
        Ok(())
    }

    fn visit_enum_constant(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let parent = self.add_symbol(
            context,
            &name,
            SymbolKind::Constant,
            node,
            self.signature(node),
        )?;
        // An enum constant with a class body declares members of its own.
        if let Some(body) = node.child_by_field_name("body") {
            let child_context = Context {
                prefix: {
                    let mut prefix = context.prefix.clone();
                    prefix.push(name.clone());
                    prefix
                },
                container: Some(name),
                parent: Some(parent),
            };
            self.visit_type_body(body, &child_context)?;
        }
        Ok(())
    }

    /// Java heritage: `class A extends B implements I` (class targets),
    /// `interface A extends B` (interface targets), `enum E implements I`,
    /// and `record R implements I`.
    fn collect_heritage(&mut self, node: Node<'_>, from: usize, context: &Context) {
        let (extends_kind, implements_kind) = match node.kind() {
            "interface_declaration" | "annotation_type_declaration" => {
                (SymbolKind::Interface, SymbolKind::Interface)
            }
            _ => (SymbolKind::Class, SymbolKind::Interface),
        };
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let (edge, target_kinds) = match (node.kind(), child.kind()) {
                (_, "superclass") => (EdgeKind::Extends, vec![extends_kind]),
                ("interface_declaration", "extends_interfaces") => {
                    (EdgeKind::Extends, vec![implements_kind])
                }
                (
                    "class_declaration" | "enum_declaration" | "record_declaration",
                    "super_interfaces",
                ) => (EdgeKind::Implements, vec![implements_kind]),
                _ => continue,
            };
            let mut types = child.walk();
            for type_node in child.named_children(&mut types) {
                let mut targets = type_node.walk();
                let targets: Vec<Node<'_>> = if type_node.kind() == "type_list" {
                    type_node.named_children(&mut targets).collect()
                } else {
                    vec![type_node]
                };
                for target in targets {
                    let Some(name) = self.scoped_name(target) else {
                        continue;
                    };
                    let candidates = self.heritage_candidates(&name, &context.prefix);
                    if candidates.is_empty() {
                        continue;
                    }
                    self.named_relations.push(NamedRelationDraft {
                        from,
                        candidates,
                        target_kinds: target_kinds.clone(),
                        kind: edge,
                    });
                }
            }
        }
    }

    /// Ordered resolution candidates for a heritage name: the nested-class
    /// scope first, then the package scope, then single-type import targets,
    /// then wildcard-import prefixes. Scoped names resolve only as written.
    fn heritage_candidates(&self, name: &str, prefix: &[String]) -> Vec<String> {
        let mut candidates = Vec::new();
        if name.contains("::") {
            candidates.push(name.to_owned());
            return candidates;
        }
        let nested = Self::qualified(prefix, name);
        candidates.push(nested);
        let package = Self::qualified(&self.package, name);
        if !candidates.contains(&package) {
            candidates.push(package);
        }
        if let Some(target) = self.imports.named.get(name) {
            candidates.push(target.clone());
        }
        for wildcard in &self.imports.type_wildcards {
            candidates.push(format!("{wildcard}::{name}"));
        }
        candidates
    }

    fn collect_calls(
        &mut self,
        node: Node<'_>,
        caller: usize,
        current_container: Option<&str>,
    ) -> Result<(), ParseError> {
        // Nested type declarations own their calls and are visited
        // separately; walking through them here would attribute their calls
        // to the enclosing callable.
        match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "class_body"
            | "import_declaration"
            | "lambda_expression" => return Ok(()),
            _ => {}
        }
        if node.kind() == "method_invocation"
            && let Some(target) = self.call_target(node, current_container)
        {
            self.calls.push(CallDraft {
                caller,
                form: target.form,
                target_kind: target.target_kind,
                name: target.name,
                qualifier: target.qualifier,
                receiver_hint: target.receiver_hint,
                location: self.range(target.location)?,
            });
        }
        if node.kind() == "object_creation_expression"
            && let Some(type_node) = node.child_by_field_name("type")
            && let Some(name) = self.scoped_name(type_node)
        {
            let qualifier = name.rsplit("::").next().map(str::to_owned);
            self.calls.push(CallDraft {
                caller,
                form: CallForm::Scoped,
                target_kind: CallTargetKind::Method,
                name: "constructor".to_owned(),
                qualifier,
                receiver_hint: self.text(type_node).and_then(bounded_receiver_hint),
                location: self.range(type_node)?,
            });
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect_calls(child, caller, current_container)?;
        }
        Ok(())
    }

    fn call_target<'tree>(
        &self,
        invocation: Node<'tree>,
        current_container: Option<&str>,
    ) -> Option<CallTarget<'tree>> {
        let name_node = invocation.child_by_field_name("name")?;
        let name = self.text(name_node)?.to_owned();
        let Some(object) = invocation.child_by_field_name("object") else {
            // A bare `name(...)` call: a static-import binding wins; the
            // call otherwise stays a simple-name candidate that resolves
            // only when the method name is unique in this revision.
            if let Some(target) = self.imports.static_members.get(&name) {
                let (qualifier, simple) = target.rsplit_once("::")?;
                return Some(CallTarget {
                    form: CallForm::Scoped,
                    target_kind: CallTargetKind::Method,
                    name: simple.to_owned(),
                    qualifier: Some(qualifier.to_owned()),
                    receiver_hint: Some(name),
                    location: name_node,
                });
            }
            return Some(CallTarget {
                form: CallForm::Function,
                target_kind: CallTargetKind::Method,
                name,
                qualifier: None,
                receiver_hint: None,
                location: name_node,
            });
        };
        match object.kind() {
            "this" => Some(CallTarget {
                form: CallForm::Member,
                target_kind: CallTargetKind::Method,
                name,
                qualifier: current_container.map(str::to_owned),
                receiver_hint: Some("this".to_owned()),
                location: name_node,
            }),
            "identifier" => {
                let object_name = self.text(object)?;
                if let Some(type_target) = self.imports.named.get(object_name) {
                    // A single-type import binds the class; static-style
                    // calls through it qualify against the imported type.
                    return Some(CallTarget {
                        form: CallForm::Scoped,
                        target_kind: CallTargetKind::Method,
                        name,
                        qualifier: Some(type_target.clone()),
                        receiver_hint: Some(object_name.to_owned()),
                        location: name_node,
                    });
                }
                if looks_like_type_name(object_name) {
                    return Some(CallTarget {
                        form: CallForm::Scoped,
                        target_kind: CallTargetKind::Method,
                        name,
                        qualifier: Some(object_name.to_owned()),
                        receiver_hint: Some(object_name.to_owned()),
                        location: name_node,
                    });
                }
                Some(CallTarget {
                    form: CallForm::Member,
                    target_kind: CallTargetKind::Method,
                    name,
                    qualifier: None,
                    receiver_hint: Some(object_name.to_owned()),
                    location: name_node,
                })
            }
            _ => Some(CallTarget {
                form: CallForm::Member,
                target_kind: CallTargetKind::Method,
                name,
                qualifier: None,
                receiver_hint: self.text(object).and_then(bounded_receiver_hint),
                location: name_node,
            }),
        }
    }
}

fn identifier_tokens(raw: &str) -> impl Iterator<Item = &str> {
    raw.split(|character: char| {
        !(character.is_alphanumeric() || character == '_' || character == '$')
    })
    .filter(|part| !part.is_empty())
}

fn bounded_receiver_hint(raw: &str) -> Option<String> {
    let hint = identifier_tokens(raw)
        .filter(|token| {
            token
                .chars()
                .next()
                .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '$')
        })
        .last()?;
    (hint.chars().count() <= MAX_RECEIVER_HINT_CHARS).then(|| hint.to_owned())
}

fn looks_like_type_name(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// Module path of a Java source: the `package` declaration when present,
/// otherwise the repository path with the conventional
/// `src/main/java`/`src/test/java` roots stripped.
pub(crate) fn module_path(path: &RepoRelativePath, package: Option<&str>) -> Vec<String> {
    if let Some(package) = package {
        let components: Vec<String> = package
            .split('.')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect();
        if !components.is_empty() {
            return components;
        }
    }
    let mut components: Vec<&str> = path.as_str().split('/').collect();
    let file = components.pop().unwrap_or_default();
    let stem = file.strip_suffix(".java").unwrap_or(file);
    for root in [
        ["src", "main", "java"].as_slice(),
        ["src", "test", "java"].as_slice(),
    ] {
        if let Some(offset) = components
            .windows(root.len())
            .position(|window| window == root)
        {
            components.drain(..offset + root.len());
            break;
        }
    }
    let mut module: Vec<String> = components.into_iter().map(str::to_owned).collect();
    module.push(stem.to_owned());
    module
}

pub(crate) struct JavaParser {
    parser: Parser,
}

impl JavaParser {
    pub(crate) fn new() -> Result<Self, ParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
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
        let mut line_starts = vec![0];
        line_starts.extend(
            source
                .match_indices('\n')
                .map(|(offset, _)| offset.saturating_add(1)),
        );
        let mut extraction = Extraction {
            path: path.clone(),
            source: source.as_ref(),
            line_starts,
            package: Vec::new(),
            imports: Imports::default(),
            symbols: Vec::new(),
            calls: Vec::new(),
            named_relations: Vec::new(),
        };
        // The package declaration and the import bindings are collected
        // before the main visit so heritage and call qualifiers see every
        // top-level import regardless of order (imports hoist in Java).
        let mut package_name = None;
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            match child.kind() {
                "package_declaration" => {
                    let mut names = child.walk();
                    package_name = child
                        .named_children(&mut names)
                        .find(|name| matches!(name.kind(), "identifier" | "scoped_identifier"))
                        .and_then(|name| extraction.scoped_name(name));
                }
                "import_declaration" => {
                    extraction.collect_import_bindings(child);
                }
                _ => {}
            }
        }
        extraction.package = package_name
            .as_deref()
            .map(|name| {
                name.split("::")
                    .filter(|segment| !segment.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let module_path = module_path(&path, package_name.as_deref());
        let context = Context {
            container: module_path.last().cloned(),
            prefix: module_path.clone(),
            parent: None,
        };
        extraction.visit(root, &context)?;
        let (diagnostics, diagnostic_count) = extraction.diagnostics(root)?;
        let Extraction {
            symbols,
            calls,
            named_relations,
            ..
        } = extraction;
        Ok(ParsedFile {
            source,
            module_path,
            symbols,
            calls,
            named_relations,
            has_errors: root.has_error(),
            diagnostics,
            diagnostic_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_module_paths_from_java_layout() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            module_path(
                &RepoRelativePath::new("src/main/java/chakra/payments/Service.java")?,
                Some("chakra.payments"),
            ),
            ["chakra", "payments"]
        );
        assert_eq!(
            module_path(
                &RepoRelativePath::new("src/main/java/chakra/payments/Service.java")?,
                None,
            ),
            ["chakra", "payments", "Service"]
        );
        assert_eq!(
            module_path(
                &RepoRelativePath::new("src/test/java/chakra/payments/FlowTest.java")?,
                None,
            ),
            ["chakra", "payments", "FlowTest"]
        );
        assert_eq!(
            module_path(
                &RepoRelativePath::new(
                    "service/src/main/java/chakra/payments/PaymentService.java",
                )?,
                None,
            ),
            ["chakra", "payments", "PaymentService"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("Legacy.java")?, None),
            ["Legacy"]
        );
        Ok(())
    }

    #[test]
    fn parses_junit_test_hints_and_static_imports() -> Result<(), Box<dyn std::error::Error>> {
        let mut parser = JavaParser::new().map_err(|error| error.to_string())?;
        let parsed = parser
            .parse(
                RepoRelativePath::new("src/test/java/chakra/payments/FlowTest.java")?,
                "package chakra.payments;\n\
                 import static chakra.payments.shared.Shared.recordEvent;\n\
                 import org.junit.jupiter.api.Test;\n\
                 class FlowTest {\n\
                 \x20   @Test void flow_marks_the_test_hint() { recordEvent(\"flow\"); }\n\
                 \x20   void helper() {}\n\
                 }\n",
            )
            .map_err(|error| error.to_string())?;
        assert!(!parsed.has_errors);
        assert!(parsed.symbols.iter().any(|symbol| symbol.key.qualified_name
            == "chakra::payments::FlowTest::flow_marks_the_test_hint"
            && symbol.key.kind == SymbolKind::Test));
        assert!(parsed.symbols.iter().any(|symbol| symbol.key.qualified_name
            == "chakra::payments::FlowTest::helper"
            && symbol.key.kind == SymbolKind::Method));
        let call = parsed
            .calls
            .iter()
            .find(|call| call.name == "recordEvent")
            .ok_or("static-import call missing")?;
        assert_eq!(
            call.qualifier.as_deref(),
            Some("chakra::payments::shared::Shared")
        );
        assert_eq!(call.form, CallForm::Scoped);
        Ok(())
    }

    #[test]
    fn parses_heritage_records_and_annotation_types() -> Result<(), Box<dyn std::error::Error>> {
        let mut parser = JavaParser::new().map_err(|error| error.to_string())?;
        let parsed = parser
            .parse(
                RepoRelativePath::new("src/main/java/chakra/shapes/Shapes.java")?,
                "package chakra.shapes;\n\
                 import chakra.base.*;\n\
                 public class Base {}\n\
                 class Derived extends Base implements AutoCloseable {\n\
                 \x20   public void close() {}\n\
                 \x20   class Inner {}\n\
                 }\n\
                 record Point(int x, int y) implements Comparable<Point> {\n\
                 \x20   public int compareTo(Point other) { return 0; }\n\
                 }\n\
                 @interface Marker {\n\
                 \x20   String value() default \"\";\n\
                 }\n",
            )
            .map_err(|error| error.to_string())?;
        assert!(!parsed.has_errors);
        let has = |name: &str, kind: SymbolKind| {
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.key.qualified_name == name && symbol.key.kind == kind)
        };
        assert!(has("chakra::shapes::Derived", SymbolKind::Class));
        assert!(has("chakra::shapes::Derived::Inner", SymbolKind::Class));
        assert!(has("chakra::shapes::Point", SymbolKind::Class));
        assert!(has("chakra::shapes::Point::compareTo", SymbolKind::Method));
        assert!(has("chakra::shapes::Marker", SymbolKind::Interface));
        assert!(has("chakra::shapes::Marker::value", SymbolKind::Method));
        let extends = parsed
            .named_relations
            .iter()
            .find(|relation| relation.kind == EdgeKind::Extends)
            .ok_or("extends relation missing")?;
        assert!(
            extends
                .candidates
                .contains(&"chakra::shapes::Base".to_owned()),
            "same-package extends candidate missing: {:?}",
            extends.candidates
        );
        let implements = parsed
            .named_relations
            .iter()
            .filter(|relation| relation.kind == EdgeKind::Implements)
            .count();
        assert_eq!(implements, 2);
        assert!(
            parsed
                .named_relations
                .iter()
                .any(|relation| relation.kind == EdgeKind::Implements
                    && relation
                        .candidates
                        .contains(&"chakra::base::AutoCloseable".to_owned())),
            "wildcard-import implements candidate missing"
        );
        Ok(())
    }

    #[test]
    fn anonymous_class_calls_are_not_attributed_to_the_enclosing_method()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut parser = JavaParser::new().map_err(|error| error.to_string())?;
        let parsed = parser
            .parse(
                RepoRelativePath::new("src/main/java/chakra/Service.java")?,
                "package chakra;\n\
                 class Service {\n\
                 \x20   void outer() {\n\
                 \x20       new Runnable() {\n\
                 \x20           public void run() { innerOnly(); }\n\
                 \x20       };\n\
                 \x20       outerOnly();\n\
                 \x20   }\n\
                 }\n",
            )
            .map_err(|error| error.to_string())?;
        assert!(!parsed.has_errors);
        let outer = parsed
            .symbols
            .iter()
            .position(|symbol| symbol.key.qualified_name == "chakra::Service::outer")
            .ok_or("outer method missing")?;
        let names: Vec<&str> = parsed
            .calls
            .iter()
            .filter(|call| call.caller == outer)
            .map(|call| call.name.as_str())
            .collect();
        assert_eq!(names, ["constructor", "outerOnly"]);
        assert!(!parsed.calls.iter().any(|call| call.name == "innerOnly"));
        Ok(())
    }
}
