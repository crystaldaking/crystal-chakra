//! Deterministic Laravel-specific syntax enrichment.
//!
//! This module intentionally sits beside, rather than inside, the generic PHP
//! parser. It is activated from Composer metadata and emits symbolic facts
//! that the indexer resolves against the ordinary PHP declaration catalog.

use std::collections::HashMap;
use std::num::TryFromIntError;

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::symbol::{EdgeKind, SymbolKind};
use thiserror::Error;
use tree_sitter::{Node, Parser, Point};

const MAX_FRAMEWORK_FACTS_PER_FILE: usize = 2_048;
const MAX_CONFIGURATION_SIGNATURE_CHARS: usize = 256;

#[derive(Debug, Error)]
pub(crate) enum LaravelParseError {
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
pub(crate) struct LaravelFile {
    pub symbols: Vec<FrameworkSymbolDraft>,
    pub relations: Vec<FrameworkRelationDraft>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct FrameworkSymbolDraft {
    pub qualified_name: String,
    pub location: SourceRange,
    pub signature: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct FrameworkRelationDraft {
    pub kind: EdgeKind,
    pub from: FrameworkEndpoint,
    pub to: FrameworkEndpoint,
    pub location: SourceRange,
}

#[derive(Debug, Clone)]
pub(crate) enum FrameworkEndpoint {
    /// Alternatives are tried in order. This is used for conventions such as
    /// listener `handle` with `__invoke` fallback without inventing both.
    Existing(Vec<FrameworkSelector>),
    Synthetic(usize),
}

#[derive(Debug, Clone)]
pub(crate) struct FrameworkSelector {
    pub qualified_name: String,
    pub kinds: Vec<SymbolKind>,
}

impl FrameworkSelector {
    fn type_named(qualified_name: String) -> Self {
        Self {
            qualified_name,
            kinds: vec![
                SymbolKind::Class,
                SymbolKind::Interface,
                SymbolKind::Trait,
                SymbolKind::Enum,
            ],
        }
    }

    fn callable(qualified_name: String, method: bool) -> Self {
        Self {
            qualified_name,
            kinds: if method {
                vec![SymbolKind::Method, SymbolKind::Test]
            } else {
                vec![SymbolKind::Function]
            },
        }
    }

    fn method(container: &str, name: &str) -> Self {
        Self::callable(format!("{container}::{name}"), true)
    }
}

#[derive(Debug, Clone, Default)]
struct Context {
    namespace: Vec<String>,
    imports: HashMap<String, String>,
    current_type: Option<FrameworkSelector>,
    current_callable: Option<FrameworkSelector>,
}

struct Extraction<'a> {
    path: RepoRelativePath,
    source: &'a str,
    line_starts: Vec<usize>,
    symbols: Vec<FrameworkSymbolDraft>,
    relations: Vec<FrameworkRelationDraft>,
    truncated: bool,
}

impl Extraction<'_> {
    fn text(&self, node: Node<'_>) -> Option<&str> {
        self.source.get(node.byte_range())
    }

    fn position(&self, point: Point) -> Result<TextPosition, LaravelParseError> {
        let line_start =
            *self
                .line_starts
                .get(point.row)
                .ok_or_else(|| LaravelParseError::InvalidPoint {
                    path: self.path.clone(),
                    row: point.row,
                    column: point.column,
                })?;
        let line_end = self.source[line_start..]
            .find('\n')
            .map_or(self.source.len(), |offset| line_start + offset);
        let line = self.source.get(line_start..line_end).ok_or_else(|| {
            LaravelParseError::InvalidPoint {
                path: self.path.clone(),
                row: point.row,
                column: point.column,
            }
        })?;
        if point.column > line.len() || !line.is_char_boundary(point.column) {
            return Err(LaravelParseError::InvalidPoint {
                path: self.path.clone(),
                row: point.row,
                column: point.column,
            });
        }
        let line_number =
            u32::try_from(point.row + 1).map_err(|source| LaravelParseError::PositionInteger {
                path: self.path.clone(),
                source,
            })?;
        let column_number =
            u32::try_from(line[..point.column].chars().count() + 1).map_err(|source| {
                LaravelParseError::PositionInteger {
                    path: self.path.clone(),
                    source,
                }
            })?;
        TextPosition::new(line_number, column_number).map_err(|error| LaravelParseError::Range {
            path: self.path.clone(),
            message: error.to_string(),
        })
    }

    fn range(&self, node: Node<'_>) -> Result<SourceRange, LaravelParseError> {
        SourceRange::new(
            self.path.clone(),
            self.position(node.start_position())?,
            self.position(node.end_position())?,
        )
        .map_err(|error| LaravelParseError::Range {
            path: self.path.clone(),
            message: error.to_string(),
        })
    }

    fn visit_children(
        &mut self,
        node: Node<'_>,
        context: &Context,
    ) -> Result<(), LaravelParseError> {
        let mut current = context.clone();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "namespace_definition" && child.child_by_field_name("body").is_none()
            {
                current.namespace = self.namespace(child);
                current.imports.clear();
                current.current_type = None;
                current.current_callable = None;
                continue;
            }
            if child.kind() == "namespace_use_declaration" {
                self.collect_imports(child, &mut current);
                continue;
            }
            self.visit(child, &current)?;
        }
        Ok(())
    }

    fn visit(&mut self, node: Node<'_>, context: &Context) -> Result<(), LaravelParseError> {
        match node.kind() {
            "namespace_definition" => {
                if let Some(body) = node.child_by_field_name("body") {
                    let mut nested = context.clone();
                    nested.namespace = self.namespace(node);
                    nested.imports.clear();
                    nested.current_type = None;
                    nested.current_callable = None;
                    self.visit_children(body, &nested)?;
                }
            }
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration" => {
                let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| self.text(name))
                else {
                    return Ok(());
                };
                let kind = match node.kind() {
                    "class_declaration" => SymbolKind::Class,
                    "interface_declaration" => SymbolKind::Interface,
                    "trait_declaration" => SymbolKind::Trait,
                    _ => SymbolKind::Enum,
                };
                let mut nested = context.clone();
                nested.current_type = Some(FrameworkSelector {
                    qualified_name: qualified(&context.namespace, name),
                    kinds: vec![kind],
                });
                nested.current_callable = None;
                if let Some(body) = node.child_by_field_name("body") {
                    self.visit_children(body, &nested)?;
                }
            }
            "method_declaration" | "function_definition" => {
                let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| self.text(name))
                else {
                    return Ok(());
                };
                let method = node.kind() == "method_declaration";
                let qualified_name = if method {
                    let Some(container) = context.current_type.as_ref() else {
                        return Ok(());
                    };
                    format!("{}::{name}", container.qualified_name)
                } else {
                    qualified(&context.namespace, name)
                };
                let mut nested = context.clone();
                nested.current_callable = Some(FrameworkSelector::callable(qualified_name, method));
                if method && name.eq_ignore_ascii_case("__construct") {
                    self.collect_constructor_injection(node, &nested)?;
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.visit_children(body, &nested)?;
                }
            }
            _ => {
                self.inspect_expression(node, context)?;
                self.visit_children(node, context)?;
            }
        }
        Ok(())
    }

    fn namespace(&self, node: Node<'_>) -> Vec<String> {
        node.child_by_field_name("name")
            .or_else(|| {
                node.named_children(&mut node.walk())
                    .find(|child| child.kind() == "namespace_name")
            })
            .and_then(|name| self.text(name))
            .map(name_segments)
            .unwrap_or_default()
    }

    fn collect_imports(&self, node: Node<'_>, context: &mut Context) {
        if node
            .child_by_field_name("type")
            .and_then(|kind| self.text(kind))
            .is_some_and(|kind| matches!(kind, "function" | "const"))
        {
            return;
        }
        let group_prefix = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() == "namespace_name")
            .and_then(|child| self.text(child))
            .map(normalize_name);
        let mut clauses = Vec::new();
        collect_nodes(node, "namespace_use_clause", &mut clauses);
        for clause in clauses {
            if clause
                .child_by_field_name("type")
                .and_then(|kind| self.text(kind))
                .is_some_and(|kind| matches!(kind, "function" | "const"))
            {
                continue;
            }
            let Some(target) = clause
                .named_children(&mut clause.walk())
                .find(|child| matches!(child.kind(), "name" | "qualified_name" | "relative_name"))
                .and_then(|target| self.text(target))
                .map(normalize_name)
            else {
                continue;
            };
            let target = group_prefix
                .as_ref()
                .map_or(target.clone(), |prefix| format!("{prefix}::{target}"));
            let alias = clause
                .child_by_field_name("alias")
                .and_then(|alias| self.text(alias))
                .map(str::to_owned)
                .or_else(|| target.rsplit("::").next().map(str::to_owned));
            if let Some(alias) = alias {
                context.imports.insert(alias, target);
            }
        }
    }

    fn collect_constructor_injection(
        &mut self,
        node: Node<'_>,
        context: &Context,
    ) -> Result<(), LaravelParseError> {
        let (Some(from), Some(parameters)) = (
            context.current_type.clone(),
            node.child_by_field_name("parameters"),
        ) else {
            return Ok(());
        };
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if !matches!(
                parameter.kind(),
                "simple_parameter" | "variadic_parameter" | "property_promotion_parameter"
            ) {
                continue;
            }
            let Some(target) = self.single_named_type(parameter, context) else {
                continue;
            };
            self.push_relation(
                EdgeKind::DependsOn,
                FrameworkEndpoint::Existing(vec![from.clone()]),
                FrameworkEndpoint::Existing(vec![FrameworkSelector::type_named(target)]),
                self.range(parameter)?,
            );
        }
        Ok(())
    }

    fn single_named_type(&self, node: Node<'_>, context: &Context) -> Option<String> {
        let mut types = Vec::new();
        collect_nodes(node.child_by_field_name("type")?, "named_type", &mut types);
        if types.len() != 1 {
            return None;
        }
        let raw = self.text(types[0])?;
        resolve_type(raw, context)
    }

    fn inspect_expression(
        &mut self,
        node: Node<'_>,
        context: &Context,
    ) -> Result<(), LaravelParseError> {
        match node.kind() {
            "function_call_expression" => self.inspect_function_call(node, context)?,
            "member_call_expression" => self.inspect_member_call(node, context)?,
            "scoped_call_expression" => self.inspect_scoped_call(node, context)?,
            _ => {}
        }
        Ok(())
    }

    fn inspect_function_call(
        &mut self,
        node: Node<'_>,
        context: &Context,
    ) -> Result<(), LaravelParseError> {
        let Some(name) = node
            .child_by_field_name("function")
            .and_then(|function| self.text(function))
            .map(|name| name.trim_start_matches('\\'))
        else {
            return Ok(());
        };
        let arguments = arguments(node);
        match name {
            "app" | "resolve" => {
                let Some(target) = arguments
                    .first()
                    .and_then(|argument| self.class_constant(*argument, context))
                else {
                    return Ok(());
                };
                self.push_from_context(
                    "resolve",
                    EdgeKind::Resolves,
                    context,
                    FrameworkEndpoint::Existing(vec![FrameworkSelector::type_named(target)]),
                    node,
                )?;
            }
            "dispatch" => {
                let Some(job) = arguments
                    .first()
                    .and_then(|argument| self.created_type(*argument, context))
                else {
                    return Ok(());
                };
                self.push_from_context(
                    "dispatch",
                    EdgeKind::Dispatches,
                    context,
                    FrameworkEndpoint::Existing(vec![FrameworkSelector::method(&job, "handle")]),
                    node,
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    fn inspect_member_call(
        &mut self,
        node: Node<'_>,
        context: &Context,
    ) -> Result<(), LaravelParseError> {
        let Some(name) = node
            .child_by_field_name("name")
            .and_then(|name| self.text(name))
            .map(str::to_owned)
        else {
            return Ok(());
        };
        let object = node
            .child_by_field_name("object")
            .and_then(|object| self.text(object))
            .map(compact_expression)
            .unwrap_or_default();
        let arguments = arguments(node);
        if matches!(name.as_str(), "bind" | "singleton" | "scoped" | "instance")
            && matches!(object.as_str(), "$this->app" | "$app" | "$this->container")
        {
            self.push_binding(&arguments, context, node)?;
        }
        if matches!(name.as_str(), "job" | "command")
            && matches!(object.as_str(), "$schedule" | "$this->schedule")
        {
            self.push_schedule(&name, &arguments, context, node)?;
        }
        if name == "commands" && object == "$this" {
            self.push_command_registrations(&arguments, context, node)?;
        }
        Ok(())
    }

    fn inspect_scoped_call(
        &mut self,
        node: Node<'_>,
        context: &Context,
    ) -> Result<(), LaravelParseError> {
        let (Some(scope_node), Some(name)) = (
            node.child_by_field_name("scope"),
            node.child_by_field_name("name")
                .and_then(|name| self.text(name))
                .map(str::to_owned),
        ) else {
            return Ok(());
        };
        let Some(raw_scope) = self.text(scope_node) else {
            return Ok(());
        };
        let Some(scope) = resolve_type(raw_scope, context) else {
            return Ok(());
        };
        let arguments = arguments(node);

        if is_facade(&scope, "Route") && route_method(&name) {
            let Some(handler) = arguments.last() else {
                return Ok(());
            };
            if let Some(target) = self.route_target(*handler, context) {
                self.push_from_context("route", EdgeKind::RoutesTo, context, target, node)?;
            }
            return Ok(());
        }
        if is_facade(&scope, "Event") && name == "listen" {
            if let (Some(event), Some(listener)) = (
                arguments
                    .first()
                    .and_then(|argument| self.class_constant(*argument, context)),
                arguments
                    .get(1)
                    .and_then(|argument| self.class_constant(*argument, context)),
            ) {
                self.push_relation(
                    EdgeKind::ListensTo,
                    FrameworkEndpoint::Existing(vec![
                        FrameworkSelector::method(&listener, "handle"),
                        FrameworkSelector::method(&listener, "__invoke"),
                    ]),
                    FrameworkEndpoint::Existing(vec![FrameworkSelector::type_named(event)]),
                    self.range(node)?,
                );
            }
            return Ok(());
        }
        if is_facade(&scope, "Gate") && name == "policy" {
            if let (Some(model), Some(policy)) = (
                arguments
                    .first()
                    .and_then(|argument| self.class_constant(*argument, context)),
                arguments
                    .get(1)
                    .and_then(|argument| self.class_constant(*argument, context)),
            ) {
                self.push_relation(
                    EdgeKind::AuthorizesWith,
                    FrameworkEndpoint::Existing(vec![FrameworkSelector::type_named(model)]),
                    FrameworkEndpoint::Existing(vec![FrameworkSelector::type_named(policy)]),
                    self.range(node)?,
                );
            }
            return Ok(());
        }
        if is_facade(&scope, "Schedule") && matches!(name.as_str(), "job" | "command") {
            self.push_schedule(&name, &arguments, context, node)?;
            return Ok(());
        }
        if is_facade(&scope, "App")
            && matches!(name.as_str(), "bind" | "singleton" | "scoped" | "instance")
        {
            self.push_binding(&arguments, context, node)?;
            return Ok(());
        }
        if is_facade(&scope, "Artisan") && name == "resolveCommands" {
            self.push_command_registrations(&arguments, context, node)?;
            return Ok(());
        }
        if matches!(
            name.as_str(),
            "dispatch" | "dispatchSync" | "dispatchAfterResponse"
        ) {
            self.push_from_context(
                "dispatch",
                EdgeKind::Dispatches,
                context,
                FrameworkEndpoint::Existing(vec![FrameworkSelector::method(&scope, "handle")]),
                node,
            )?;
        }
        Ok(())
    }

    fn push_binding(
        &mut self,
        arguments: &[Node<'_>],
        context: &Context,
        node: Node<'_>,
    ) -> Result<(), LaravelParseError> {
        let (Some(abstract_type), Some(concrete_type)) = (
            arguments
                .first()
                .and_then(|argument| self.class_constant(*argument, context)),
            arguments.get(1).and_then(|argument| {
                self.class_constant(*argument, context)
                    .or_else(|| self.created_type(*argument, context))
            }),
        ) else {
            return Ok(());
        };
        self.push_relation(
            EdgeKind::Binds,
            FrameworkEndpoint::Existing(vec![FrameworkSelector::type_named(abstract_type)]),
            FrameworkEndpoint::Existing(vec![FrameworkSelector::type_named(concrete_type)]),
            self.range(node)?,
        );
        Ok(())
    }

    fn push_schedule(
        &mut self,
        _kind: &str,
        arguments: &[Node<'_>],
        context: &Context,
        node: Node<'_>,
    ) -> Result<(), LaravelParseError> {
        let Some(target_type) = arguments.first().and_then(|argument| {
            self.created_type(*argument, context)
                .or_else(|| self.class_constant(*argument, context))
        }) else {
            return Ok(());
        };
        self.push_from_context(
            "schedule",
            EdgeKind::Schedules,
            context,
            FrameworkEndpoint::Existing(vec![FrameworkSelector::method(&target_type, "handle")]),
            node,
        )?;
        Ok(())
    }

    fn push_command_registrations(
        &mut self,
        arguments: &[Node<'_>],
        context: &Context,
        node: Node<'_>,
    ) -> Result<(), LaravelParseError> {
        let Some(array) = arguments
            .first()
            .filter(|node| node.kind() == "array_creation_expression")
        else {
            return Ok(());
        };
        for element in array_values(*array) {
            let Some(command) = self.class_constant(element, context) else {
                continue;
            };
            self.push_from_context(
                "command",
                EdgeKind::Registers,
                context,
                FrameworkEndpoint::Existing(vec![FrameworkSelector::method(&command, "handle")]),
                node,
            )?;
        }
        Ok(())
    }

    fn route_target(&self, handler: Node<'_>, context: &Context) -> Option<FrameworkEndpoint> {
        if let Some(controller) = self.class_constant(handler, context) {
            return Some(FrameworkEndpoint::Existing(vec![
                FrameworkSelector::method(&controller, "__invoke"),
            ]));
        }
        if handler.kind() != "array_creation_expression" {
            return None;
        }
        let values = array_values(handler);
        let controller = values
            .first()
            .and_then(|value| self.class_constant(*value, context))?;
        let method = values
            .get(1)
            .and_then(|value| self.plain_method_name(*value))?;
        Some(FrameworkEndpoint::Existing(vec![
            FrameworkSelector::method(&controller, &method),
        ]))
    }

    fn class_constant(&self, node: Node<'_>, context: &Context) -> Option<String> {
        if node.kind() != "class_constant_access_expression" {
            return None;
        }
        let children: Vec<_> = node.named_children(&mut node.walk()).collect();
        if children.len() != 2 || !self.text(children[1])?.eq_ignore_ascii_case("class") {
            return None;
        }
        resolve_type(self.text(children[0])?, context)
    }

    fn created_type(&self, node: Node<'_>, context: &Context) -> Option<String> {
        if node.kind() != "object_creation_expression" {
            return None;
        }
        let class = node
            .named_children(&mut node.walk())
            .find(|child| matches!(child.kind(), "name" | "qualified_name" | "relative_name"))?;
        resolve_type(self.text(class)?, context)
    }

    fn plain_method_name(&self, node: Node<'_>) -> Option<String> {
        if node.kind() != "string" {
            return None;
        }
        let raw = self.text(node)?;
        let value = raw
            .strip_prefix('\'')
            .and_then(|value| value.strip_suffix('\''))
            .or_else(|| {
                raw.strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"'))
            })?;
        let mut characters = value.chars();
        let first = characters.next()?;
        if (first == '_' || first.is_ascii_alphabetic())
            && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
        {
            Some(value.to_owned())
        } else {
            None
        }
    }

    fn push_from_context(
        &mut self,
        category: &str,
        kind: EdgeKind,
        context: &Context,
        to: FrameworkEndpoint,
        node: Node<'_>,
    ) -> Result<(), LaravelParseError> {
        let location = self.range(node)?;
        let from = if let Some(callable) = context.current_callable.clone() {
            FrameworkEndpoint::Existing(vec![callable])
        } else {
            if self.symbols.len().saturating_add(self.relations.len()) + 2
                > MAX_FRAMEWORK_FACTS_PER_FILE
            {
                self.truncated = true;
                return Ok(());
            }
            let start = location.start();
            let qualified_name = format!(
                "Laravel::{category}::{}:{}:{}",
                self.path,
                start.line(),
                start.column()
            );
            let signature = self.text(node).and_then(bounded_signature);
            let index = self.symbols.len();
            self.symbols.push(FrameworkSymbolDraft {
                qualified_name,
                location: location.clone(),
                signature,
            });
            FrameworkEndpoint::Synthetic(index)
        };
        self.push_relation(kind, from, to, location);
        Ok(())
    }

    fn push_relation(
        &mut self,
        kind: EdgeKind,
        from: FrameworkEndpoint,
        to: FrameworkEndpoint,
        location: SourceRange,
    ) {
        if self.symbols.len().saturating_add(self.relations.len()) >= MAX_FRAMEWORK_FACTS_PER_FILE {
            self.truncated = true;
            return;
        }
        self.relations.push(FrameworkRelationDraft {
            kind,
            from,
            to,
            location,
        });
    }
}

pub(crate) struct LaravelParser {
    parser: Parser,
}

impl LaravelParser {
    pub fn new() -> Result<Self, LaravelParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .map_err(|error| LaravelParseError::Language(error.to_string()))?;
        Ok(Self { parser })
    }

    pub fn parse(
        &mut self,
        path: RepoRelativePath,
        source: &str,
    ) -> Result<LaravelFile, LaravelParseError> {
        let tree = self
            .parser
            .parse(source, None)
            .ok_or_else(|| LaravelParseError::NoTree(path.clone()))?;
        let line_starts = std::iter::once(0)
            .chain(
                source
                    .bytes()
                    .enumerate()
                    .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
            )
            .collect();
        let mut extraction = Extraction {
            path,
            source,
            line_starts,
            symbols: Vec::new(),
            relations: Vec::new(),
            truncated: false,
        };
        extraction.visit_children(tree.root_node(), &Context::default())?;
        Ok(LaravelFile {
            symbols: extraction.symbols,
            relations: extraction.relations,
            truncated: extraction.truncated,
        })
    }
}

fn arguments(node: Node<'_>) -> Vec<Node<'_>> {
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return Vec::new();
    };
    arguments
        .named_children(&mut arguments.walk())
        .filter_map(|argument| argument.named_children(&mut argument.walk()).next())
        .collect()
}

fn array_values(node: Node<'_>) -> Vec<Node<'_>> {
    node.named_children(&mut node.walk())
        .filter_map(|element| {
            if element.kind() == "array_element_initializer" {
                element.named_children(&mut element.walk()).next()
            } else if element.kind() == "pair" {
                element.child_by_field_name("value")
            } else {
                None
            }
        })
        .collect()
}

fn collect_nodes<'tree>(node: Node<'tree>, kind: &str, output: &mut Vec<Node<'tree>>) {
    if node.kind() == kind {
        output.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_nodes(child, kind, output);
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

fn qualified(namespace: &[String], name: &str) -> String {
    if namespace.is_empty() {
        name.to_owned()
    } else {
        format!("{}::{name}", namespace.join("::"))
    }
}

fn resolve_type(raw: &str, context: &Context) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let absolute = raw.starts_with('\\');
    let namespace_relative = raw.starts_with("namespace\\");
    let normalized = normalize_name(raw);
    if normalized.is_empty() {
        return None;
    }
    if absolute {
        return Some(normalized);
    }
    if namespace_relative {
        return Some(qualified(&context.namespace, &normalized));
    }
    let mut segments = normalized.split("::");
    let first = segments.next()?;
    if let Some(imported) = context.imports.get(first) {
        let suffix = segments.collect::<Vec<_>>().join("::");
        return Some(if suffix.is_empty() {
            imported.clone()
        } else {
            format!("{imported}::{suffix}")
        });
    }
    Some(qualified(&context.namespace, &normalized))
}

fn compact_expression(raw: &str) -> String {
    raw.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn is_facade(resolved: &str, name: &str) -> bool {
    resolved == format!("Illuminate::Support::Facades::{name}")
}

fn route_method(name: &str) -> bool {
    matches!(
        name,
        "get"
            | "post"
            | "put"
            | "patch"
            | "delete"
            | "options"
            | "match"
            | "any"
            | "resource"
            | "apiResource"
    )
}

fn bounded_signature(raw: &str) -> Option<String> {
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let value: String = normalized
        .chars()
        .take(MAX_CONFIGURATION_SIGNATURE_CHARS)
        .collect();
    Some(value)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn extracts_bounded_laravel_conventions_with_resolved_imports() -> Result<(), Box<dyn Error>> {
        let source = r#"<?php
namespace App\Providers;
use App\Contracts\Reporter;
use App\Services\DbReporter;
use App\Http\Controllers\UserController;
use App\Jobs\SyncReport;
use App\Events\UserCreated;
use App\Listeners\SendWelcome;
use App\Models\User;
use App\Policies\UserPolicy;
use Illuminate\Support\Facades\Event;
use Illuminate\Support\Facades\Gate;
use Illuminate\Support\Facades\Route;
use Illuminate\Support\Facades\Schedule;

class AppServiceProvider {
    public function __construct(Reporter $reporter) {}
    public function register(): void {
        $this->app->bind(Reporter::class, DbReporter::class);
        app(Reporter::class);
    }
    public function boot(): void {
        Route::get('/users', [UserController::class, 'show']);
        Route::get('/invoke', UserController::class);
        SyncReport::dispatch();
        Event::listen(UserCreated::class, SendWelcome::class);
        Schedule::job(new SyncReport);
        Gate::policy(User::class, UserPolicy::class);
    }
}
"#;
        let mut parser = LaravelParser::new()?;
        let parsed = parser.parse(
            RepoRelativePath::new("app/Providers/AppServiceProvider.php")?,
            source,
        )?;
        let kinds: Vec<_> = parsed
            .relations
            .iter()
            .map(|relation| relation.kind)
            .collect();
        for kind in [
            EdgeKind::DependsOn,
            EdgeKind::Binds,
            EdgeKind::Resolves,
            EdgeKind::RoutesTo,
            EdgeKind::Dispatches,
            EdgeKind::ListensTo,
            EdgeKind::Schedules,
            EdgeKind::AuthorizesWith,
        ] {
            assert!(kinds.contains(&kind), "missing {kind:?}");
        }
        assert!(!parsed.truncated);
        Ok(())
    }

    #[test]
    fn rejects_dynamic_framework_targets() -> Result<(), Box<dyn Error>> {
        let source = r#"<?php
namespace App;
use Illuminate\Support\Facades\Route;
function configure($controller, $job): void {
    Route::get('/dynamic', [$controller, 'show']);
    app($controller);
    dispatch($job);
}
"#;
        let mut parser = LaravelParser::new()?;
        let parsed = parser.parse(RepoRelativePath::new("routes/web.php")?, source)?;
        assert!(parsed.relations.is_empty());
        assert!(parsed.symbols.is_empty());
        Ok(())
    }
}
