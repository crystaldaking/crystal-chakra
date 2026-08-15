//! Tree-sitter Rust extraction into language-neutral Chakra drafts.

use std::num::TryFromIntError;

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::symbol::{Language, SymbolKey, SymbolKind};
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

#[derive(Debug)]
pub(crate) struct ParsedFile {
    pub path: RepoRelativePath,
    pub source: String,
    pub module_path: Vec<String>,
    pub symbols: Vec<SymbolDraft>,
    pub calls: Vec<CallDraft>,
    pub implementations: Vec<ImplDraft>,
    pub has_errors: bool,
}

#[derive(Debug)]
pub(crate) struct SymbolDraft {
    pub key: SymbolKey,
    pub location: SourceRange,
    pub signature: Option<String>,
    pub parent: Option<usize>,
}

#[derive(Debug)]
pub(crate) struct CallDraft {
    pub caller: usize,
    pub name: String,
    pub qualifier: Option<String>,
    pub location: SourceRange,
}

#[derive(Debug)]
pub(crate) struct ImplDraft {
    pub symbol: usize,
    pub target_type: String,
    pub trait_name: Option<String>,
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
            self.collect_calls(body, caller)?;
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
        let Some(target_text) = self.text(target_node) else {
            return Ok(());
        };
        let target_type = base_type_identifier(target_text)
            .unwrap_or("unknown")
            .to_owned();
        let trait_name = node
            .child_by_field_name("trait")
            .and_then(|item| self.text(item))
            .and_then(last_identifier)
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
            target_type: target_type.clone(),
            trait_name,
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
        let Some(raw) = self.text(node) else {
            return Ok(());
        };
        let import = raw
            .trim()
            .strip_prefix("use")
            .unwrap_or(raw)
            .trim()
            .trim_end_matches(';')
            .trim();
        if import.is_empty() {
            return Ok(());
        }
        let name = format!("use {import}");
        self.add_symbol(
            context,
            &name,
            SymbolKind::Import,
            node,
            Some(raw.trim().to_owned()),
        )?;
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

    fn collect_calls(&mut self, node: Node<'_>, caller: usize) -> Result<(), ParseError> {
        if node.kind() == "call_expression"
            && let Some(function) = node.child_by_field_name("function")
            && let Some((name, qualifier, location_node)) = self.call_target(function)
        {
            self.calls.push(CallDraft {
                caller,
                name,
                qualifier,
                location: self.range(location_node)?,
            });
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect_calls(child, caller)?;
        }
        Ok(())
    }

    fn call_target<'tree>(
        &self,
        function: Node<'tree>,
    ) -> Option<(String, Option<String>, Node<'tree>)> {
        match function.kind() {
            "identifier" => Some((self.text(function)?.to_owned(), None, function)),
            "scoped_identifier" | "scoped_type_identifier" => {
                let name_node = function.child_by_field_name("name")?;
                let path = function.child_by_field_name("path")?;
                Some((
                    self.text(name_node)?.to_owned(),
                    self.text(path).and_then(last_identifier).map(str::to_owned),
                    name_node,
                ))
            }
            "field_expression" => {
                let name_node = function.child_by_field_name("field")?;
                let value = function.child_by_field_name("value")?;
                Some((
                    self.text(name_node)?.to_owned(),
                    self.text(value)
                        .and_then(last_identifier)
                        .map(str::to_owned),
                    name_node,
                ))
            }
            "generic_function" => function
                .child_by_field_name("function")
                .and_then(|inner| self.call_target(inner)),
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
        source: String,
    ) -> Result<ParsedFile, ParseError> {
        let tree = self
            .parser
            .parse(&source, None)
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
            source: &source,
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
            path,
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
}
