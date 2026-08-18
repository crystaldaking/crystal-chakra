//! Tree-sitter PHP extraction into language-neutral Chakra drafts.

use std::collections::HashMap;
use std::num::TryFromIntError;
use std::sync::Arc;

use chakra_domain::diagnostic::{
    KnownSyntaxGrammarGap, MAX_SYNTAX_DIAGNOSTICS_PER_FILE, SyntaxDiagnostic,
    SyntaxDiagnosticCause, SyntaxDiagnosticKind,
};
use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{
    CallForm, CallTargetKind, EdgeKind, Language, MAX_RECEIVER_HINT_CHARS, ReceiverTypeSource,
    SymbolKey, SymbolKind,
};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedFile {
    pub source: Arc<str>,
    pub symbols: Vec<SymbolDraft>,
    pub calls: Vec<CallDraft>,
    pub named_relations: Vec<NamedRelationDraft>,
    pub type_relations: Vec<TypeRelationDraft>,
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
    pub receiver_type: Option<String>,
    pub receiver_type_source: Option<ReceiverTypeSource>,
    pub receiver_hint: Option<String>,
    pub location: SourceRange,
}

struct CallTarget<'tree> {
    form: CallForm,
    target_kind: CallTargetKind,
    name: String,
    qualifier: Option<String>,
    receiver_type: Option<String>,
    receiver_type_source: Option<ReceiverTypeSource>,
    receiver_hint: Option<String>,
    location: Node<'tree>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedRelationDraft {
    pub from: usize,
    pub target: String,
    pub target_kinds: Vec<SymbolKind>,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeRelationDraft {
    pub from: usize,
    pub target: String,
    pub kind: TypeRelationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TypeRelationKind {
    Trait,
    Extends,
    Implements,
}

#[derive(Debug, Clone, Default)]
struct Imports {
    types: HashMap<String, String>,
    functions: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct PropertyType {
    name: String,
    promoted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiverType {
    name: String,
    source: ReceiverTypeSource,
}

#[derive(Debug, Clone)]
struct ReceiverEnvironment {
    namespace: Vec<String>,
    imports: Arc<Imports>,
    current_type: Option<String>,
    parent_type: Option<String>,
    properties: Arc<HashMap<String, PropertyType>>,
    variables: HashMap<String, ReceiverType>,
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
    imports: Arc<Imports>,
    current_type: Option<String>,
    current_type_symbol: Option<usize>,
    parent_type: Option<String>,
    properties: Arc<HashMap<String, PropertyType>>,
}

#[derive(Debug)]
struct Extraction<'a> {
    path: RepoRelativePath,
    source: &'a str,
    line_starts: Vec<usize>,
    symbols: Vec<SymbolDraft>,
    calls: Vec<CallDraft>,
    named_relations: Vec<NamedRelationDraft>,
    type_relations: Vec<TypeRelationDraft>,
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
                        language: Language::Php,
                        range: self.range(node)?,
                        kind,
                        provenance: Provenance::TreeSitter,
                        precision: Precision::Syntax,
                        cause: self.diagnostic_cause(node, kind),
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
                            language: Language::Php,
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

    fn diagnostic_cause(
        &self,
        node: Node<'_>,
        kind: SyntaxDiagnosticKind,
    ) -> SyntaxDiagnosticCause {
        if kind == SyntaxDiagnosticKind::Error
            && has_declaration_list_ancestor(node)
            && self
                .text(node)
                .is_some_and(is_typed_class_constant_named_default)
        {
            SyntaxDiagnosticCause::KnownGrammarGap(
                KnownSyntaxGrammarGap::PhpTypedClassConstantNamedDefault,
            )
        } else {
            SyntaxDiagnosticCause::ParseRecovery
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
            } else if child.kind() == "namespace_use_declaration" {
                self.visit_import(child, &mut current)?;
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
            "namespace_use_declaration" => {
                let mut context = context.clone();
                self.visit_import(node, &mut context)
            }
            "use_declaration" if context.current_type_symbol.is_some() => {
                self.collect_trait_uses(node, context);
                Ok(())
            }
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
            imports: Arc::new(Imports::default()),
            current_type: None,
            current_type_symbol: None,
            parent_type: None,
            properties: Arc::new(HashMap::new()),
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
            imports: Arc::new(Imports::default()),
            current_type: None,
            current_type_symbol: None,
            parent_type: None,
            properties: Arc::new(HashMap::new()),
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
        let current_type = Self::qualified(&context.prefix, &name);
        let parent_type = self.collect_inheritance(node, parent, kind, context);
        if let Some(body) = node.child_by_field_name("body") {
            let properties = Arc::new(self.collect_property_types(
                body,
                context,
                &current_type,
                parent_type.as_deref(),
            ));
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
                    imports: context.imports.clone(),
                    current_type: Some(current_type),
                    current_type_symbol: Some(parent),
                    parent_type,
                    properties,
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
        context: &Context,
    ) -> Option<String> {
        let mut parent_type = None;
        let mut cursor = node.walk();
        for clause in node.named_children(&mut cursor) {
            let (relation, type_relation, targets): (EdgeKind, TypeRelationKind, &[SymbolKind]) =
                match clause.kind() {
                    "base_clause" if kind == SymbolKind::Interface => (
                        EdgeKind::Extends,
                        TypeRelationKind::Extends,
                        &[SymbolKind::Interface],
                    ),
                    "base_clause" => (
                        EdgeKind::Extends,
                        TypeRelationKind::Extends,
                        &[SymbolKind::Class],
                    ),
                    "class_interface_clause" => (
                        EdgeKind::Implements,
                        TypeRelationKind::Implements,
                        &[SymbolKind::Interface],
                    ),
                    _ => continue,
                };
            let mut names = clause.walk();
            for target in clause.named_children(&mut names) {
                let Some(raw) = self.text(target) else {
                    continue;
                };
                let Some(normalized) = resolve_type_name(raw, context) else {
                    continue;
                };
                if relation == EdgeKind::Extends
                    && kind == SymbolKind::Class
                    && parent_type.is_none()
                {
                    parent_type = Some(normalized.clone());
                }
                self.named_relations.push(NamedRelationDraft {
                    from,
                    target: normalized.clone(),
                    target_kinds: targets.to_vec(),
                    kind: relation,
                });
                self.type_relations.push(TypeRelationDraft {
                    from,
                    target: normalized,
                    kind: type_relation,
                });
            }
        }
        parent_type
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
            let mut environment = ReceiverEnvironment {
                namespace: context.namespace_prefix.clone(),
                imports: context.imports.clone(),
                current_type: context.current_type.clone(),
                parent_type: context.parent_type.clone(),
                properties: context.properties.clone(),
                variables: HashMap::new(),
            };
            self.collect_parameter_types(node, &mut environment);
            self.collect_calls(body, caller, &mut environment)?;
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
                    imports: context.imports.clone(),
                    current_type: None,
                    current_type_symbol: None,
                    parent_type: None,
                    properties: Arc::new(HashMap::new()),
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

    fn visit_import(&mut self, node: Node<'_>, context: &mut Context) -> Result<(), ParseError> {
        let Some(signature) = self.signature(node) else {
            return Ok(());
        };
        let name = signature.trim_end_matches(';').trim().to_owned();
        if !name.is_empty() {
            self.add_symbol(context, &name, SymbolKind::Import, node, Some(signature))?;
        }
        let declaration_kind = import_kind(node, self.source);
        let group_prefix = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() == "namespace_name")
            .and_then(|child| self.text(child))
            .map(normalize_name);
        let mut clauses = Vec::new();
        collect_nodes(node, "namespace_use_clause", &mut clauses);
        for clause in clauses {
            let kind = import_kind(clause, self.source).or(declaration_kind);
            let Some(target_node) = clause
                .named_children(&mut clause.walk())
                .find(|child| matches!(child.kind(), "name" | "qualified_name" | "relative_name"))
            else {
                continue;
            };
            let Some(raw_target) = self.text(target_node) else {
                continue;
            };
            let mut target = normalize_name(raw_target);
            if let Some(prefix) = &group_prefix {
                target = format!("{prefix}::{target}");
            }
            let alias = clause
                .child_by_field_name("alias")
                .and_then(|alias| self.text(alias))
                .map(str::to_owned)
                .or_else(|| target.rsplit("::").next().map(str::to_owned));
            let Some(alias) = alias else {
                continue;
            };
            let imports = Arc::make_mut(&mut context.imports);
            match kind {
                Some(ImportKind::Function) => {
                    imports.functions.insert(alias, target);
                }
                Some(ImportKind::Constant) => {}
                None => {
                    imports.types.insert(alias, target);
                }
            }
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

    fn collect_property_types(
        &self,
        body: Node<'_>,
        context: &Context,
        current_type: &str,
        parent_type: Option<&str>,
    ) -> HashMap<String, PropertyType> {
        let mut properties = HashMap::new();
        let mut cursor = body.walk();
        for member in body.named_children(&mut cursor) {
            match member.kind() {
                "property_declaration" => {
                    let Some(property_type) = member.child_by_field_name("type").and_then(|node| {
                        self.type_name(node, context, Some(current_type), parent_type)
                    }) else {
                        continue;
                    };
                    let mut elements = member.walk();
                    for element in member.named_children(&mut elements) {
                        if element.kind() != "property_element" {
                            continue;
                        }
                        if let Some(name) = element
                            .child_by_field_name("name")
                            .and_then(|name| self.text(name))
                            .map(variable_name)
                        {
                            properties.insert(
                                name,
                                PropertyType {
                                    name: property_type.clone(),
                                    promoted: false,
                                },
                            );
                        }
                    }
                }
                "method_declaration" if self.node_name(member) == Some("__construct") => {
                    let Some(parameters) = member.child_by_field_name("parameters") else {
                        continue;
                    };
                    let mut parameter_cursor = parameters.walk();
                    for parameter in parameters.named_children(&mut parameter_cursor) {
                        if parameter.kind() != "property_promotion_parameter" {
                            continue;
                        }
                        let Some(name) = parameter
                            .child_by_field_name("name")
                            .and_then(|name| self.text(name))
                            .map(variable_name)
                        else {
                            continue;
                        };
                        let Some(property_type) =
                            parameter.child_by_field_name("type").and_then(|node| {
                                self.type_name(node, context, Some(current_type), parent_type)
                            })
                        else {
                            continue;
                        };
                        properties.insert(
                            name,
                            PropertyType {
                                name: property_type,
                                promoted: true,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        properties
    }

    fn collect_parameter_types(&self, callable: Node<'_>, environment: &mut ReceiverEnvironment) {
        let Some(parameters) = callable.child_by_field_name("parameters") else {
            return;
        };
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if !matches!(
                parameter.kind(),
                "simple_parameter" | "variadic_parameter" | "property_promotion_parameter"
            ) {
                continue;
            }
            let Some(name) = parameter
                .child_by_field_name("name")
                .and_then(|name| self.text(name))
                .map(variable_name)
            else {
                continue;
            };
            let Some(type_name) = parameter
                .child_by_field_name("type")
                .and_then(|node| self.type_name_in_environment(node, environment))
            else {
                continue;
            };
            environment.variables.insert(
                name,
                ReceiverType {
                    name: type_name,
                    source: ReceiverTypeSource::Parameter,
                },
            );
        }
    }

    fn type_name_in_environment(
        &self,
        node: Node<'_>,
        environment: &ReceiverEnvironment,
    ) -> Option<String> {
        let mut named_types = Vec::new();
        collect_nodes(node, "named_type", &mut named_types);
        let mut resolved = Vec::new();
        for named_type in named_types {
            let raw = self.text(named_type)?;
            let value = match raw.trim().to_ascii_lowercase().as_str() {
                "self" | "static" => environment.current_type.clone(),
                "parent" => environment.parent_type.clone(),
                _ => resolve_type_name_in_environment(raw, environment),
            }?;
            if !resolved.contains(&value) {
                resolved.push(value);
            }
        }
        (resolved.len() == 1).then(|| resolved.remove(0))
    }

    fn type_name(
        &self,
        node: Node<'_>,
        context: &Context,
        current_type: Option<&str>,
        parent_type: Option<&str>,
    ) -> Option<String> {
        let mut named_types = Vec::new();
        collect_nodes(node, "named_type", &mut named_types);
        let mut resolved = Vec::new();
        for named_type in named_types {
            let raw = self.text(named_type)?;
            let value = match raw.trim().to_ascii_lowercase().as_str() {
                "self" | "static" => current_type.map(str::to_owned),
                "parent" => parent_type.map(str::to_owned),
                _ => resolve_type_name(raw, context),
            }?;
            if !resolved.contains(&value) {
                resolved.push(value);
            }
        }
        (resolved.len() == 1).then(|| resolved.remove(0))
    }

    fn collect_trait_uses(&mut self, node: Node<'_>, context: &Context) {
        let Some(from) = context.current_type_symbol else {
            return;
        };
        let mut cursor = node.walk();
        for target in node.named_children(&mut cursor) {
            if !matches!(target.kind(), "name" | "qualified_name" | "relative_name") {
                continue;
            }
            let Some(raw) = self.text(target) else {
                continue;
            };
            let Some(target) = resolve_type_name(raw, context) else {
                continue;
            };
            self.type_relations.push(TypeRelationDraft {
                from,
                target,
                kind: TypeRelationKind::Trait,
            });
        }
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
        environment: &mut ReceiverEnvironment,
    ) -> Result<(), ParseError> {
        if matches!(node.kind(), "function_definition" | "method_declaration") {
            return Ok(());
        }
        if matches!(node.kind(), "anonymous_function" | "arrow_function") {
            let mut closure = environment.clone();
            self.collect_parameter_types(node, &mut closure);
            if let Some(body) = node.child_by_field_name("body") {
                self.collect_calls(body, caller, &mut closure)?;
            }
            return Ok(());
        }
        if node.kind() == "assignment_expression" {
            if let Some(right) = node.child_by_field_name("right") {
                self.collect_calls(right, caller, environment)?;
                if let Some(left) = node
                    .child_by_field_name("left")
                    .filter(|left| left.kind() == "variable_name")
                    .and_then(|left| self.text(left))
                    .map(variable_name)
                {
                    let inferred = self
                        .expression_receiver_type(right, environment)
                        .or_else(|| self.fluent_reassignment_type(right, &left, environment));
                    if let Some(inferred) = inferred {
                        environment.variables.insert(left, inferred);
                    } else {
                        environment.variables.remove(&left);
                    }
                }
            }
            return Ok(());
        }
        if let Some(target) = self.call_target(node, environment) {
            self.calls.push(CallDraft {
                caller,
                form: target.form,
                target_kind: target.target_kind,
                name: target.name,
                qualifier: target.qualifier,
                receiver_type: target.receiver_type,
                receiver_type_source: target.receiver_type_source,
                receiver_hint: target.receiver_hint,
                location: self.range(target.location)?,
            });
        }
        if is_control_flow_boundary(node.kind()) {
            let original = environment.variables.clone();
            let mut branch_variables = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                let mut branch = environment.clone();
                self.collect_calls(child, caller, &mut branch)?;
                branch_variables.push(branch.variables);
            }
            environment.variables = original
                .into_iter()
                .filter(|(name, receiver)| {
                    branch_variables
                        .iter()
                        .all(|variables| variables.get(name) == Some(receiver))
                })
                .collect();
            return Ok(());
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect_calls(child, caller, environment)?;
        }
        Ok(())
    }

    fn fluent_reassignment_type(
        &self,
        expression: Node<'_>,
        variable: &str,
        environment: &ReceiverEnvironment,
    ) -> Option<ReceiverType> {
        if !matches!(
            expression.kind(),
            "member_call_expression" | "nullsafe_member_call_expression"
        ) {
            return None;
        }
        let object = expression.child_by_field_name("object")?;
        if object.kind() != "variable_name" || variable_name(self.text(object)?) != variable {
            return None;
        }
        environment.variables.get(variable).cloned()
    }

    fn call_target<'a>(
        &self,
        node: Node<'a>,
        environment: &ReceiverEnvironment,
    ) -> Option<CallTarget<'a>> {
        match node.kind() {
            "function_call_expression" => {
                let target = node.child_by_field_name("function")?;
                let raw = self.text(target)?;
                if !matches!(target.kind(), "name" | "qualified_name" | "relative_name") {
                    return None;
                }
                let resolved = resolve_function_name(raw, environment);
                let (qualifier, name) = resolved.rsplit_once("::").map_or_else(
                    || (None, resolved.clone()),
                    |(qualifier, name)| (Some(qualifier.to_owned()), name.to_owned()),
                );
                Some(CallTarget {
                    form: CallForm::Function,
                    target_kind: CallTargetKind::Function,
                    name,
                    qualifier,
                    receiver_type: None,
                    receiver_type_source: None,
                    receiver_hint: None,
                    location: target,
                })
            }
            "member_call_expression" | "nullsafe_member_call_expression" => {
                let name_node = node.child_by_field_name("name")?;
                if name_node.kind() != "name" {
                    return None;
                }
                let name = self.text(name_node)?.to_owned();
                let object_node = node.child_by_field_name("object")?;
                let receiver_hint = self.text(object_node).and_then(bounded_receiver_hint);
                let receiver = self.expression_receiver_type(object_node, environment);
                let qualifier = receiver.as_ref().map(|receiver| receiver.name.clone());
                Some(CallTarget {
                    form: if node.kind() == "nullsafe_member_call_expression" {
                        CallForm::NullsafeMember
                    } else {
                        CallForm::Member
                    },
                    target_kind: CallTargetKind::Method,
                    name,
                    qualifier,
                    receiver_type: receiver.as_ref().map(|receiver| receiver.name.clone()),
                    receiver_type_source: receiver.map(|receiver| receiver.source),
                    receiver_hint,
                    location: name_node,
                })
            }
            "scoped_call_expression" => {
                let name_node = node.child_by_field_name("name")?;
                if name_node.kind() != "name" {
                    return None;
                }
                let name = self.text(name_node)?.to_owned();
                let scope = node.child_by_field_name("scope")?;
                let raw_scope = self.text(scope)?;
                let receiver = match raw_scope.to_ascii_lowercase().as_str() {
                    "self" | "static" => {
                        environment.current_type.as_ref().map(|name| ReceiverType {
                            name: name.clone(),
                            source: ReceiverTypeSource::SelfType,
                        })
                    }
                    "parent" => environment.parent_type.as_ref().map(|name| ReceiverType {
                        name: name.clone(),
                        source: ReceiverTypeSource::ParentType,
                    }),
                    _ if matches!(scope.kind(), "name" | "qualified_name" | "relative_name") => {
                        resolve_type_name_in_environment(raw_scope, environment).map(|name| {
                            ReceiverType {
                                name,
                                source: ReceiverTypeSource::ScopedType,
                            }
                        })
                    }
                    _ => None,
                };
                let qualifier = receiver.as_ref().map(|receiver| receiver.name.clone());
                Some(CallTarget {
                    form: CallForm::Scoped,
                    target_kind: CallTargetKind::Method,
                    name,
                    qualifier,
                    receiver_type: receiver.as_ref().map(|receiver| receiver.name.clone()),
                    receiver_type_source: receiver.map(|receiver| receiver.source),
                    receiver_hint: bounded_receiver_hint(raw_scope),
                    location: name_node,
                })
            }
            _ => None,
        }
    }

    fn expression_receiver_type(
        &self,
        node: Node<'_>,
        environment: &ReceiverEnvironment,
    ) -> Option<ReceiverType> {
        match node.kind() {
            "variable_name" => {
                let name = variable_name(self.text(node)?);
                if name == "this" {
                    return environment.current_type.as_ref().map(|name| ReceiverType {
                        name: name.clone(),
                        source: ReceiverTypeSource::This,
                    });
                }
                environment.variables.get(&name).cloned()
            }
            "member_access_expression" | "nullsafe_member_access_expression" => {
                let object = node.child_by_field_name("object")?;
                let property = node.child_by_field_name("name")?;
                if self.text(object)? != "$this" || property.kind() != "name" {
                    return None;
                }
                let property = environment.properties.get(self.text(property)?)?;
                Some(ReceiverType {
                    name: property.name.clone(),
                    source: if property.promoted {
                        ReceiverTypeSource::PromotedProperty
                    } else {
                        ReceiverTypeSource::Property
                    },
                })
            }
            "object_creation_expression" => {
                let mut cursor = node.walk();
                let class = node.named_children(&mut cursor).find(|child| {
                    matches!(child.kind(), "name" | "qualified_name" | "relative_name")
                })?;
                let name = resolve_receiver_type_name(self.text(class)?, environment)?;
                Some(ReceiverType {
                    name,
                    source: ReceiverTypeSource::LocalNew,
                })
            }
            "function_call_expression" => self.service_locator_type(node, environment),
            "parenthesized_expression" => {
                let mut cursor = node.walk();
                let expression = node.named_children(&mut cursor).next()?;
                self.expression_receiver_type(expression, environment)
            }
            _ => None,
        }
    }

    fn service_locator_type(
        &self,
        node: Node<'_>,
        environment: &ReceiverEnvironment,
    ) -> Option<ReceiverType> {
        let function = node.child_by_field_name("function")?;
        let function = self.text(function)?.trim_start_matches('\\');
        if !matches!(function, "app" | "resolve") {
            return None;
        }
        let arguments = node.child_by_field_name("arguments")?;
        let mut arguments_cursor = arguments.walk();
        let argument = arguments.named_children(&mut arguments_cursor).next()?;
        let mut argument_cursor = argument.walk();
        let expression = argument.named_children(&mut argument_cursor).next()?;
        if expression.kind() != "class_constant_access_expression" {
            return None;
        }
        let mut parts_cursor = expression.walk();
        let parts: Vec<_> = expression.named_children(&mut parts_cursor).collect();
        let (scope, constant) = match parts.as_slice() {
            [scope, constant] => (*scope, *constant),
            _ => return None,
        };
        if !self.text(constant)?.eq_ignore_ascii_case("class") {
            return None;
        }
        if !matches!(
            scope.kind(),
            "name" | "qualified_name" | "relative_name" | "relative_scope"
        ) {
            return None;
        }
        let name = resolve_receiver_type_name(self.text(scope)?, environment)?;
        Some(ReceiverType {
            name,
            source: ReceiverTypeSource::ServiceLocator,
        })
    }
}

fn has_declaration_list_ancestor(mut node: Node<'_>) -> bool {
    for _ in 0..3 {
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind() == "declaration_list" {
            return true;
        }
        node = parent;
    }
    false
}

fn is_typed_class_constant_named_default(source: &str) -> bool {
    let Some((declaration, _)) = source.split_once('=') else {
        return false;
    };
    let tokens = declaration.split_ascii_whitespace().collect::<Vec<_>>();
    let Some(const_index) = tokens
        .iter()
        .position(|token| token.eq_ignore_ascii_case("const"))
    else {
        return false;
    };
    let declaration_tail = &tokens[const_index.saturating_add(1)..];
    declaration_tail.len() == 2
        && declaration_tail
            .last()
            .is_some_and(|name| name.eq_ignore_ascii_case("default"))
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
            type_relations: Vec::new(),
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
                imports: Arc::new(Imports::default()),
                current_type: None,
                current_type_symbol: None,
                parent_type: None,
                properties: Arc::new(HashMap::new()),
            },
        )?;
        let (diagnostics, diagnostic_count) = extraction.diagnostics(root)?;
        Ok(ParsedFile {
            source: source.clone(),
            symbols: extraction.symbols,
            calls: extraction.calls,
            named_relations: extraction.named_relations,
            type_relations: extraction.type_relations,
            has_errors: root.has_error(),
            diagnostics,
            diagnostic_count,
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

fn bounded_receiver_hint(raw: &str) -> Option<String> {
    let hint = normalize_name(raw).trim_start_matches('$').to_owned();
    (!hint.is_empty() && hint.chars().count() <= MAX_RECEIVER_HINT_CHARS).then_some(hint)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportKind {
    Function,
    Constant,
}

fn import_kind(node: Node<'_>, source: &str) -> Option<ImportKind> {
    let kind = node.child_by_field_name("type")?;
    match source.get(kind.byte_range())? {
        "function" => Some(ImportKind::Function),
        "const" => Some(ImportKind::Constant),
        _ => None,
    }
}

fn collect_nodes<'tree>(node: Node<'tree>, kind: &str, result: &mut Vec<Node<'tree>>) {
    if node.kind() == kind {
        result.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_nodes(child, kind, result);
    }
}

fn variable_name(raw: &str) -> String {
    raw.trim_start_matches('$').to_owned()
}

fn resolve_type_name(raw: &str, context: &Context) -> Option<String> {
    resolve_reference(raw, &context.namespace_prefix, &context.imports.types)
}

fn resolve_type_name_in_environment(
    raw: &str,
    environment: &ReceiverEnvironment,
) -> Option<String> {
    resolve_reference(raw, &environment.namespace, &environment.imports.types)
}

fn resolve_receiver_type_name(raw: &str, environment: &ReceiverEnvironment) -> Option<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "self" | "static" => environment.current_type.clone(),
        "parent" => environment.parent_type.clone(),
        _ => resolve_type_name_in_environment(raw, environment),
    }
}

fn resolve_reference(
    raw: &str,
    namespace: &[String],
    aliases: &HashMap<String, String>,
) -> Option<String> {
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
        return Some(if namespace.is_empty() {
            normalized
        } else {
            format!("{}::{normalized}", namespace.join("::"))
        });
    }
    let mut segments = normalized.split("::");
    let first = segments.next()?;
    if let Some(imported) = aliases.get(first) {
        let suffix = segments.collect::<Vec<_>>().join("::");
        return Some(if suffix.is_empty() {
            imported.clone()
        } else {
            format!("{imported}::{suffix}")
        });
    }
    if namespace.is_empty() {
        Some(normalized)
    } else {
        Some(format!("{}::{normalized}", namespace.join("::")))
    }
}

fn resolve_function_name(raw: &str, environment: &ReceiverEnvironment) -> String {
    let raw = raw.trim();
    let normalized = normalize_name(raw);
    if raw.starts_with('\\') {
        return normalized;
    }
    if raw.starts_with("namespace\\") {
        return if environment.namespace.is_empty() {
            normalized
        } else {
            format!("{}::{normalized}", environment.namespace.join("::"))
        };
    }
    let mut segments = normalized.split("::");
    if let Some(first) = segments.next()
        && let Some(imported) = environment.imports.functions.get(first)
    {
        let suffix = segments.collect::<Vec<_>>().join("::");
        return if suffix.is_empty() {
            imported.clone()
        } else {
            format!("{imported}::{suffix}")
        };
    }
    if environment.namespace.is_empty() {
        normalized
    } else {
        format!("{}::{normalized}", environment.namespace.join("::"))
    }
}

fn is_control_flow_boundary(kind: &str) -> bool {
    matches!(
        kind,
        "if_statement"
            | "switch_statement"
            | "while_statement"
            | "do_statement"
            | "for_statement"
            | "foreach_statement"
            | "try_statement"
            | "match_expression"
    )
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
        let member_call = parsed
            .calls
            .iter()
            .find(|call| call.name == "audit")
            .ok_or("member call missing")?;
        assert_eq!(member_call.form, CallForm::Member);
        assert_eq!(member_call.target_kind, CallTargetKind::Method);
        assert_eq!(member_call.receiver_hint.as_deref(), Some("this"));
        assert_eq!(
            member_call.qualifier.as_deref(),
            Some("App::Service::PaymentService")
        );
        let scoped_call = parsed
            .calls
            .iter()
            .find(|call| call.name == "send")
            .ok_or("scoped call missing")?;
        assert_eq!(scoped_call.form, CallForm::Scoped);
        assert_eq!(scoped_call.target_kind, CallTargetKind::Method);
        assert_eq!(scoped_call.receiver_hint.as_deref(), Some("Provider"));
        assert_eq!(
            scoped_call.qualifier.as_deref(),
            Some("App::Provider::Provider")
        );
        let function_call = parsed
            .calls
            .iter()
            .find(|call| call.name == "helper")
            .ok_or("function call missing")?;
        assert_eq!(function_call.form, CallForm::Function);
        assert_eq!(function_call.target_kind, CallTargetKind::Function);
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
        assert!(parsed.diagnostic_count > 0);
        assert!(!parsed.diagnostics.is_empty());
        assert!(parsed.diagnostics.iter().all(|diagnostic| {
            diagnostic.language == Language::Php
                && diagnostic.range.file().as_str() == "broken.php"
                && diagnostic.cause == SyntaxDiagnosticCause::ParseRecovery
        }));
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.key.qualified_name == "still_visible")
        );
        Ok(())
    }

    #[test]
    fn accepts_modern_php_syntax_without_diagnostics() -> Result<(), Box<dyn std::error::Error>> {
        let mut parser = PhpParser::new()?;
        let parsed = parser.parse(
            RepoRelativePath::new("src/Modern.php")?,
            Arc::from(
                r#"<?php
#[Attribute]
final class Marker {}

enum Status: string { case Paid = 'paid'; }

readonly class Payment {
    public const string KIND = 'payment';
    public function __construct(public string $id) {}
}
"#,
            ),
        )?;
        assert!(!parsed.has_errors);
        assert_eq!(parsed.diagnostic_count, 0);
        assert!(parsed.diagnostics.is_empty());
        Ok(())
    }

    #[test]
    fn classifies_keyword_named_typed_class_constant_as_known_grammar_gap()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = "<?php class Payment { public const string DEFAULT = 'default'; }";
        let mut parser = PhpParser::new()?;
        let parsed = parser.parse(RepoRelativePath::new("src/Payment.php")?, Arc::from(source))?;
        assert!(parsed.has_errors);
        assert_eq!(parsed.diagnostic_count, 1);
        assert_eq!(
            parsed.diagnostics[0].cause,
            SyntaxDiagnosticCause::KnownGrammarGap(
                KnownSyntaxGrammarGap::PhpTypedClassConstantNamedDefault,
            )
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

    #[test]
    fn infers_bounded_receiver_types_from_supported_php_syntax()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = r#"<?php
namespace App\Feature;
use Vendor\Services\{First as One, Second};
use Vendor\Policy as PolicyAlias;
use function Vendor\Functions\run as invoke;
trait SharedTrait { public function shared(): void {} }
class Child extends PolicyAlias {
    use SharedTrait;
    private One $property;
    public function __construct(private Second $promoted, PolicyAlias $parameter) {
        $local = new PolicyAlias();
        $resolved = app(PolicyAlias::class);
        $this->own();
        $this->property->run();
        $this->promoted?->run();
        $parameter->run();
        $parameter = $parameter->configure();
        $parameter->run();
        $local->run();
        $resolved->run();
        resolve(PolicyAlias::class)->run();
        self::own();
        parent::run();
        PolicyAlias::run();
        $dynamic->run();
        $class::run();
        $callback = function (PolicyAlias $closureService): void {
            $closureService->run();
        };
        invoke();
    }
    private function own(): void {}
    public function branch(PolicyAlias $branch, bool $flag): void {
        if ($flag) { $branch = app(One::class); }
        $branch->run();
    }
}
"#;
        let mut parser = PhpParser::new()?;
        let parsed = parser.parse(RepoRelativePath::new("src/Child.php")?, Arc::from(source))?;

        let call = |hint: &str, source| {
            parsed.calls.iter().find(|call| {
                call.receiver_hint.as_deref() == Some(hint) && call.receiver_type_source == source
            })
        };
        assert_eq!(
            call("this->property", Some(ReceiverTypeSource::Property))
                .and_then(|call| call.receiver_type.as_deref()),
            Some("Vendor::Services::First")
        );
        assert_eq!(
            call("this->promoted", Some(ReceiverTypeSource::PromotedProperty))
                .and_then(|call| call.receiver_type.as_deref()),
            Some("Vendor::Services::Second")
        );
        assert_eq!(
            call("parameter", Some(ReceiverTypeSource::Parameter))
                .and_then(|call| call.receiver_type.as_deref()),
            Some("Vendor::Policy")
        );
        assert_eq!(
            parsed
                .calls
                .iter()
                .filter(|call| {
                    call.receiver_hint.as_deref() == Some("parameter")
                        && call.receiver_type_source == Some(ReceiverTypeSource::Parameter)
                })
                .count(),
            3
        );
        assert_eq!(
            call("closureService", Some(ReceiverTypeSource::Parameter))
                .and_then(|call| call.receiver_type.as_deref()),
            Some("Vendor::Policy")
        );
        assert_eq!(
            call("local", Some(ReceiverTypeSource::LocalNew))
                .and_then(|call| call.receiver_type.as_deref()),
            Some("Vendor::Policy")
        );
        assert_eq!(
            call("resolved", Some(ReceiverTypeSource::ServiceLocator))
                .and_then(|call| call.receiver_type.as_deref()),
            Some("Vendor::Policy")
        );
        assert_eq!(
            call("parent", Some(ReceiverTypeSource::ParentType))
                .and_then(|call| call.receiver_type.as_deref()),
            Some("Vendor::Policy")
        );
        assert!(call("dynamic", None).is_some());
        assert!(call("class", None).is_some());
        assert!(call("branch", None).is_some());
        assert!(parsed.calls.iter().any(|call| {
            call.form == CallForm::Function
                && call.name == "run"
                && call.qualifier.as_deref() == Some("Vendor::Functions")
        }));
        assert!(
            parsed
                .type_relations
                .iter()
                .any(|relation| { relation.target == "Vendor::Policy" })
        );
        assert!(
            parsed
                .type_relations
                .iter()
                .any(|relation| { relation.target == "App::Feature::SharedTrait" })
        );
        Ok(())
    }
}
