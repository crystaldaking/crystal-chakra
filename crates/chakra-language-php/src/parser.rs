//! Tree-sitter PHP extraction into language-neutral Chakra drafts.

use std::num::TryFromIntError;
use std::sync::Arc;

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::symbol::{EdgeKind, Language, SymbolKey, SymbolKind};
use thiserror::Error;
use tree_sitter::{Node, Parser, Point};

const MAX_SIGNATURE_CHARS: usize = 512;

#[derive(Debug, Error)]
pub(crate) enum ParseError {
    #[error("failed to load the Tree-sitter PHP grammar: {0}")]
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
    pub symbols: Vec<SymbolDraft>,
    pub calls: Vec<CallDraft>,
    pub named_relations: Vec<NamedRelationDraft>,
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
    pub name: String,
    pub qualifier: Option<String>,
    pub location: SourceRange,
}

#[derive(Debug, Clone)]
pub(crate) struct NamedRelationDraft {
    pub from: usize,
    pub target: String,
    pub target_kinds: Vec<SymbolKind>,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone)]
struct Context {
    prefix: Vec<String>,
    container: Option<String>,
    parent: Option<usize>,
    method_container: bool,
    namespace_prefix: Vec<String>,
    namespace_container: Option<String>,
    namespace_parent: Option<usize>,
}

#[derive(Debug)]
struct Extraction<'a> {
    path: RepoRelativePath,
    source: &'a str,
    line_starts: Vec<usize>,
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
        let mut value = String::new();
        let mut count = 0_usize;
        let mut truncated = false;
        'words: for word in raw.split_whitespace() {
            if !value.is_empty() {
                if count == MAX_SIGNATURE_CHARS {
                    truncated = true;
                    break;
                }
                value.push(' ');
                count += 1;
            }
            for character in word.chars() {
                if count == MAX_SIGNATURE_CHARS {
                    truncated = true;
                    break 'words;
                }
                value.push(character);
                count += 1;
            }
        }
        if truncated {
            if let Some((last, _)) = value.char_indices().next_back() {
                value.truncate(last);
            }
            value.push('…');
        }
        Some(value)
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
                language: Language::Php,
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

    fn visit_sequence(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut current = context.clone();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "namespace_definition" && child.child_by_field_name("body").is_none()
            {
                current = self.visit_namespace(child, &current)?.unwrap_or(current);
            } else {
                self.visit(child, &current)?;
            }
        }
        Ok(())
    }

    fn visit(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        match node.kind() {
            "function_definition" => self.visit_callable(node, context, false),
            "method_declaration" => self.visit_callable(node, context, true),
            "class_declaration" => self.visit_container(node, context, SymbolKind::Class),
            "interface_declaration" => self.visit_container(node, context, SymbolKind::Interface),
            "trait_declaration" => self.visit_container(node, context, SymbolKind::Trait),
            "enum_declaration" => self.visit_container(node, context, SymbolKind::Enum),
            "namespace_definition" => {
                self.visit_namespace(node, context)?;
                Ok(())
            }
            "namespace_use_declaration" => self.visit_import(node, context),
            "property_declaration" => self.visit_properties(node, context),
            "const_declaration" => self.visit_constants(node, context),
            _ => self.visit_sequence(node, context),
        }
    }

    fn visit_namespace(
        &mut self,
        node: Node<'_>,
        context: &Context,
    ) -> Result<Option<Context>, ParseError> {
        let Some(raw_name) = node
            .child_by_field_name("name")
            .and_then(|name| self.text(name))
        else {
            return Ok(None);
        };
        let segments = name_segments(raw_name);
        if segments.is_empty() {
            return Ok(None);
        }
        let display = segments.join("::");
        let root = Context {
            prefix: Vec::new(),
            container: None,
            parent: None,
            method_container: false,
            namespace_prefix: Vec::new(),
            namespace_container: None,
            namespace_parent: None,
        };
        let parent = self.add_symbol(
            &root,
            &display,
            SymbolKind::Module,
            node,
            self.signature(node),
        )?;
        let child = Context {
            prefix: segments.clone(),
            container: Some(display.clone()),
            parent: Some(parent),
            method_container: false,
            namespace_prefix: segments,
            namespace_container: Some(display),
            namespace_parent: Some(parent),
        };
        if let Some(body) = node.child_by_field_name("body") {
            self.visit_sequence(body, &child)?;
            Ok(None)
        } else {
            let _ = context;
            Ok(Some(child))
        }
    }

    fn visit_container(
        &mut self,
        node: Node<'_>,
        context: &Context,
        kind: SymbolKind,
    ) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let parent = self.add_symbol(context, &name, kind, node, self.signature(node))?;
        let namespace_prefix = context.prefix.clone();
        self.collect_inheritance(node, parent, kind, &namespace_prefix);
        if let Some(body) = node.child_by_field_name("body") {
            let mut prefix = context.prefix.clone();
            prefix.push(name.clone());
            self.visit_sequence(
                body,
                &Context {
                    prefix,
                    container: Some(name),
                    parent: Some(parent),
                    method_container: true,
                    namespace_prefix: context.namespace_prefix.clone(),
                    namespace_container: context.namespace_container.clone(),
                    namespace_parent: context.namespace_parent,
                },
            )?;
        }
        Ok(())
    }

    fn collect_inheritance(
        &mut self,
        node: Node<'_>,
        from: usize,
        kind: SymbolKind,
        namespace: &[String],
    ) {
        let mut cursor = node.walk();
        for clause in node.named_children(&mut cursor) {
            let (relation, targets): (EdgeKind, &[SymbolKind]) = match clause.kind() {
                "base_clause" if kind == SymbolKind::Interface => {
                    (EdgeKind::Extends, &[SymbolKind::Interface])
                }
                "base_clause" => (EdgeKind::Extends, &[SymbolKind::Class]),
                "class_interface_clause" => (EdgeKind::Implements, &[SymbolKind::Interface]),
                _ => continue,
            };
            let mut names = clause.walk();
            for target in clause.named_children(&mut names) {
                let Some(raw) = self.text(target) else {
                    continue;
                };
                let normalized = qualified_reference(namespace, raw);
                self.named_relations.push(NamedRelationDraft {
                    from,
                    target: normalized,
                    target_kinds: targets.to_vec(),
                    kind: relation,
                });
            }
        }
    }

    fn visit_callable(
        &mut self,
        node: Node<'_>,
        context: &Context,
        method: bool,
    ) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let test = name.starts_with("test") || self.has_test_attribute(node);
        let kind = if test {
            SymbolKind::Test
        } else if method || context.method_container {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };
        let caller = self.add_symbol(context, &name, kind, node, self.signature(node))?;
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(body, caller, context.container.as_deref())?;
            // Named nested functions own their declarations and calls. Walk
            // the body a second time through the declaration visitor; it
            // does not collect ordinary expressions, so calls already
            // attributed above are not duplicated.
            // PHP nested named functions are declared in the surrounding
            // namespace, not in a lexical `outer::inner` namespace and not as
            // class methods. They still own the calls in their own bodies.
            self.visit_sequence(
                body,
                &Context {
                    prefix: context.namespace_prefix.clone(),
                    container: context.namespace_container.clone(),
                    parent: context.namespace_parent,
                    method_container: false,
                    namespace_prefix: context.namespace_prefix.clone(),
                    namespace_container: context.namespace_container.clone(),
                    namespace_parent: context.namespace_parent,
                },
            )?;
        }
        Ok(())
    }

    fn has_test_attribute(&self, node: Node<'_>) -> bool {
        node.child_by_field_name("attributes")
            .and_then(|attributes| self.text(attributes))
            .is_some_and(|text| {
                text.split(|character: char| !character.is_alphanumeric() && character != '_')
                    .any(|word| word == "Test")
            })
    }

    fn visit_import(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(signature) = self.signature(node) else {
            return Ok(());
        };
        let name = signature.trim_end_matches(';').trim().to_owned();
        if !name.is_empty() {
            self.add_symbol(context, &name, SymbolKind::Import, node, Some(signature))?;
        }
        Ok(())
    }

    fn visit_properties(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        for element in node.named_children(&mut cursor) {
            if element.kind() != "property_element" {
                continue;
            }
            let Some(name) = element
                .child_by_field_name("name")
                .and_then(|name| self.text(name))
                .map(|name| name.trim_start_matches('$').to_owned())
            else {
                continue;
            };
            self.add_symbol(
                context,
                &name,
                SymbolKind::Property,
                element,
                self.signature(node),
            )?;
        }
        Ok(())
    }

    fn visit_constants(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        for element in node.named_children(&mut cursor) {
            if element.kind() != "const_element" {
                continue;
            }
            let mut children = element.walk();
            let name = element
                .named_children(&mut children)
                .find(|child| child.kind() == "name")
                .and_then(|name| self.text(name))
                .map(str::to_owned);
            if let Some(name) = name {
                self.add_symbol(
                    context,
                    &name,
                    SymbolKind::Constant,
                    element,
                    self.signature(node),
                )?;
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
        if matches!(node.kind(), "function_definition" | "method_declaration") {
            return Ok(());
        }
        if let Some((name, qualifier, location)) = self.call_target(node, current_container) {
            self.calls.push(CallDraft {
                caller,
                name,
                qualifier,
                location: self.range(location)?,
            });
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect_calls(child, caller, current_container)?;
        }
        Ok(())
    }

    fn call_target<'a>(
        &self,
        node: Node<'a>,
        current_container: Option<&str>,
    ) -> Option<(String, Option<String>, Node<'a>)> {
        match node.kind() {
            "function_call_expression" => {
                let target = node.child_by_field_name("function")?;
                let raw = self.text(target)?;
                if !matches!(target.kind(), "name" | "qualified_name" | "relative_name") {
                    return None;
                }
                let parts = name_segments(raw);
                let name = parts.last()?.clone();
                let qualifier = (parts.len() > 1).then(|| parts[..parts.len() - 1].join("::"));
                Some((name, qualifier, target))
            }
            "member_call_expression" | "nullsafe_member_call_expression" => {
                let name_node = node.child_by_field_name("name")?;
                if name_node.kind() != "name" {
                    return None;
                }
                let name = self.text(name_node)?.to_owned();
                let object = node
                    .child_by_field_name("object")
                    .and_then(|object| self.text(object));
                let qualifier = match object {
                    Some("$this") => current_container.map(str::to_owned),
                    Some(raw) if !raw.starts_with('$') => Some(normalize_name(raw)),
                    _ => None,
                };
                Some((name, qualifier, name_node))
            }
            "scoped_call_expression" => {
                let name_node = node.child_by_field_name("name")?;
                if name_node.kind() != "name" {
                    return None;
                }
                let name = self.text(name_node)?.to_owned();
                let scope = node.child_by_field_name("scope")?;
                let raw_scope = self.text(scope)?;
                let qualifier = match raw_scope {
                    "self" | "static" => current_container.map(str::to_owned),
                    _ => Some(normalize_name(raw_scope)),
                };
                Some((name, qualifier, name_node))
            }
            _ => None,
        }
    }
}

pub(crate) struct PhpParser {
    parser: Parser,
}

impl PhpParser {
    pub fn new() -> Result<Self, ParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .map_err(|error| ParseError::Language(error.to_string()))?;
        Ok(Self { parser })
    }

    pub fn parse(
        &mut self,
        path: RepoRelativePath,
        source: Arc<str>,
    ) -> Result<ParsedFile, ParseError> {
        let tree = self
            .parser
            .parse(source.as_ref(), None)
            .ok_or_else(|| ParseError::NoTree(path.clone()))?;
        let root = tree.root_node();
        let mut line_starts = vec![0];
        line_starts.extend(
            source
                .bytes()
                .enumerate()
                .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
        );
        let mut extraction = Extraction {
            path,
            source: source.as_ref(),
            line_starts,
            symbols: Vec::new(),
            calls: Vec::new(),
            named_relations: Vec::new(),
        };
        extraction.visit_sequence(
            root,
            &Context {
                prefix: Vec::new(),
                container: None,
                parent: None,
                method_container: false,
                namespace_prefix: Vec::new(),
                namespace_container: None,
                namespace_parent: None,
            },
        )?;
        Ok(ParsedFile {
            source: source.clone(),
            symbols: extraction.symbols,
            calls: extraction.calls,
            named_relations: extraction.named_relations,
            has_errors: root.has_error(),
        })
    }
}

fn name_segments(raw: &str) -> Vec<String> {
    raw.trim_start_matches("namespace\\")
        .trim_start_matches('\\')
        .split('\\')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect()
}

fn normalize_name(raw: &str) -> String {
    name_segments(raw).join("::")
}

fn qualified_reference(namespace: &[String], raw: &str) -> String {
    let absolute = raw.starts_with('\\');
    let normalized = normalize_name(raw);
    if absolute || normalized.contains("::") || namespace.is_empty() {
        normalized
    } else {
        format!("{}::{normalized}", namespace.join("::"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_namespaced_php_symbols_calls_and_tests() -> Result<(), Box<dyn std::error::Error>> {
        let mut parser = PhpParser::new()?;
        let parsed = parser.parse(
            RepoRelativePath::new("src/PaymentService.php")?,
            Arc::from(
                r#"<?php
namespace App\Service;
use App\Provider\Provider;
interface Refundable { public function refund(int $amount): void; }
class PaymentService implements Refundable {
    private Provider $provider;
    public function refund(int $amount): void { $this->audit(); Provider::send(); helper(); }
    #[Test]
    public function refunds_payment(): void { $this->refund(10); }
    private function audit(): void {}
}
function helper(): void {}
"#,
            ),
        )?;
        assert!(!parsed.has_errors);
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.qualified_name == "App::Service::PaymentService::refund"
                && symbol.key.kind == SymbolKind::Method
        }));
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.qualified_name == "App::Service::PaymentService::refunds_payment"
                && symbol.key.kind == SymbolKind::Test
        }));
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.key.kind == SymbolKind::Property)
        );
        assert!(parsed.calls.iter().any(|call| call.name == "audit"));
        assert!(parsed.calls.iter().any(|call| call.name == "helper"));
        Ok(())
    }

    #[test]
    fn preserves_facts_during_temporary_syntax_errors() -> Result<(), Box<dyn std::error::Error>> {
        let mut parser = PhpParser::new()?;
        let parsed = parser.parse(
            RepoRelativePath::new("broken.php")?,
            Arc::from("<?php function still_visible( { return helper(); }"),
        )?;
        assert!(parsed.has_errors);
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.key.qualified_name == "still_visible")
        );
        Ok(())
    }

    #[test]
    fn converts_tree_sitter_byte_columns_to_unicode_scalar_columns()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = "<?php function caller(): void { $gem = '💎'; target(); }";
        let mut parser = PhpParser::new()?;
        let parsed = parser.parse(
            RepoRelativePath::new("src/functions.php")?,
            Arc::from(source),
        )?;
        let call = parsed
            .calls
            .iter()
            .find(|call| call.name == "target")
            .ok_or("target call missing")?;
        let byte = source.find("target").ok_or("target text missing")?;
        assert_eq!(
            call.location.start().column() as usize,
            source[..byte]
                .rsplit_once('\n')
                .map_or(&source[..byte], |(_, line)| line)
                .chars()
                .count()
                + 1
        );
        Ok(())
    }

    #[test]
    fn nested_functions_own_calls_while_closures_belong_to_the_enclosing_symbol()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = r#"<?php
namespace App;
function target(): void {}
function outer(): void {
    function inner(): void { target(); }
    $closure = function (): void { target(); };
    $arrow = fn () => target();
    inner();
}
"#;
        let mut parser = PhpParser::new()?;
        let parsed = parser.parse(
            RepoRelativePath::new("src/functions.php")?,
            Arc::from(source),
        )?;
        let outer = parsed
            .symbols
            .iter()
            .position(|symbol| symbol.key.qualified_name == "App::outer")
            .ok_or("outer symbol missing")?;
        let inner = parsed
            .symbols
            .iter()
            .position(|symbol| symbol.key.qualified_name == "App::inner")
            .ok_or("inner symbol missing")?;
        assert_ne!(parsed.symbols[inner].parent, Some(outer));
        assert!(
            parsed
                .symbols
                .iter()
                .all(|symbol| symbol.key.qualified_name != "App::outer::inner")
        );
        assert_eq!(
            parsed
                .calls
                .iter()
                .filter(|call| call.caller == inner && call.name == "target")
                .count(),
            1
        );
        assert_eq!(
            parsed
                .calls
                .iter()
                .filter(|call| call.caller == outer && call.name == "target")
                .count(),
            2
        );
        assert!(
            parsed
                .calls
                .iter()
                .any(|call| call.caller == outer && call.name == "inner")
        );
        Ok(())
    }
}
