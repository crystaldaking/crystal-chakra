//! Tree-sitter TypeScript/TSX extraction into language-neutral Chakra drafts.
//!
//! Grammar coverage follows ADR-0027: `.ts`/`.mts`/`.cts` sources parse with
//! the TypeScript grammar, `.tsx` sources with the TSX grammar. Extraction is
//! deliberately syntactic: module resolution only follows relative import
//! specifiers, and heritage/import resolution never invents targets it cannot
//! name from the source text.

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
    #[error("failed to load the Tree-sitter TypeScript grammar: {0}")]
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
/// candidates (same-container first, then relative-import targets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedRelationDraft {
    pub from: usize,
    pub candidates: Vec<String>,
    pub target_kinds: Vec<SymbolKind>,
    pub kind: EdgeKind,
}

/// Import aliases resolvable from relative specifiers: a named alias maps to
/// the qualified target symbol, a namespace alias to the qualified module.
#[derive(Debug, Clone, Default)]
struct Imports {
    named: HashMap<String, String>,
    namespaces: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct Context {
    prefix: Vec<String>,
    container: Option<String>,
    parent: Option<usize>,
    method_container: bool,
}

#[derive(Debug)]
struct Extraction<'a> {
    path: RepoRelativePath,
    source: &'a str,
    line_starts: Vec<usize>,
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
                        language: Language::TypeScript,
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
                            language: Language::TypeScript,
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
                language: Language::TypeScript,
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

    /// Resolves a relative module specifier to a Chakra module path
    /// (repository-relative, `src`-stripped, index-collapsed). Package
    /// specifiers are not resolvable syntactically and return `None`.
    fn resolve_specifier(&self, specifier: &str) -> Option<Vec<String>> {
        if !specifier.starts_with('.') {
            return None;
        }
        let mut components: Vec<&str> = self.path.as_str().split('/').collect();
        components.pop();
        for segment in specifier.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    components.pop()?;
                }
                name => components.push(name),
            }
        }
        let file = components.pop()?;
        let stem = file
            .strip_suffix(".js")
            .or_else(|| file.strip_suffix(".mjs"))
            .or_else(|| file.strip_suffix(".cjs"))
            .or_else(|| file.strip_suffix(".jsx"))
            .or_else(|| file.strip_suffix(".ts"))
            .or_else(|| file.strip_suffix(".tsx"))
            .or_else(|| file.strip_suffix(".mts"))
            .or_else(|| file.strip_suffix(".cts"))
            .unwrap_or(file);
        let stem = stem.strip_suffix(".d").unwrap_or(stem);
        let mut module: Vec<String> = components.into_iter().map(str::to_owned).collect();
        if module.first().is_some_and(|first| first == "src") {
            module.remove(0);
        }
        if stem != "index" {
            module.push(stem.to_owned());
        }
        Some(module)
    }

    fn record_import(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        if let Some(signature) = self.signature(node) {
            let name = signature.trim_end_matches(';').trim().to_owned();
            if !name.is_empty() {
                self.add_symbol(context, &name, SymbolKind::Import, node, Some(signature))?;
            }
        }
        self.collect_import_aliases(node);
        Ok(())
    }

    /// Fills the import alias maps from one `import` statement (or an
    /// `export ... from` re-export) without emitting any symbol.
    fn collect_import_aliases(&mut self, node: Node<'_>) {
        let Some(source_node) = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() == "string")
        else {
            return;
        };
        let Some(specifier) = self
            .text(source_node)
            .map(|raw| raw.trim_matches(|character| character == '"' || character == '\''))
        else {
            return;
        };
        let Some(module) = self.resolve_specifier(specifier) else {
            return;
        };
        let Some(clause) = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() == "import_clause")
        else {
            return;
        };
        let mut cursor = clause.walk();
        for child in clause.named_children(&mut cursor) {
            match child.kind() {
                "named_imports" => {
                    let mut specifiers = child.walk();
                    for specifier in child.named_children(&mut specifiers) {
                        if specifier.kind() != "import_specifier" {
                            continue;
                        }
                        let Some(imported) = specifier
                            .child_by_field_name("name")
                            .and_then(|name| self.text(name))
                        else {
                            continue;
                        };
                        let alias = specifier
                            .child_by_field_name("alias")
                            .and_then(|alias| self.text(alias))
                            .unwrap_or(imported);
                        self.imports
                            .named
                            .insert(alias.to_owned(), Self::qualified(&module, imported));
                    }
                }
                "namespace_import" => {
                    if let Some(alias) = child
                        .named_children(&mut child.walk())
                        .find(|name| name.kind() == "identifier")
                        .and_then(|name| self.text(name))
                    {
                        self.imports
                            .namespaces
                            .insert(alias.to_owned(), module.join("::"));
                    }
                }
                _ => {}
            }
        }
    }

    fn visit(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        match node.kind() {
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                self.visit_function(node, context)
            }
            "method_definition" | "abstract_method_signature" | "method_signature" => {
                self.visit_method(node, context)
            }
            "class_declaration" | "abstract_class_declaration" => self.visit_class(node, context),
            "interface_declaration" => self.visit_interface(node, context),
            "type_alias_declaration" => self.visit_simple(node, context, SymbolKind::TypeAlias),
            "enum_declaration" => self.visit_enum(node, context),
            "internal_module" => self.visit_module(node, context),
            "import_statement" => self.record_import(node, context),
            "export_statement" => self.visit_export(node, context),
            "lexical_declaration" | "variable_declaration" => self.visit_variables(node, context),
            "call_expression"
                if test_callee(node.child_by_field_name("function"), self.source).is_some() =>
            {
                self.visit_test_block(node, context)
            }
            _ => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.visit(child, context)?;
                }
                Ok(())
            }
        }
    }

    fn visit_simple(
        &mut self,
        node: Node<'_>,
        context: &Context,
        kind: SymbolKind,
    ) -> Result<(), ParseError> {
        if let Some(name) = self.node_name(node).map(str::to_owned) {
            self.add_symbol(context, &name, kind, node, self.signature(node))?;
        }
        Ok(())
    }

    fn visit_export(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        let mut visited_declaration = false;
        for child in &children {
            match child.kind() {
                "function_declaration"
                | "generator_function_declaration"
                | "function_signature"
                | "method_definition"
                | "class_declaration"
                | "abstract_class_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "internal_module"
                | "lexical_declaration"
                | "variable_declaration" => {
                    visited_declaration = true;
                    self.visit(*child, context)?;
                }
                _ => {}
            }
        }
        if !visited_declaration && children.iter().any(|child| child.kind() == "string") {
            // `export ... from "..."` re-export: recorded as a module
            // relation fact with the same shape as an import.
            self.record_import(node, context)?;
        }
        Ok(())
    }

    fn visit_function(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let kind = if context.method_container {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };
        let caller = self.add_symbol(context, &name, kind, node, self.signature(node))?;
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
                    method_container: false,
                },
            )?;
        }
        Ok(())
    }

    fn visit_method(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let caller = self.add_symbol(
            context,
            &name,
            SymbolKind::Method,
            node,
            self.signature(node),
        )?;
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(body, caller, context.container.as_deref())?;
        }
        Ok(())
    }

    fn visit_class(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let parent = self.add_symbol(
            context,
            &name,
            SymbolKind::Class,
            node,
            self.signature(node),
        )?;
        self.collect_heritage(node, parent, context, &[SymbolKind::Class]);
        let Some(body) = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() == "class_body")
        else {
            return Ok(());
        };
        let mut child_context = context.clone();
        child_context.prefix.push(name.clone());
        child_context.container = Some(name);
        child_context.parent = Some(parent);
        child_context.method_container = true;
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "public_field_definition" | "property_signature" => {
                    if let Some(field_name) = self.node_name(child).map(str::to_owned) {
                        self.add_symbol(
                            &child_context,
                            &field_name,
                            SymbolKind::Property,
                            child,
                            self.signature(child),
                        )?;
                    }
                }
                _ => self.visit(child, &child_context)?,
            }
        }
        Ok(())
    }

    fn visit_interface(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let parent = self.add_symbol(
            context,
            &name,
            SymbolKind::Interface,
            node,
            self.signature(node),
        )?;
        self.collect_heritage(node, parent, context, &[SymbolKind::Interface]);
        let Some(body) = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() == "interface_body")
        else {
            return Ok(());
        };
        let mut child_context = context.clone();
        child_context.prefix.push(name.clone());
        child_context.container = Some(name);
        child_context.parent = Some(parent);
        child_context.method_container = true;
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "method_signature" | "abstract_method_signature" => {
                    self.visit_method(child, &child_context)?;
                }
                "property_signature" => {
                    if let Some(field_name) = self.node_name(child).map(str::to_owned) {
                        self.add_symbol(
                            &child_context,
                            &field_name,
                            SymbolKind::Property,
                            child,
                            self.signature(child),
                        )?;
                    }
                }
                _ => self.visit(child, &child_context)?,
            }
        }
        Ok(())
    }

    fn collect_heritage(
        &mut self,
        node: Node<'_>,
        from: usize,
        context: &Context,
        interface_kinds: &[SymbolKind],
    ) {
        let is_interface = interface_kinds.contains(&SymbolKind::Interface)
            && !interface_kinds.contains(&SymbolKind::Class);
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() != "class_heritage" {
                continue;
            }
            let mut clauses = child.walk();
            for clause in child.named_children(&mut clauses) {
                let (kind, target_kinds): (EdgeKind, &[SymbolKind]) = match clause.kind() {
                    "extends_clause" if is_interface => (EdgeKind::Extends, interface_kinds),
                    "extends_clause" => (EdgeKind::Extends, &[SymbolKind::Class]),
                    "implements_clause" => (EdgeKind::Implements, &[SymbolKind::Interface]),
                    _ => continue,
                };
                let mut targets = clause.walk();
                for target in clause.named_children(&mut targets) {
                    let Some(name) = self.heritage_name(target) else {
                        continue;
                    };
                    let candidates = self.heritage_candidates(&name, &context.prefix);
                    if candidates.is_empty() {
                        continue;
                    }
                    self.named_relations.push(NamedRelationDraft {
                        from,
                        candidates,
                        target_kinds: target_kinds.to_vec(),
                        kind,
                    });
                }
            }
        }
    }

    /// Syntactic name of a heritage type: plain, generic (`Base<T>`), or
    /// dotted (`ns.Base`, normalized to `ns::Base`).
    fn heritage_name(&self, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "identifier" | "type_identifier" => self.text(node).map(str::to_owned),
            "generic_type" => node
                .child_by_field_name("name")
                .and_then(|name| self.heritage_name(name)),
            "member_expression" => {
                let object = node.child_by_field_name("object")?;
                let property = node.child_by_field_name("property")?;
                let object = self.heritage_name(object)?;
                let property = self.text(property)?;
                Some(format!("{object}::{property}"))
            }
            _ => None,
        }
    }

    /// Ordered resolution candidates for a heritage name: the containing
    /// module prefix first, then aliases recorded from relative imports
    /// (named or namespace).
    fn heritage_candidates(&self, name: &str, prefix: &[String]) -> Vec<String> {
        let mut candidates = Vec::new();
        if !name.contains("::") {
            candidates.push(Self::qualified(prefix, name));
        }
        if let Some((namespace, member)) = name.rsplit_once("::") {
            if let Some(module) = self.imports.namespaces.get(namespace) {
                candidates.push(format!("{module}::{member}"));
            }
        } else if let Some(target) = self.imports.named.get(name) {
            candidates.push(target.clone());
        }
        candidates
    }

    fn visit_enum(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let parent =
            self.add_symbol(context, &name, SymbolKind::Enum, node, self.signature(node))?;
        let Some(body) = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() == "enum_body")
        else {
            return Ok(());
        };
        let mut child_context = context.clone();
        child_context.prefix.push(name.clone());
        child_context.container = Some(name);
        child_context.parent = Some(parent);
        child_context.method_container = false;
        let mut cursor = body.walk();
        for member in body.named_children(&mut cursor) {
            let member_name = match member.kind() {
                "property_identifier" => self.text(member),
                "enum_assignment" => member
                    .child_by_field_name("name")
                    .and_then(|name| self.text(name)),
                _ => None,
            };
            if let Some(member_name) = member_name.map(str::to_owned) {
                self.add_symbol(
                    &child_context,
                    &member_name,
                    SymbolKind::Constant,
                    member,
                    self.signature(member),
                )?;
            }
        }
        Ok(())
    }

    fn visit_module(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let name_node = node.child_by_field_name("name");
        let raw_name = name_node.and_then(|name| match name.kind() {
            "string" => self
                .text(name)
                .map(|raw| raw.trim_matches(|character| character == '"' || character == '\'')),
            _ => self.text(name),
        });
        let Some(raw_name) = raw_name else {
            return Ok(());
        };
        let segments: Vec<String> = raw_name
            .split('.')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect();
        if segments.is_empty() {
            return Ok(());
        }
        let display = segments.join("::");
        let parent = self.add_symbol(
            context,
            &display,
            SymbolKind::Module,
            node,
            self.signature(node),
        )?;
        let Some(body) = node.child_by_field_name("body") else {
            return Ok(());
        };
        let mut child_context = context.clone();
        child_context.prefix.extend(segments);
        child_context.container = Some(display);
        child_context.parent = Some(parent);
        child_context.method_container = false;
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            self.visit(child, &child_context)?;
        }
        Ok(())
    }

    fn visit_variables(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        for declarator in node.named_children(&mut cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = declarator.child_by_field_name("name") else {
                continue;
            };
            if name_node.kind() != "identifier" {
                // Destructuring declarations bind no single name.
                continue;
            }
            let Some(name) = self.text(name_node).map(str::to_owned) else {
                continue;
            };
            let value = declarator.child_by_field_name("value");
            let function_value = value.filter(|value| {
                matches!(
                    value.kind(),
                    "arrow_function" | "function_expression" | "generator_function_expression"
                )
            });
            if let Some(function) = function_value {
                let caller = self.add_symbol(
                    context,
                    &name,
                    if context.method_container {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    },
                    declarator,
                    self.signature(declarator),
                )?;
                if let Some(body) = function.child_by_field_name("body") {
                    self.collect_calls(body, caller, context.container.as_deref())?;
                    let mut prefix = context.prefix.clone();
                    prefix.push(name.clone());
                    self.visit(
                        body,
                        &Context {
                            container: Some(name),
                            prefix,
                            parent: Some(caller),
                            method_container: false,
                        },
                    )?;
                }
            } else {
                self.add_symbol(
                    context,
                    &name,
                    SymbolKind::Constant,
                    declarator,
                    self.signature(declarator),
                )?;
            }
        }
        Ok(())
    }

    /// jest/vitest/mocha test blocks: `it`/`test` become test-kind symbols
    /// named by their title; `describe` only groups and owns no symbol.
    fn visit_test_block(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(kind) = test_callee(node.child_by_field_name("function"), self.source) else {
            return Ok(());
        };
        let Some(arguments) = node.child_by_field_name("arguments") else {
            return Ok(());
        };
        let mut cursor = arguments.walk();
        let argument_nodes: Vec<_> = arguments.named_children(&mut cursor).collect();
        let title = argument_nodes
            .iter()
            .find(|argument| argument.kind() == "string")
            .and_then(|title| self.text(*title))
            .map(|raw| raw.trim_matches(|character| character == '"' || character == '\''))
            .map(str::to_owned);
        let callback = argument_nodes
            .iter()
            .find(|argument| matches!(argument.kind(), "arrow_function" | "function_expression"));
        match kind {
            TestCallee::Describe => {
                if let Some(body) =
                    callback.and_then(|callback| callback.child_by_field_name("body"))
                {
                    self.visit(body, context)?;
                }
            }
            TestCallee::Test => {
                let name = title.unwrap_or_else(|| "test".to_owned());
                let caller =
                    self.add_symbol(context, &name, SymbolKind::Test, node, self.signature(node))?;
                if let Some(body) =
                    callback.and_then(|callback| callback.child_by_field_name("body"))
                {
                    self.collect_calls(body, caller, context.container.as_deref())?;
                    self.visit(
                        body,
                        &Context {
                            container: context.container.clone(),
                            prefix: context.prefix.clone(),
                            parent: Some(caller),
                            method_container: false,
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    fn collect_calls(
        &mut self,
        node: Node<'_>,
        caller: usize,
        current_container: Option<&str>,
    ) -> Result<(), ParseError> {
        // Nested declarations own their calls and are visited separately;
        // walking through them here would attribute their calls to the
        // enclosing callable.
        match node.kind() {
            "function_declaration"
            | "generator_function_declaration"
            | "function_signature"
            | "method_definition"
            | "abstract_method_signature"
            | "class_declaration"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "internal_module"
            | "import_statement" => return Ok(()),
            "variable_declarator" => {
                let is_function_value = node.child_by_field_name("value").is_some_and(|value| {
                    matches!(
                        value.kind(),
                        "arrow_function" | "function_expression" | "generator_function_expression"
                    )
                });
                if is_function_value {
                    return Ok(());
                }
            }
            "call_expression"
                if test_callee(node.child_by_field_name("function"), self.source).is_some() =>
            {
                return Ok(());
            }
            _ => {}
        }
        if node.kind() == "call_expression"
            && let Some(function) = node.child_by_field_name("function")
            && let Some(target) = self.call_target(function, current_container)
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
        if node.kind() == "new_expression"
            && let Some(constructor) = node.child_by_field_name("constructor")
            && let Some(name) = self.heritage_name(constructor)
        {
            let qualifier = name.rsplit("::").next().map(str::to_owned);
            self.calls.push(CallDraft {
                caller,
                form: CallForm::Scoped,
                target_kind: CallTargetKind::Method,
                name: "constructor".to_owned(),
                qualifier,
                receiver_hint: self.text(constructor).and_then(bounded_receiver_hint),
                location: self.range(constructor)?,
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
        function: Node<'tree>,
        current_container: Option<&str>,
    ) -> Option<CallTarget<'tree>> {
        match function.kind() {
            "identifier" => {
                let name = self.text(function)?.to_owned();
                if let Some(target) = self.imports.named.get(&name) {
                    // A named import binds the alias to one qualified target;
                    // calls through it resolve against that module.
                    let (qualifier, simple) = target.rsplit_once("::")?;
                    return Some(CallTarget {
                        form: CallForm::Scoped,
                        target_kind: CallTargetKind::Function,
                        name: simple.to_owned(),
                        qualifier: Some(qualifier.to_owned()),
                        receiver_hint: Some(name),
                        location: function,
                    });
                }
                Some(CallTarget {
                    form: CallForm::Function,
                    target_kind: CallTargetKind::Function,
                    name,
                    qualifier: None,
                    receiver_hint: None,
                    location: function,
                })
            }
            "member_expression" => {
                let name_node = function.child_by_field_name("property")?;
                let object = function.child_by_field_name("object")?;
                let name = self.text(name_node)?.to_owned();
                if object.kind() == "this" {
                    return Some(CallTarget {
                        form: CallForm::Member,
                        target_kind: CallTargetKind::Method,
                        name,
                        qualifier: current_container.map(str::to_owned),
                        receiver_hint: Some("this".to_owned()),
                        location: name_node,
                    });
                }
                if object.kind() == "identifier"
                    && let Some(object_name) = self.text(object)
                {
                    if let Some(module) = self.imports.namespaces.get(object_name) {
                        return Some(CallTarget {
                            form: CallForm::Scoped,
                            target_kind: CallTargetKind::Function,
                            name,
                            qualifier: Some(module.clone()),
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
                }
                Some(CallTarget {
                    form: CallForm::Member,
                    target_kind: CallTargetKind::Method,
                    name,
                    qualifier: None,
                    receiver_hint: self.text(object).and_then(bounded_receiver_hint),
                    location: name_node,
                })
            }
            "await_expression" | "parenthesized_expression" => function
                .named_child(0)
                .and_then(|inner| self.call_target(inner, current_container)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestCallee {
    Describe,
    Test,
}

/// Recognizes jest/vitest/mocha suite and test callees, including the
/// `.only`/`.skip`/`.todo` member variants.
fn test_callee(function: Option<Node<'_>>, source: &str) -> Option<TestCallee> {
    let function = function?;
    let name = match function.kind() {
        "identifier" => source.get(function.byte_range())?,
        "member_expression" => {
            let object = function.child_by_field_name("object")?;
            if object.kind() != "identifier" {
                return None;
            }
            source.get(object.byte_range())?
        }
        _ => return None,
    };
    match name {
        "describe" => Some(TestCallee::Describe),
        "it" | "test" => Some(TestCallee::Test),
        _ => None,
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

/// Module path of a TypeScript source: `src` collapsed, `index` files
/// represented by their directory, test/declaration suffixes normalized away.
pub(crate) fn module_path(path: &RepoRelativePath) -> Vec<String> {
    let mut components: Vec<&str> = path.as_str().split('/').collect();
    let file = components.pop().unwrap_or_default();
    let mut stem = file;
    for suffix in [".tsx", ".mts", ".cts", ".ts"] {
        if let Some(stripped) = stem.strip_suffix(suffix) {
            stem = stripped;
            break;
        }
    }
    stem = stem.strip_suffix(".d").unwrap_or(stem);
    stem = stem.strip_suffix(".test").unwrap_or(stem);
    stem = stem.strip_suffix(".spec").unwrap_or(stem);

    if components.first() == Some(&"src") {
        components.remove(0);
    }
    let mut module: Vec<String> = components.into_iter().map(str::to_owned).collect();
    if stem != "index" {
        module.push(stem.to_owned());
    }
    module
}

pub(crate) struct TypeScriptParser {
    typescript: Parser,
    tsx: Parser,
}

impl TypeScriptParser {
    pub(crate) fn new() -> Result<Self, ParseError> {
        let mut typescript = Parser::new();
        typescript
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .map_err(|error| ParseError::Language(error.to_string()))?;
        let mut tsx = Parser::new();
        tsx.set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
            .map_err(|error| ParseError::Language(error.to_string()))?;
        Ok(Self { typescript, tsx })
    }

    pub(crate) fn parse(
        &mut self,
        path: RepoRelativePath,
        source: impl Into<Arc<str>>,
    ) -> Result<ParsedFile, ParseError> {
        let source = source.into();
        let parser = if path.as_str().ends_with(".tsx") {
            &mut self.tsx
        } else {
            &mut self.typescript
        };
        let tree = parser
            .parse(source.as_ref(), None)
            .ok_or_else(|| ParseError::NoTree(path.clone()))?;
        let root = tree.root_node();
        let module_path = module_path(&path);
        let mut line_starts = vec![0];
        line_starts.extend(
            source
                .match_indices('\n')
                .map(|(offset, _)| offset.saturating_add(1)),
        );
        let context = Context {
            container: module_path.last().cloned(),
            prefix: module_path.clone(),
            parent: None,
            method_container: false,
        };
        let mut extraction = Extraction {
            path: path.clone(),
            source: source.as_ref(),
            line_starts,
            imports: Imports::default(),
            symbols: Vec::new(),
            calls: Vec::new(),
            named_relations: Vec::new(),
        };
        // Import aliases are resolved before the main visit so heritage and
        // call qualifiers see every top-level import regardless of order.
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() == "import_statement" {
                extraction.collect_import_aliases(child);
            }
        }
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
    fn derives_module_paths_from_typescript_layout() -> Result<(), Box<dyn std::error::Error>> {
        assert!(module_path(&RepoRelativePath::new("src/index.ts")?).is_empty());
        assert_eq!(
            module_path(&RepoRelativePath::new("src/service.ts")?),
            ["service"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("src/api/controller.ts")?),
            ["api", "controller"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("src/api/index.ts")?),
            ["api"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("tests/conformance_flow.ts")?),
            ["tests", "conformance_flow"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("src/util.test.ts")?),
            ["util"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("src/types.d.ts")?),
            ["types"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("src/view.tsx")?),
            ["view"]
        );
        Ok(())
    }
}
