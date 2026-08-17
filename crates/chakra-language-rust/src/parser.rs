//! Tree-sitter Rust extraction into language-neutral Chakra drafts.

use std::num::TryFromIntError;
use std::sync::Arc;

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::symbol::{
    CallForm, CallTargetKind, Language, MAX_RECEIVER_HINT_CHARS, SymbolKey, SymbolKind,
};
use thiserror::Error;
use tree_sitter::{Node, Parser, Point};

const MAX_SIGNATURE_CHARS: usize = 512;

#[derive(Debug, Error)]
pub(crate) enum ParseError {
    #[error("failed to load the Tree-sitter Rust grammar: {0}")]
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

#[derive(Debug, Clone)]
pub(crate) struct ParsedFile {
    pub source: Arc<str>,
    pub module_path: Vec<String>,
    pub symbols: Vec<SymbolDraft>,
    pub calls: Vec<CallDraft>,
    pub implementations: Vec<ImplDraft>,
    pub has_errors: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SymbolDraft {
    pub key: SymbolKey,
    pub location: SourceRange,
    pub signature: Option<String>,
    pub parent: Option<usize>,
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub(crate) struct ImplDraft {
    pub symbol: usize,
    /// Exact syntactic container prefix at the impl site, including inline
    /// modules nested inside the physical file module.
    pub module_path: Vec<String>,
    /// Same-module lookup candidate only when the target syntax is an
    /// unqualified type identifier (optionally with generic arguments).
    pub target_lookup: Option<String>,
    /// Same-module lookup candidate only when the trait syntax is an
    /// unqualified type identifier (optionally with generic arguments).
    pub trait_lookup: Option<String>,
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
    symbols: Vec<SymbolDraft>,
    calls: Vec<CallDraft>,
    implementations: Vec<ImplDraft>,
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

    fn type_name(&self, node: Node<'_>) -> Option<&str> {
        match node.kind() {
            "identifier" | "type_identifier" => self.text(node),
            "generic_type" => node
                .child_by_field_name("type")
                .and_then(|base| self.type_name(base)),
            "scoped_type_identifier" => node
                .child_by_field_name("name")
                .and_then(|name| self.text(name)),
            _ => self.text(node).and_then(base_type_identifier),
        }
    }

    fn simple_type_lookup(&self, node: Node<'_>) -> Option<&str> {
        let base = if node.kind() == "generic_type" {
            node.child_by_field_name("type")?
        } else {
            node
        };
        matches!(base.kind(), "identifier" | "type_identifier")
            .then(|| self.text(base))
            .flatten()
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
                language: Language::Rust,
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

    fn visit(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        match node.kind() {
            "function_item" | "function_signature_item" => self.visit_function(node, context),
            "struct_item" => self.visit_struct(node, context),
            "enum_item" => self.visit_simple(node, context, SymbolKind::Enum),
            "trait_item" => self.visit_trait(node, context),
            "impl_item" => self.visit_impl(node, context),
            "mod_item" => self.visit_module(node, context),
            "use_declaration" => self.visit_import(node, context),
            "const_item" | "static_item" => self.visit_simple(node, context, SymbolKind::Constant),
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

    fn visit_function(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let kind = if self.has_test_attribute(node) {
            SymbolKind::Test
        } else if context.method_container {
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

    fn visit_struct(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let parent = self.add_symbol(
            context,
            &name,
            SymbolKind::Struct,
            node,
            self.signature(node),
        )?;
        let Some(body) = node.child_by_field_name("body") else {
            return Ok(());
        };
        let mut field_context = context.clone();
        field_context.prefix.push(name.clone());
        field_context.container = Some(name);
        field_context.parent = Some(parent);
        field_context.method_container = false;
        let mut cursor = body.walk();
        for field in body.named_children(&mut cursor) {
            if field.kind() != "field_declaration" {
                continue;
            }
            let Some(field_name) = self.node_name(field).map(str::to_owned) else {
                continue;
            };
            self.add_symbol(
                &field_context,
                &field_name,
                SymbolKind::Field,
                field,
                self.signature(field),
            )?;
        }
        Ok(())
    }

    fn visit_trait(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let parent = self.add_symbol(
            context,
            &name,
            SymbolKind::Trait,
            node,
            self.signature(node),
        )?;
        if let Some(body) = node.child_by_field_name("body") {
            let mut child_context = context.clone();
            child_context.prefix.push(name.clone());
            child_context.container = Some(name);
            child_context.parent = Some(parent);
            child_context.method_container = true;
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                self.visit(child, &child_context)?;
            }
        }
        Ok(())
    }

    fn visit_impl(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(target_node) = node.child_by_field_name("type") else {
            return Ok(());
        };
        let Some(target_type) = self.type_name(target_node).map(str::to_owned) else {
            return Ok(());
        };
        let target_lookup = self.simple_type_lookup(target_node).map(str::to_owned);
        let trait_node = node.child_by_field_name("trait");
        let trait_name = trait_node
            .and_then(|item| self.type_name(item))
            .map(str::to_owned);
        let trait_lookup = trait_node
            .and_then(|item| self.simple_type_lookup(item))
            .map(str::to_owned);
        let label = trait_name.as_ref().map_or_else(
            || format!("<impl {target_type}>"),
            |trait_name| format!("<impl {trait_name} for {target_type}>"),
        );
        let parent = self.add_symbol(
            context,
            &label,
            SymbolKind::ImplBlock,
            node,
            self.signature(node),
        )?;
        self.implementations.push(ImplDraft {
            symbol: parent,
            module_path: context.prefix.clone(),
            target_lookup,
            trait_lookup,
        });
        if let Some(body) = node.child_by_field_name("body") {
            let mut child_context = context.clone();
            child_context.prefix.push(target_type.clone());
            child_context.container = Some(target_type);
            child_context.parent = Some(parent);
            child_context.method_container = true;
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                self.visit(child, &child_context)?;
            }
        }
        Ok(())
    }

    fn visit_module(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let parent = self.add_symbol(
            context,
            &name,
            SymbolKind::Module,
            node,
            self.signature(node),
        )?;
        if let Some(body) = node.child_by_field_name("body") {
            let mut child_context = context.clone();
            child_context.prefix.push(name.clone());
            child_context.container = Some(name);
            child_context.parent = Some(parent);
            child_context.method_container = false;
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                self.visit(child, &child_context)?;
            }
        }
        Ok(())
    }

    fn visit_import(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(signature) = self.signature(node) else {
            return Ok(());
        };
        let name = signature.trim_end_matches(';').trim().to_owned();
        if name.is_empty() {
            return Ok(());
        }
        self.add_symbol(context, &name, SymbolKind::Import, node, Some(signature))?;
        Ok(())
    }

    fn has_test_attribute(&self, node: Node<'_>) -> bool {
        let mut sibling = node.prev_named_sibling();
        while let Some(attribute) = sibling {
            if attribute.kind() != "attribute_item" {
                break;
            }
            if self.text(attribute).is_some_and(is_test_attribute) {
                return true;
            }
            sibling = attribute.prev_named_sibling();
        }
        false
    }

    fn collect_calls(
        &mut self,
        node: Node<'_>,
        caller: usize,
        current_container: Option<&str>,
    ) -> Result<(), ParseError> {
        // Nested item bodies own their calls and are visited separately by
        // `visit_function`; walking through them here would attribute their
        // calls to the enclosing function.
        if matches!(node.kind(), "function_item" | "function_signature_item") {
            return Ok(());
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
            "identifier" => Some(CallTarget {
                form: CallForm::Function,
                target_kind: CallTargetKind::Function,
                name: self.text(function)?.to_owned(),
                qualifier: None,
                receiver_hint: None,
                location: function,
            }),
            "scoped_identifier" | "scoped_type_identifier" => {
                let name_node = function.child_by_field_name("name")?;
                let path = function.child_by_field_name("path")?;
                let receiver_hint = self.text(path).and_then(bounded_receiver_hint);
                let qualifier = match receiver_hint.as_deref() {
                    Some("Self") | Some("self") => current_container.map(str::to_owned),
                    _ => receiver_hint.clone(),
                };
                let target_kind = if matches!(
                    path.kind(),
                    "type_identifier" | "generic_type" | "scoped_type_identifier"
                ) || receiver_hint.as_deref().is_some_and(looks_like_type_name)
                {
                    CallTargetKind::Method
                } else {
                    CallTargetKind::Function
                };
                Some(CallTarget {
                    form: CallForm::Scoped,
                    target_kind,
                    name: self.text(name_node)?.to_owned(),
                    qualifier,
                    receiver_hint,
                    location: name_node,
                })
            }
            "field_expression" => {
                let name_node = function.child_by_field_name("field")?;
                let value = function.child_by_field_name("value")?;
                let receiver_hint = self.text(value).and_then(bounded_receiver_hint);
                let qualifier = match receiver_hint.as_deref() {
                    Some("self") => current_container.map(str::to_owned),
                    _ => None,
                };
                Some(CallTarget {
                    form: CallForm::Member,
                    target_kind: CallTargetKind::Method,
                    name: self.text(name_node)?.to_owned(),
                    qualifier,
                    receiver_hint,
                    location: name_node,
                })
            }
            "generic_function" => function
                .child_by_field_name("function")
                .and_then(|inner| self.call_target(inner, current_container)),
            _ => None,
        }
    }
}

fn is_test_attribute(raw: &str) -> bool {
    let compact: String = raw
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let Some(inner) = compact
        .strip_prefix("#[")
        .and_then(|value| value.strip_suffix(']'))
    else {
        return false;
    };
    let path = inner.split('(').next().unwrap_or(inner);
    path.rsplit("::").next() == Some("test")
}

fn last_identifier(raw: &str) -> Option<&str> {
    raw.trim_matches(|character: char| !character.is_alphanumeric() && character != '_')
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .rfind(|part| !part.is_empty())
}

fn bounded_receiver_hint(raw: &str) -> Option<String> {
    let hint = last_identifier(raw)?;
    (hint.chars().count() <= MAX_RECEIVER_HINT_CHARS).then(|| hint.to_owned())
}

fn looks_like_type_name(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

fn base_type_identifier(raw: &str) -> Option<&str> {
    last_identifier(raw.split('<').next().unwrap_or(raw))
}

pub(crate) fn module_path(path: &RepoRelativePath) -> Vec<String> {
    let mut components: Vec<&str> = path.as_str().split('/').collect();
    let file = components.pop().unwrap_or_default();
    let stem = file.strip_suffix(".rs").unwrap_or(file);

    if components.first() == Some(&"src") {
        components.remove(0);
    }
    let mut module: Vec<String> = components.into_iter().map(str::to_owned).collect();
    if !matches!(stem, "lib" | "main" | "mod") {
        module.push(stem.to_owned());
    }
    module
}

pub(crate) struct RustParser {
    parser: Parser,
}

impl RustParser {
    pub(crate) fn new() -> Result<Self, ParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
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
            symbols: Vec::new(),
            calls: Vec::new(),
            implementations: Vec::new(),
        };
        extraction.visit(root, &context)?;
        let Extraction {
            symbols,
            calls,
            implementations,
            ..
        } = extraction;
        Ok(ParsedFile {
            source,
            module_path,
            symbols,
            calls,
            implementations,
            has_errors: root.has_error(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_module_paths_from_rust_layout() -> Result<(), Box<dyn std::error::Error>> {
        assert!(module_path(&RepoRelativePath::new("src/lib.rs")?).is_empty());
        assert_eq!(
            module_path(&RepoRelativePath::new("src/api/controller.rs")?),
            ["api", "controller"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("src/provider/mod.rs")?),
            ["provider"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("tests/refund_flow.rs")?),
            ["tests", "refund_flow"]
        );
        Ok(())
    }

    #[test]
    fn extracts_declarations_containers_calls_and_test_hints()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = r#"
            use crate::Provider;
            pub struct Service { provider: Provider }
            impl Service {
                pub fn refund(&self) { self.provider.refund(); }
            }
            #[test]
            fn refund_works() { Service::refund(); }
        "#;
        let mut parser = RustParser::new()?;
        let parsed = parser.parse(RepoRelativePath::new("src/service.rs")?, source.to_owned())?;

        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.qualified_name == "service::Service" && symbol.key.kind == SymbolKind::Struct
        }));
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.qualified_name == "service::Service::provider"
                && symbol.key.kind == SymbolKind::Field
        }));
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.qualified_name == "service::Service::refund"
                && symbol.key.kind == SymbolKind::Method
        }));
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.qualified_name == "service::refund_works"
                && symbol.key.kind == SymbolKind::Test
        }));
        assert_eq!(
            parsed
                .calls
                .iter()
                .filter(|call| call.name == "refund")
                .count(),
            2
        );
        let receiver_call = parsed
            .calls
            .iter()
            .find(|call| call.form == CallForm::Member)
            .ok_or("receiver call missing")?;
        assert_eq!(receiver_call.target_kind, CallTargetKind::Method);
        assert_eq!(receiver_call.receiver_hint.as_deref(), Some("provider"));
        assert_eq!(receiver_call.qualifier, None);
        let scoped_call = parsed
            .calls
            .iter()
            .find(|call| call.form == CallForm::Scoped)
            .ok_or("scoped call missing")?;
        assert_eq!(scoped_call.target_kind, CallTargetKind::Method);
        assert_eq!(scoped_call.receiver_hint.as_deref(), Some("Service"));
        assert_eq!(scoped_call.qualifier.as_deref(), Some("Service"));
        assert!(!parsed.has_errors);
        Ok(())
    }

    #[test]
    fn retains_valid_symbols_from_a_tree_with_syntax_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut parser = RustParser::new()?;
        let parsed = parser.parse(
            RepoRelativePath::new("src/lib.rs")?,
            "pub fn valid() {}\npub fn broken( {\n".to_owned(),
        )?;
        assert!(parsed.has_errors);
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.key.qualified_name == "valid")
        );
        Ok(())
    }

    #[test]
    fn converts_tree_sitter_byte_columns_to_unicode_scalar_columns()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = "fn caller() { let gem = \"💎\"; target(); }";
        let mut parser = RustParser::new()?;
        let parsed = parser.parse(RepoRelativePath::new("src/lib.rs")?, source.to_owned())?;
        let call = parsed
            .calls
            .iter()
            .find(|call| call.name == "target")
            .ok_or("target call missing")?;
        let byte = source.find("target").ok_or("target text missing")?;
        assert_eq!(
            call.location.start().column() as usize,
            source[..byte].chars().count() + 1
        );
        Ok(())
    }

    #[test]
    fn nested_functions_own_their_symbols_and_calls() -> Result<(), Box<dyn std::error::Error>> {
        let source = "pub fn outer() { fn inner() { target(); } inner(); } pub fn target() {}";
        let mut parser = RustParser::new()?;
        let parsed = parser.parse(RepoRelativePath::new("src/lib.rs")?, source.to_owned())?;

        let outer = parsed
            .symbols
            .iter()
            .position(|symbol| symbol.key.qualified_name == "outer")
            .ok_or("outer symbol missing")?;
        let inner = parsed
            .symbols
            .iter()
            .position(|symbol| symbol.key.qualified_name == "outer::inner")
            .ok_or("inner symbol missing")?;
        assert!(
            parsed
                .calls
                .iter()
                .any(|call| call.caller == outer && call.name == "inner")
        );
        assert!(
            parsed
                .calls
                .iter()
                .any(|call| call.caller == inner && call.name == "target")
        );
        assert!(
            parsed
                .calls
                .iter()
                .all(|call| !(call.caller == outer && call.name == "target"))
        );
        Ok(())
    }

    #[test]
    fn bounds_every_signature_including_imports_and_the_ellipsis()
    -> Result<(), Box<dyn std::error::Error>> {
        let long_identifier = "a".repeat(MAX_SIGNATURE_CHARS + 100);
        let source = format!("pub use crate::{long_identifier};\nfn short() {{}}\n");
        let mut parser = RustParser::new()?;
        let parsed = parser.parse(RepoRelativePath::new("src/lib.rs")?, source)?;
        let import = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.key.kind == SymbolKind::Import)
            .ok_or("import symbol missing")?;
        let signature = import.signature.as_deref().ok_or("signature missing")?;

        assert_eq!(signature.chars().count(), MAX_SIGNATURE_CHARS);
        assert!(signature.ends_with('…'));
        assert!(import.key.qualified_name.chars().count() <= MAX_SIGNATURE_CHARS);
        Ok(())
    }

    #[test]
    fn impl_lookups_keep_simple_generic_names_but_reject_qualified_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = r#"
            trait Local<T> {}
            trait Marker {}
            struct S<T>(T);
            impl<T> Local<T> for S<T> {}
            impl std::fmt::Display for S<u8> {}
            impl Marker for std::vec::Vec<u8> {}
        "#;
        let mut parser = RustParser::new()?;
        let parsed = parser.parse(RepoRelativePath::new("src/lib.rs")?, source.to_owned())?;

        assert_eq!(parsed.implementations.len(), 3);
        assert!(parsed.implementations[0].module_path.is_empty());
        assert_eq!(
            parsed.implementations[0].target_lookup.as_deref(),
            Some("S")
        );
        assert_eq!(
            parsed.implementations[0].trait_lookup.as_deref(),
            Some("Local")
        );
        assert_eq!(
            parsed.implementations[1].target_lookup.as_deref(),
            Some("S")
        );
        assert_eq!(parsed.implementations[1].trait_lookup, None);
        assert_eq!(parsed.implementations[2].target_lookup, None);
        assert_eq!(
            parsed.implementations[2].trait_lookup.as_deref(),
            Some("Marker")
        );
        Ok(())
    }
}
