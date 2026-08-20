//! Tree-sitter C# extraction into language-neutral Chakra drafts.
//!
//! The grammar supplies syntax facts for namespaces, types, members,
//! heritage, tests, and calls. Resolution stays deliberately honest: syntax
//! candidates come from source names and `using` directives; semantic C#
//! binding remains the optional `csharp-ls` provider's responsibility.

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
    #[error("failed to load the Tree-sitter C# grammar: {0}")]
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
    pub extension_scopes: Vec<String>,
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
    pub is_extension_method: bool,
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

/// One syntax-derived base-type relation with ordered resolution candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedRelationDraft {
    pub from: usize,
    pub candidates: Vec<String>,
    pub target_kinds: Vec<SymbolKind>,
    pub kind: EdgeKind,
}

/// Bindings recoverable from C# `using` directives.
#[derive(Debug, Clone, Default)]
struct Imports {
    aliases: HashMap<String, String>,
    namespace_prefixes: Vec<String>,
    static_types: Vec<String>,
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
    namespace: Vec<String>,
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
                        language: Language::CSharp,
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
                            language: Language::CSharp,
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
            .or_else(|| node.child_by_field_name("accessors"))
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
                language: Language::CSharp,
                qualified_name: Self::qualified(&context.prefix, name),
                container: context.container.clone(),
                kind,
                path: self.path.clone(),
            },
            location: self.range(node)?,
            signature,
            parent: context.parent,
            is_extension_method: false,
        });
        Ok(index)
    }

    fn node_name(&self, node: Node<'_>) -> Option<&str> {
        node.child_by_field_name("name")
            .and_then(|name| self.text(name))
    }

    /// Normalizes a C# qualified or generic type/name to Chakra `::` form.
    fn scoped_name(&self, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "identifier" | "predefined_type" => self.text(node).map(str::to_owned),
            "generic_name" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find(|child| child.kind() == "identifier")
                    .and_then(|name| self.text(name))
                    .map(str::to_owned)
            }
            "qualified_name" => {
                let qualifier = node.child_by_field_name("qualifier")?;
                let name = node.child_by_field_name("name")?;
                Some(format!(
                    "{}::{}",
                    self.scoped_name(qualifier)?,
                    self.scoped_name(name)?
                ))
            }
            "alias_qualified_name" => {
                let alias = node.child_by_field_name("alias")?;
                let name = node.child_by_field_name("name")?;
                Some(format!(
                    "{}::{}",
                    self.scoped_name(alias)?,
                    self.scoped_name(name)?
                ))
            }
            "nullable_type" | "array_type" | "pointer_type" => node
                .child_by_field_name("type")
                .and_then(|inner| self.scoped_name(inner)),
            _ => {
                let raw = self.text(node)?.trim();
                normalize_type_text(raw)
            }
        }
    }

    fn record_using(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        if let Some(signature) = self.signature(node) {
            let name = signature.trim_end_matches(';').trim().to_owned();
            if !name.is_empty() {
                self.add_symbol(context, &name, SymbolKind::Import, node, Some(signature))?;
            }
        }
        Ok(())
    }

    fn collect_using_binding(&mut self, node: Node<'_>) {
        let Some(raw) = self.text(node) else {
            return;
        };
        let raw = raw.trim().trim_end_matches(';').trim();
        let raw = raw.strip_prefix("global ").unwrap_or(raw);
        let Some(raw) = raw.strip_prefix("using ") else {
            return;
        };
        if let Some(target) = raw.strip_prefix("static ").and_then(normalize_type_text) {
            if !self.imports.static_types.contains(&target) {
                self.imports.static_types.push(target);
            }
            return;
        }
        if let Some((alias, target)) = raw.split_once('=') {
            let alias = alias.trim();
            if let Some(target) = normalize_type_text(target.trim())
                && !alias.is_empty()
            {
                self.imports.aliases.insert(alias.to_owned(), target);
            }
            return;
        }
        if let Some(prefix) = normalize_type_text(raw)
            && !self.imports.namespace_prefixes.contains(&prefix)
        {
            self.imports.namespace_prefixes.push(prefix);
        }
    }

    fn collect_using_bindings(&mut self, node: Node<'_>) {
        if node.kind() == "using_directive" {
            self.collect_using_binding(node);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect_using_bindings(child);
        }
    }

    fn has_test_attribute(&self, node: Node<'_>) -> bool {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .filter(|child| child.kind() == "attribute_list")
            .flat_map(|list| {
                let mut cursor = list.walk();
                list.named_children(&mut cursor)
                    .filter(|child| child.kind() == "attribute")
                    .collect::<Vec<_>>()
            })
            .filter_map(|attribute| attribute.child_by_field_name("name"))
            .filter_map(|name| self.scoped_name(name))
            .filter_map(|name| name.rsplit("::").next().map(str::to_owned))
            .map(|name| name.strip_suffix("Attribute").unwrap_or(&name).to_owned())
            .any(|name| {
                matches!(
                    name.as_str(),
                    "Fact"
                        | "Theory"
                        | "Test"
                        | "TestCase"
                        | "TestCaseSource"
                        | "TestMethod"
                        | "DataTestMethod"
                )
            })
    }

    fn is_extension_method(&self, node: Node<'_>) -> bool {
        let Some(parameters) = node.child_by_field_name("parameters") else {
            return false;
        };
        let mut cursor = parameters.walk();
        parameters
            .named_children(&mut cursor)
            .find(|child| child.kind() == "parameter")
            .is_some_and(|parameter| {
                let mut cursor = parameter.walk();
                parameter.named_children(&mut cursor).any(|child| {
                    child.kind() == "modifier"
                        && self.text(child).is_some_and(|text| text.trim() == "this")
                })
            })
    }

    fn extension_scopes(&self) -> Vec<String> {
        let mut scopes = self.imports.namespace_prefixes.clone();
        let current = self.namespace.join("::");
        if !scopes.contains(&current) {
            scopes.push(current);
        }
        scopes
    }

    fn visit(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        match node.kind() {
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                self.visit_namespace(node, context)
            }
            "class_declaration" => self.visit_type(node, context, SymbolKind::Class),
            "record_declaration" => {
                let mut cursor = node.walk();
                let kind = if node
                    .children(&mut cursor)
                    .any(|child| child.kind() == "struct")
                {
                    SymbolKind::Struct
                } else {
                    SymbolKind::Class
                };
                self.visit_type(node, context, kind)
            }
            "struct_declaration" => self.visit_type(node, context, SymbolKind::Struct),
            "interface_declaration" => self.visit_type(node, context, SymbolKind::Interface),
            "enum_declaration" => self.visit_type(node, context, SymbolKind::Enum),
            "delegate_declaration" => self.visit_delegate(node, context),
            "using_directive" => self.record_using(node, context),
            "local_function_statement" => self.visit_method(node, context),
            _ => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.visit(child, context)?;
                }
                Ok(())
            }
        }
    }

    fn visit_namespace(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name_node) = node.child_by_field_name("name") else {
            return Ok(());
        };
        let Some(name) = self.scoped_name(name_node) else {
            return Ok(());
        };
        let mut prefix = context.prefix.clone();
        prefix.extend(name.split("::").map(str::to_owned));
        let namespace_context = Context {
            container: prefix.last().cloned(),
            prefix,
            parent: context.parent,
        };
        if let Some(body) = node.child_by_field_name("body") {
            return self.visit(body, &namespace_context);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.id() != name_node.id() {
                self.visit(child, &namespace_context)?;
            }
        }
        Ok(())
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
        self.collect_heritage(node, parent, context, kind);
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

    fn visit_delegate(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        self.add_symbol(
            context,
            &name,
            SymbolKind::TypeAlias,
            node,
            self.signature(node),
        )?;
        Ok(())
    }

    fn visit_type_body(&mut self, body: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "method_declaration" => self.visit_method(child, context)?,
                "constructor_declaration" => self.visit_constructor(child, context)?,
                "destructor_declaration" => {
                    self.visit_named_callable(child, context, "destructor")?
                }
                "operator_declaration" => self.visit_operator(child, context)?,
                "conversion_operator_declaration" => {
                    self.visit_named_callable(child, context, "operator")?
                }
                "property_declaration" | "event_declaration" | "indexer_declaration" => {
                    self.visit_property(child, context)?
                }
                "field_declaration" | "event_field_declaration" => {
                    self.visit_field(child, context)?
                }
                "enum_member_declaration" => self.visit_enum_member(child, context)?,
                _ => self.visit(child, context)?,
            }
        }
        Ok(())
    }

    fn visit_method(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let kind = if self.has_test_attribute(node) {
            SymbolKind::Test
        } else {
            SymbolKind::Method
        };
        let is_extension_method = self.is_extension_method(node);
        let caller = self.add_symbol(context, &name, kind, node, self.signature(node))?;
        if let Some(symbol) = self.symbols.get_mut(caller) {
            symbol.is_extension_method = is_extension_method;
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(body, caller, context.container.as_deref())?;
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

    fn visit_named_callable(
        &mut self,
        node: Node<'_>,
        context: &Context,
        name: &str,
    ) -> Result<(), ParseError> {
        let caller = self.add_symbol(
            context,
            name,
            SymbolKind::Method,
            node,
            self.signature(node),
        )?;
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(body, caller, context.container.as_deref())?;
        }
        Ok(())
    }

    fn visit_constructor(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        self.visit_named_callable(node, context, "constructor")
    }

    fn visit_operator(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let name = node
            .child_by_field_name("operator")
            .and_then(|operator| self.text(operator))
            .map_or_else(
                || "operator".to_owned(),
                |operator| format!("operator{operator}"),
            );
        self.visit_named_callable(node, context, &name)
    }

    fn visit_property(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let name = if node.kind() == "indexer_declaration" {
            "this".to_owned()
        } else {
            let Some(name) = self.node_name(node).map(str::to_owned) else {
                return Ok(());
            };
            name
        };
        let caller = self.add_symbol(
            context,
            &name,
            SymbolKind::Property,
            node,
            self.signature(node),
        )?;
        for field in ["accessors", "value"] {
            if let Some(body) = node.child_by_field_name(field) {
                self.collect_calls(body, caller, context.container.as_deref())?;
            }
        }
        Ok(())
    }

    fn visit_field(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        for declaration in node.named_children(&mut cursor) {
            if declaration.kind() != "variable_declaration" {
                continue;
            }
            let mut variables = declaration.walk();
            for declarator in declaration
                .named_children(&mut variables)
                .filter(|child| child.kind() == "variable_declarator")
            {
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
        }
        Ok(())
    }

    fn visit_enum_member(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        self.add_symbol(
            context,
            &name,
            SymbolKind::Constant,
            node,
            self.signature(node),
        )?;
        Ok(())
    }

    /// The C# colon syntax does not encode whether a class's first base is a
    /// class or interface. Syntax marks the first class/record base as an
    /// `extends` candidate and remaining bases as `implements`; the precise
    /// provider is authoritative when semantic binding is needed.
    fn collect_heritage(
        &mut self,
        node: Node<'_>,
        from: usize,
        context: &Context,
        declared_kind: SymbolKind,
    ) {
        let mut cursor = node.walk();
        let Some(base_list) = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "base_list")
        else {
            return;
        };
        let mut bases = base_list.walk();
        let targets: Vec<_> = base_list
            .named_children(&mut bases)
            .filter(|child| child.kind() != "argument_list")
            .collect();
        for (index, target) in targets.into_iter().enumerate() {
            let Some(name) = self.scoped_name(target) else {
                continue;
            };
            let candidates = self.heritage_candidates(&name, &context.prefix);
            if candidates.is_empty() {
                continue;
            }
            let (kind, target_kinds) = match declared_kind {
                SymbolKind::Interface => (EdgeKind::Extends, vec![SymbolKind::Interface]),
                SymbolKind::Struct | SymbolKind::Enum => {
                    (EdgeKind::Implements, vec![SymbolKind::Interface])
                }
                _ if index == 0 => (
                    EdgeKind::Extends,
                    vec![SymbolKind::Class, SymbolKind::Interface],
                ),
                _ => (EdgeKind::Implements, vec![SymbolKind::Interface]),
            };
            self.named_relations.push(NamedRelationDraft {
                from,
                candidates,
                target_kinds,
                kind,
            });
        }
    }

    fn heritage_candidates(&self, name: &str, prefix: &[String]) -> Vec<String> {
        let mut candidates = Vec::new();
        if name.contains("::") {
            if let Some((alias, suffix)) = name.split_once("::")
                && let Some(target) = self.imports.aliases.get(alias)
            {
                candidates.push(format!("{target}::{suffix}"));
            }
            candidates.push(name.to_owned());
            candidates.dedup();
            return candidates;
        }
        if let Some(target) = self.imports.aliases.get(name) {
            candidates.push(target.clone());
        }
        candidates.push(Self::qualified(prefix, name));
        let namespace = Self::qualified(&self.namespace, name);
        if !candidates.contains(&namespace) {
            candidates.push(namespace);
        }
        for using in &self.imports.namespace_prefixes {
            let candidate = format!("{using}::{name}");
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
        candidates
    }

    fn collect_calls(
        &mut self,
        node: Node<'_>,
        caller: usize,
        current_container: Option<&str>,
    ) -> Result<(), ParseError> {
        match node.kind() {
            "class_declaration"
            | "struct_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "declaration_list"
            | "local_function_statement"
            | "lambda_expression"
            | "anonymous_method_expression" => return Ok(()),
            _ => {}
        }
        if node.kind() == "invocation_expression"
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
        let function = invocation.child_by_field_name("function")?;
        match function.kind() {
            "identifier" | "generic_name" => {
                let name = self.scoped_name(function)?;
                let qualifier = (self.imports.static_types.len() == 1)
                    .then(|| self.imports.static_types[0].clone());
                Some(CallTarget {
                    form: if qualifier.is_some() {
                        CallForm::Scoped
                    } else {
                        CallForm::Function
                    },
                    target_kind: CallTargetKind::Method,
                    name,
                    qualifier,
                    receiver_hint: None,
                    location: function,
                })
            }
            "member_access_expression" => {
                let object = function.child_by_field_name("expression")?;
                let name_node = function.child_by_field_name("name")?;
                let name = self.scoped_name(name_node)?;
                let object_text = self.text(object)?.trim();
                if matches!(object_text, "this" | "base") {
                    return Some(CallTarget {
                        form: CallForm::Member,
                        target_kind: CallTargetKind::Method,
                        name,
                        qualifier: current_container.map(str::to_owned),
                        receiver_hint: Some(object_text.to_owned()),
                        location: name_node,
                    });
                }
                if let Some(alias) = self.imports.aliases.get(object_text) {
                    return Some(CallTarget {
                        form: CallForm::Scoped,
                        target_kind: CallTargetKind::Method,
                        name,
                        qualifier: Some(alias.clone()),
                        receiver_hint: Some(object_text.to_owned()),
                        location: name_node,
                    });
                }
                if looks_like_type_name(object_text) {
                    return Some(CallTarget {
                        form: CallForm::Scoped,
                        target_kind: CallTargetKind::Method,
                        name,
                        qualifier: self.scoped_name(object),
                        receiver_hint: bounded_receiver_hint(object_text),
                        location: name_node,
                    });
                }
                Some(CallTarget {
                    form: CallForm::Member,
                    target_kind: CallTargetKind::Method,
                    name,
                    qualifier: None,
                    receiver_hint: bounded_receiver_hint(object_text),
                    location: name_node,
                })
            }
            "qualified_name" | "alias_qualified_name" => {
                let qualified = self.scoped_name(function)?;
                let (qualifier, name) = qualified.rsplit_once("::")?;
                Some(CallTarget {
                    form: CallForm::Scoped,
                    target_kind: CallTargetKind::Method,
                    name: name.to_owned(),
                    qualifier: Some(qualifier.to_owned()),
                    receiver_hint: self.text(function).and_then(bounded_receiver_hint),
                    location: function.child_by_field_name("name").unwrap_or(function),
                })
            }
            _ => {
                let hint = self.text(function).and_then(bounded_receiver_hint)?;
                Some(CallTarget {
                    form: CallForm::Member,
                    target_kind: CallTargetKind::Method,
                    name: hint.clone(),
                    qualifier: None,
                    receiver_hint: Some(hint),
                    location: function,
                })
            }
        }
    }
}

fn normalize_type_text(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut normalized = String::new();
    let mut generic_depth = 0_u32;
    for character in raw.chars() {
        match character {
            '<' => generic_depth = generic_depth.saturating_add(1),
            '>' => generic_depth = generic_depth.saturating_sub(1),
            '?' | '[' | ']' | '*' if generic_depth == 0 => {}
            '.' if generic_depth == 0 => normalized.push_str("::"),
            ':' if generic_depth == 0 => normalized.push(':'),
            character if generic_depth == 0 && !character.is_whitespace() => {
                normalized.push(character);
            }
            _ => {}
        }
    }
    (!normalized.is_empty()).then_some(normalized)
}

fn identifier_tokens(raw: &str) -> impl Iterator<Item = &str> {
    raw.split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|part| !part.is_empty())
}

fn bounded_receiver_hint(raw: &str) -> Option<String> {
    let hint = identifier_tokens(raw)
        .filter(|token| {
            token
                .chars()
                .next()
                .is_some_and(|first| first.is_alphabetic() || first == '_')
        })
        .last()?;
    (hint.chars().count() <= MAX_RECEIVER_HINT_CHARS).then(|| hint.to_owned())
}

fn looks_like_type_name(name: &str) -> bool {
    name.rsplit(['.', ':'])
        .find(|part| !part.is_empty())
        .and_then(|part| part.chars().next())
        .is_some_and(char::is_uppercase)
}

/// Module path of a C# source: the declared namespace when present,
/// otherwise the deterministic repository path and `.cs` file stem.
pub(crate) fn module_path(path: &RepoRelativePath, namespace: Option<&str>) -> Vec<String> {
    if let Some(namespace) = namespace {
        let components: Vec<String> = namespace
            .split(['.', ':'])
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect();
        if !components.is_empty() {
            return components;
        }
    }
    let mut components: Vec<String> = path.as_str().split('/').map(str::to_owned).collect();
    if let Some(file) = components.last_mut() {
        *file = file.strip_suffix(".cs").unwrap_or(file).to_owned();
    }
    components
}

fn first_namespace(extraction: &Extraction<'_>, root: Node<'_>) -> Option<String> {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|child| {
            matches!(
                child.kind(),
                "namespace_declaration" | "file_scoped_namespace_declaration"
            )
        })
        .and_then(|namespace| namespace.child_by_field_name("name"))
        .and_then(|name| extraction.scoped_name(name))
}

fn has_file_scoped_namespace(root: Node<'_>) -> bool {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .any(|child| child.kind() == "file_scoped_namespace_declaration")
}

pub(crate) struct CSharpParser {
    parser: Parser,
}

impl CSharpParser {
    pub(crate) fn new() -> Result<Self, ParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
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
            namespace: Vec::new(),
            imports: Imports::default(),
            symbols: Vec::new(),
            calls: Vec::new(),
            named_relations: Vec::new(),
        };
        let namespace_name = first_namespace(&extraction, root);
        extraction.namespace = namespace_name
            .as_deref()
            .map(|name| name.split("::").map(str::to_owned).collect())
            .unwrap_or_default();
        extraction.collect_using_bindings(root);
        let module_path = module_path(&path, namespace_name.as_deref());
        let extension_scopes = extraction.extension_scopes();
        let root_context = if namespace_name.is_some() && !has_file_scoped_namespace(root) {
            Context {
                container: None,
                prefix: Vec::new(),
                parent: None,
            }
        } else {
            Context {
                container: module_path.last().cloned(),
                prefix: module_path.clone(),
                parent: None,
            }
        };
        extraction.visit(root, &root_context)?;
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
            extension_scopes,
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
    fn derives_module_paths_from_namespace_or_file() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            module_path(
                &RepoRelativePath::new("src/Payments/Service.cs")?,
                Some("Chakra.Payments"),
            ),
            ["Chakra", "Payments"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("src/Payments/Service.cs")?, None),
            ["src", "Payments", "Service"]
        );
        Ok(())
    }

    #[test]
    fn parses_types_members_framework_tests_and_calls() -> Result<(), Box<dyn std::error::Error>> {
        let mut parser = CSharpParser::new().map_err(|error| error.to_string())?;
        let parsed = parser
            .parse(
                RepoRelativePath::new("tests/PaymentServiceTests.cs")?,
                "using Xunit;\n\
                 using static Chakra.Shared.Events;\n\
                 namespace Chakra.Payments;\n\
                 public partial class PaymentService<T> : BaseService, IDisposable {\n\
                 \x20   private readonly T gateway;\n\
                 \x20   public string Name { get; init; }\n\
                 \x20   public PaymentService(T gateway) { this.gateway = gateway; }\n\
                 \x20   public async Task CaptureAsync() { await SaveAsync(); Record(); }\n\
                 \x20   [Fact] public void Captures_payment() { CaptureAsync(); }\n\
                 }\n\
                 public class OtherFrameworkTests {\n\
                 \x20   [TestCase(1)] public void NUnit_case(int value) {}\n\
                 \x20   [TestMethod] public void MSTest_case() {}\n\
                 }\n",
            )
            .map_err(|error| error.to_string())?;
        assert!(!parsed.has_errors, "diagnostics: {:?}", parsed.diagnostics);
        let has = |name: &str, kind: SymbolKind| {
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.key.qualified_name == name && symbol.key.kind == kind)
        };
        assert!(has("Chakra::Payments::PaymentService", SymbolKind::Class));
        assert!(has(
            "Chakra::Payments::PaymentService::gateway",
            SymbolKind::Field
        ));
        assert!(has(
            "Chakra::Payments::PaymentService::Name",
            SymbolKind::Property
        ));
        assert!(has(
            "Chakra::Payments::PaymentService::constructor",
            SymbolKind::Method
        ));
        assert!(has(
            "Chakra::Payments::PaymentService::CaptureAsync",
            SymbolKind::Method
        ));
        assert!(has(
            "Chakra::Payments::PaymentService::Captures_payment",
            SymbolKind::Test
        ));
        assert!(has(
            "Chakra::Payments::OtherFrameworkTests::NUnit_case",
            SymbolKind::Test
        ));
        assert!(has(
            "Chakra::Payments::OtherFrameworkTests::MSTest_case",
            SymbolKind::Test
        ));
        assert!(parsed.calls.iter().any(|call| call.name == "SaveAsync"));
        assert!(parsed.calls.iter().any(|call| call.name == "Record"
            && call.qualifier.as_deref() == Some("Chakra::Shared::Events")));
        assert_eq!(parsed.named_relations.len(), 2);
        Ok(())
    }

    #[test]
    fn parses_block_namespaces_structs_delegates_and_enum_members()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut parser = CSharpParser::new().map_err(|error| error.to_string())?;
        let parsed = parser
            .parse(
                RepoRelativePath::new("src/Contracts.cs")?,
                "namespace Chakra.Contracts {\n\
                 \x20   public interface ICommand { void Execute(); }\n\
                 \x20   public readonly struct Amount : IComparable<Amount> {\n\
                 \x20       public int CompareTo(Amount other) => 0;\n\
                 \x20   }\n\
                 \x20   public delegate void Handler<T>(T value);\n\
                 \x20   public enum State { Pending, Complete = 2 }\n\
                 \x20   public record class Envelope<T>(T Value);\n\
                 \x20   public readonly record struct RecordAmount(int Value) : IComparable<RecordAmount>;\n\
                 }\n",
            )
            .map_err(|error| error.to_string())?;
        assert!(!parsed.has_errors, "diagnostics: {:?}", parsed.diagnostics);
        assert!(parsed.symbols.iter().any(|symbol| symbol.key.qualified_name
            == "Chakra::Contracts::ICommand"
            && symbol.key.kind == SymbolKind::Interface));
        assert!(parsed.symbols.iter().any(|symbol| symbol.key.qualified_name
            == "Chakra::Contracts::Amount"
            && symbol.key.kind == SymbolKind::Struct));
        assert!(parsed.symbols.iter().any(|symbol| symbol.key.qualified_name
            == "Chakra::Contracts::Handler"
            && symbol.key.kind == SymbolKind::TypeAlias));
        assert!(parsed.symbols.iter().any(|symbol| symbol.key.qualified_name
            == "Chakra::Contracts::State::Complete"
            && symbol.key.kind == SymbolKind::Constant));
        assert!(parsed.symbols.iter().any(|symbol| symbol.key.qualified_name
            == "Chakra::Contracts::Envelope"
            && symbol.key.kind == SymbolKind::Class));
        assert!(parsed.symbols.iter().any(|symbol| symbol.key.qualified_name
            == "Chakra::Contracts::RecordAmount"
            && symbol.key.kind == SymbolKind::Struct));
        let record_amount = parsed
            .symbols
            .iter()
            .position(|symbol| symbol.key.qualified_name == "Chakra::Contracts::RecordAmount")
            .ok_or("record struct missing")?;
        assert!(parsed.named_relations.iter().any(|relation| {
            relation.from == record_amount
                && relation.kind == EdgeKind::Implements
                && relation.target_kinds == [SymbolKind::Interface]
        }));
        Ok(())
    }

    #[test]
    fn malformed_csharp_reports_bounded_diagnostics() -> Result<(), Box<dyn std::error::Error>> {
        let mut parser = CSharpParser::new().map_err(|error| error.to_string())?;
        let parsed = parser
            .parse(
                RepoRelativePath::new("src/Broken.cs")?,
                "namespace Chakra; class Broken { void Run( { }",
            )
            .map_err(|error| error.to_string())?;
        assert!(parsed.has_errors);
        assert!(parsed.diagnostic_count > 0);
        assert!(parsed.diagnostics.len() <= MAX_SYNTAX_DIAGNOSTICS_PER_FILE);
        Ok(())
    }
}
