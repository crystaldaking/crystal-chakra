//! Tree-sitter Go extraction into language-neutral Chakra drafts.
//!
//! The syntax tier records file/package containers, imports and build
//! constraints, named types, struct fields, interface methods, functions,
//! methods, constants, variables, Go test entry points, diagnostics, and
//! bounded static call candidates. Type-directed dispatch remains gopls's
//! responsibility.

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
    #[error("failed to load the Tree-sitter Go grammar: {0}")]
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
    local_types: HashMap<String, usize>,
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
                        language: Language::Go,
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
                            language: Language::Go,
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
                language: Language::Go,
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

    fn record_build_constraints(&mut self, root: Node<'_>) -> Result<(), ParseError> {
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() != "comment" {
                continue;
            }
            let Some(raw) = self.text(child).map(str::trim) else {
                continue;
            };
            let expression = raw
                .strip_prefix("//go:build")
                .or_else(|| raw.strip_prefix("// +build"))
                .map(str::trim);
            let Some(expression) = expression.filter(|value| !value.is_empty()) else {
                continue;
            };
            self.add_symbol(
                &format!("build::{expression}"),
                None,
                SymbolKind::Import,
                child,
                None,
            )?;
        }
        Ok(())
    }

    fn record_imports(&mut self, node: Node<'_>) -> Result<(), ParseError> {
        if node.kind() == "import_spec" {
            let Some(path_node) = node.child_by_field_name("path") else {
                return Ok(());
            };
            let Some(path) = self.text(path_node).map(trim_go_string) else {
                return Ok(());
            };
            let alias = node
                .child_by_field_name("name")
                .and_then(|name| self.text(name))
                .unwrap_or("");
            let name = if alias.is_empty() {
                format!("import::{:04}::{path}", self.import_ordinal)
            } else {
                format!("import::{:04}::{alias}::{path}", self.import_ordinal)
            };
            self.import_ordinal += 1;
            self.add_symbol(&name, None, SymbolKind::Import, node, None)?;
            return Ok(());
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.record_imports(child)?;
        }
        Ok(())
    }

    fn collect_types(&mut self, root: Node<'_>) -> Result<(), ParseError> {
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() != "type_declaration" {
                continue;
            }
            let mut declaration = child.walk();
            for spec in child.named_children(&mut declaration) {
                if !matches!(spec.kind(), "type_spec" | "type_alias") {
                    continue;
                }
                let Some(name_node) = spec.child_by_field_name("name") else {
                    continue;
                };
                let Some(name) = self.text(name_node).map(str::to_owned) else {
                    continue;
                };
                let kind = if spec.kind() == "type_alias" {
                    SymbolKind::TypeAlias
                } else {
                    match spec.child_by_field_name("type").map(|node| node.kind()) {
                        Some("struct_type") => SymbolKind::Struct,
                        Some("interface_type") => SymbolKind::Interface,
                        _ => SymbolKind::TypeAlias,
                    }
                };
                let symbol = self.add_symbol(&name, None, kind, spec, None)?;
                self.local_types.insert(name.clone(), symbol);
                if let Some(body) = spec.child_by_field_name("type") {
                    match body.kind() {
                        "struct_type" => self.collect_struct_fields(body, &name, symbol)?,
                        "interface_type" => {
                            self.collect_interface_methods(body, &name, symbol)?;
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn collect_struct_fields(
        &mut self,
        node: Node<'_>,
        container: &str,
        parent: usize,
    ) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "field_declaration" {
                let mut names = child.walk();
                let explicit: Vec<Node<'_>> =
                    child.children_by_field_name("name", &mut names).collect();
                if explicit.is_empty() {
                    if let Some(type_node) = child.child_by_field_name("type")
                        && let Some(name) = terminal_type_name(self.text(type_node).unwrap_or(""))
                    {
                        self.add_symbol(
                            &name,
                            Some(container),
                            SymbolKind::Field,
                            child,
                            Some(parent),
                        )?;
                    }
                } else {
                    for name_node in explicit {
                        if let Some(name) = self.text(name_node).map(str::to_owned) {
                            self.add_symbol(
                                &name,
                                Some(container),
                                SymbolKind::Field,
                                name_node,
                                Some(parent),
                            )?;
                        }
                    }
                }
            } else {
                self.collect_struct_fields(child, container, parent)?;
            }
        }
        Ok(())
    }

    fn collect_interface_methods(
        &mut self,
        node: Node<'_>,
        container: &str,
        parent: usize,
    ) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "method_elem"
                && let Some(name_node) = child.child_by_field_name("name")
                && let Some(name) = self.text(name_node).map(str::to_owned)
            {
                self.add_symbol(
                    &name,
                    Some(container),
                    SymbolKind::Method,
                    child,
                    Some(parent),
                )?;
            } else if child.kind() != "type_elem" {
                self.collect_interface_methods(child, container, parent)?;
            }
        }
        Ok(())
    }

    fn collect_top_level_values(&mut self, root: Node<'_>) -> Result<(), ParseError> {
        let mut cursor = root.walk();
        for declaration in root.named_children(&mut cursor) {
            let kind = match declaration.kind() {
                "const_declaration" => SymbolKind::Constant,
                "var_declaration" => SymbolKind::Property,
                _ => continue,
            };
            let spec_kind = if kind == SymbolKind::Constant {
                "const_spec"
            } else {
                "var_spec"
            };
            let mut stack = vec![declaration];
            while let Some(node) = stack.pop() {
                if node.kind() == spec_kind {
                    let mut names = node.walk();
                    for name_node in node.children_by_field_name("name", &mut names) {
                        if let Some(name) = self.text(name_node).map(str::to_owned) {
                            self.add_symbol(&name, None, kind, name_node, None)?;
                        }
                    }
                    continue;
                }
                let mut children = node.walk();
                stack.extend(node.named_children(&mut children));
            }
        }
        Ok(())
    }

    fn collect_callables(&mut self, root: Node<'_>) -> Result<(), ParseError> {
        let mut cursor = root.walk();
        for node in root.named_children(&mut cursor) {
            match node.kind() {
                "function_declaration" => self.collect_function(node)?,
                "method_declaration" => self.collect_method(node)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn collect_function(&mut self, node: Node<'_>) -> Result<(), ParseError> {
        let Some(name_node) = node.child_by_field_name("name") else {
            return Ok(());
        };
        let Some(name) = self.text(name_node).map(str::to_owned) else {
            return Ok(());
        };
        let kind = if is_go_test(&self.path, &name) {
            SymbolKind::Test
        } else {
            SymbolKind::Function
        };
        let caller = self.add_symbol(&name, None, kind, node, None)?;
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(body, caller)?;
        }
        Ok(())
    }

    fn collect_method(&mut self, node: Node<'_>) -> Result<(), ParseError> {
        let Some(name_node) = node.child_by_field_name("name") else {
            return Ok(());
        };
        let Some(name) = self.text(name_node).map(str::to_owned) else {
            return Ok(());
        };
        let receiver = node
            .child_by_field_name("receiver")
            .and_then(|receiver| receiver_type_name(receiver, self.source))
            .unwrap_or_else(|| "<receiver>".to_owned());
        let parent = self.local_types.get(&receiver).copied();
        let caller = self.add_symbol(&name, Some(&receiver), SymbolKind::Method, node, parent)?;
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(body, caller)?;
        }
        Ok(())
    }

    fn collect_calls(&mut self, node: Node<'_>, caller: usize) -> Result<(), ParseError> {
        if node.kind() == "func_literal" {
            return Ok(());
        }
        if node.kind() == "call_expression"
            && let Some(function) = node.child_by_field_name("function")
            && let Some(call) = self.call_target(function, caller)?
        {
            self.calls.push(call);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect_calls(child, caller)?;
        }
        Ok(())
    }

    fn call_target(
        &self,
        function: Node<'_>,
        caller: usize,
    ) -> Result<Option<CallDraft>, ParseError> {
        let function = unwrap_generic_function(function);
        match function.kind() {
            "identifier" => {
                let Some(name) = self.text(function).map(str::to_owned) else {
                    return Ok(None);
                };
                Ok(Some(CallDraft {
                    caller,
                    form: CallForm::Function,
                    target_kind: if is_test_name(&name) {
                        CallTargetKind::Test
                    } else {
                        CallTargetKind::Function
                    },
                    name,
                    qualifier: None,
                    receiver_hint: None,
                    location: self.range(function)?,
                }))
            }
            "selector_expression" => {
                let Some(field) = function.child_by_field_name("field") else {
                    return Ok(None);
                };
                let Some(operand) = function.child_by_field_name("operand") else {
                    return Ok(None);
                };
                let Some(name) = self.text(field).map(str::to_owned) else {
                    return Ok(None);
                };
                let receiver_hint = self.text(operand).and_then(bounded_receiver_hint);
                let qualifier = receiver_hint
                    .as_deref()
                    .and_then(terminal_type_name)
                    .filter(|value| value.chars().next().is_some_and(char::is_uppercase));
                Ok(Some(CallDraft {
                    caller,
                    form: CallForm::Member,
                    target_kind: CallTargetKind::Method,
                    name,
                    qualifier,
                    receiver_hint,
                    location: self.range(field)?,
                }))
            }
            _ => Ok(None),
        }
    }
}

fn trim_go_string(raw: &str) -> String {
    raw.trim_matches(['"', '`']).to_owned()
}

fn terminal_type_name(raw: &str) -> Option<String> {
    let raw = raw
        .find('[')
        .filter(|index| *index > 0)
        .and_then(|index| raw.get(..index))
        .unwrap_or(raw);
    raw.split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|part| !part.is_empty())
        .rfind(|part| {
            !matches!(
                *part,
                "struct" | "interface" | "map" | "chan" | "func" | "any"
            )
        })
        .map(str::to_owned)
}

fn receiver_type_name(receiver: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = receiver.walk();
    let declaration = receiver
        .named_children(&mut cursor)
        .find(|child| child.kind() == "parameter_declaration")?;
    let mut type_node = declaration.child_by_field_name("type")?;
    loop {
        match type_node.kind() {
            "type_identifier" => return source.get(type_node.byte_range()).map(str::to_owned),
            "qualified_type" => {
                let name = type_node.child_by_field_name("name")?;
                return source.get(name.byte_range()).map(str::to_owned);
            }
            "generic_type" => type_node = type_node.child_by_field_name("type")?,
            _ => {
                let mut children = type_node.walk();
                type_node = type_node.named_children(&mut children).next()?;
            }
        }
    }
}

fn bounded_receiver_hint(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(raw.chars().take(MAX_RECEIVER_HINT_CHARS).collect())
}

fn unwrap_generic_function(mut node: Node<'_>) -> Node<'_> {
    while matches!(node.kind(), "generic_type" | "index_expression") {
        let next = node
            .child_by_field_name("type")
            .or_else(|| node.child_by_field_name("operand"));
        let Some(next) = next else {
            break;
        };
        node = next;
    }
    node
}

fn is_test_name(name: &str) -> bool {
    ["Test", "Benchmark", "Fuzz", "Example"]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix).is_some_and(|tail| {
                tail.is_empty() || !tail.chars().next().is_some_and(char::is_lowercase)
            })
        })
}

fn is_go_test(path: &RepoRelativePath, name: &str) -> bool {
    path.as_str().ends_with("_test.go") && is_test_name(name)
}

pub(crate) fn module_path(path: &RepoRelativePath, package: &str) -> Vec<String> {
    let stem = path
        .as_str()
        .rsplit('/')
        .next()
        .and_then(|file| file.strip_suffix(".go"))
        .filter(|stem| !stem.is_empty())
        .unwrap_or("source");
    vec![package.to_owned(), stem.to_owned()]
}

fn package_name(root: Node<'_>, source: &str) -> String {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|child| child.kind() == "package_clause")
        .and_then(|clause| {
            let mut children = clause.walk();
            clause
                .named_children(&mut children)
                .find(|child| child.kind() == "package_identifier")
        })
        .and_then(|node| source.get(node.byte_range()))
        .unwrap_or("package")
        .to_owned()
}

pub(crate) struct GoParser {
    parser: Parser,
}

impl GoParser {
    pub(crate) fn new() -> Result<Self, ParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
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
            local_types: HashMap::new(),
            import_ordinal: 0,
        };
        extraction.symbols.push(SymbolDraft {
            key: SymbolKey {
                language: Language::Go,
                qualified_name: module_path.join("::"),
                container: Some(package.clone()),
                kind: SymbolKind::Module,
                path: extraction.path.clone(),
            },
            location: extraction.range(root)?,
            signature: Some(format!("package {package}")),
            parent: None,
        });
        extraction.record_build_constraints(root)?;
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() == "import_declaration" {
                extraction.record_imports(child)?;
            }
        }
        extraction.collect_types(root)?;
        extraction.collect_top_level_values(root)?;
        extraction.collect_callables(root)?;
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
    fn parses_packages_generics_imports_types_tests_and_calls()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = RepoRelativePath::new("internal/payment/service_test.go")?;
        let source: Arc<str> = Arc::from(
            r#"//go:build linux
package payment

import (
    "context"
    alias "example.com/shared"
)

const DefaultLimit = 10
var Enabled = true

type Service[T any] struct { Client T; embedded }
type Refunder interface { Refund(context.Context) error }
type Alias = Service[string]

func normalize(value int) int { return value }
func TestRefund(t *testing.T) { normalize(1) }
func (s *Service[T]) Refund(ctx context.Context) error { return s.Client.Pay(ctx) }
"#,
        );
        let parsed = GoParser::new()?.parse(path, source)?;
        assert!(!parsed.has_errors, "diagnostics: {:?}", parsed.diagnostics);
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.kind == SymbolKind::Struct && simple_name(symbol) == "Service"
        }));
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.kind == SymbolKind::Interface && simple_name(symbol) == "Refunder"
        }));
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.kind == SymbolKind::Method
                && symbol.key.qualified_name == "payment::Service::Refund"
        }));
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.kind == SymbolKind::Test && simple_name(symbol) == "TestRefund"
        }));
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.kind == SymbolKind::Import
                && symbol.key.qualified_name.contains("build::linux")
        }));
        assert_eq!(
            parsed
                .symbols
                .iter()
                .filter(|symbol| symbol.key.kind == SymbolKind::Import)
                .count(),
            3
        );
        assert!(parsed.calls.iter().any(|call| call.name == "normalize"));
        assert!(parsed.calls.iter().any(|call| call.name == "Pay"));
        Ok(())
    }

    #[test]
    fn malformed_go_retains_valid_symbols_and_reports_diagnostics()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = RepoRelativePath::new("service.go")?;
        let parsed = GoParser::new()?.parse(
            path,
            Arc::<str>::from(
                "package service\nfunc retained() int { return 1 }\ntype Broken struct {\n",
            ),
        )?;
        assert!(parsed.has_errors);
        assert!(parsed.diagnostic_count > 0);
        assert!(parsed.diagnostics.len() <= MAX_SYNTAX_DIAGNOSTICS_PER_FILE);
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| simple_name(symbol) == "retained")
        );
        Ok(())
    }

    #[test]
    fn go_test_hints_follow_the_non_lowercase_suffix_rule() {
        for name in [
            "TestRefund",
            "Test_Refund",
            "Benchmark1",
            "Fuzz",
            "ExampleAPI",
        ] {
            assert!(is_test_name(name), "{name}");
        }
        for name in ["Testrefund", "Benchmarksmall", "Fuzzcase", "Exampleapi"] {
            assert!(!is_test_name(name), "{name}");
        }
    }
}
