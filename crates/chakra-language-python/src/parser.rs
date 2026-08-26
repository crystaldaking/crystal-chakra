//! Tree-sitter Python extraction into language-neutral Chakra drafts.
//!
//! Grammar coverage follows ADR-0027: `.py`/`.pyi` sources parse with the
//! official Tree-sitter Python grammar. Extraction is deliberately
//! syntactic: import resolution only follows module paths nameable from the
//! source text (repository-relative dotted modules and relative imports),
//! and base-class resolution never invents targets it cannot name.

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
pub(crate) use chakra_language_index::facts::{
    CallDraft, NamedRelationDraft, ParsedFile, SymbolDraft,
};
use thiserror::Error;

struct CallTarget<'tree> {
    form: CallForm,
    target_kind: CallTargetKind,
    name: String,
    qualifier: Option<String>,
    receiver_hint: Option<String>,
    location: Node<'tree>,
}
use tree_sitter::{Node, Parser, Point};

const MAX_SIGNATURE_CHARS: usize = 512;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("failed to load the Tree-sitter Python grammar: {0}")]
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

/// Import aliases resolvable from the source text: a named alias maps to the
/// qualified target symbol, a namespace alias to the qualified module.
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
    /// Sole syntactic base of the enclosing class, used to qualify `super()`
    /// calls only when exactly one base is written.
    super_base: Option<String>,
}

#[derive(Debug)]
struct Extraction<'a> {
    path: RepoRelativePath,
    module_path: Vec<String>,
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
                        language: Language::Python,
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
                            language: Language::Python,
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

    /// Signature text from the (possibly decorated) declaration start up to
    /// its body. Decorators stay recorded because a decorated definition
    /// starts at its first `@`.
    fn signature(&self, node: Node<'_>, definition: Node<'_>) -> Option<String> {
        let end = definition
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
                language: Language::Python,
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

    /// Resolves an `import from` module designator (absolute `dotted_name`
    /// or relative with leading dots) against this file's package. Absolute
    /// dotted paths are repository-relative module paths; relative imports
    /// anchor at the containing package, one extra dot per level up.
    fn resolve_from_module(&self, designator: &str) -> Option<Vec<String>> {
        if !designator.starts_with('.') {
            let segments: Vec<String> = designator
                .split('.')
                .filter(|segment| !segment.is_empty())
                .map(str::to_owned)
                .collect();
            return (!segments.is_empty()).then_some(segments);
        }
        let dots = designator
            .chars()
            .take_while(|character| *character == '.')
            .count();
        let mut package: Vec<String> = self
            .module_path
            .iter()
            .take(self.module_path.len().saturating_sub(1))
            .cloned()
            .collect();
        for _ in 1..dots {
            package.pop()?;
        }
        for segment in designator[dots..].split('.') {
            if !segment.is_empty() {
                package.push(segment.to_owned());
            }
        }
        Some(package)
    }

    fn record_import(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        if let Some(signature) = self.signature(node, node) {
            let name = signature.trim().to_owned();
            if !name.is_empty() {
                self.add_symbol(context, &name, SymbolKind::Import, node, Some(signature))?;
            }
        }
        self.collect_import_aliases(node);
        Ok(())
    }

    /// Fills the import alias maps from one `import`/`from ... import`
    /// statement without emitting any symbol.
    fn collect_import_aliases(&mut self, node: Node<'_>) {
        match node.kind() {
            "import_statement" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        // `import a.b` binds `a.b` (attribute access walks the
                        // chain) and the top segment `a`.
                        "dotted_name" => {
                            let Some(dotted) = self.text(child).map(str::to_owned) else {
                                continue;
                            };
                            let module: Vec<String> = dotted
                                .split('.')
                                .filter(|segment| !segment.is_empty())
                                .map(str::to_owned)
                                .collect();
                            if module.is_empty() {
                                continue;
                            }
                            if let Some((first, _)) = dotted.split_once('.') {
                                self.imports
                                    .namespaces
                                    .insert(first.to_owned(), first.to_owned());
                            }
                            self.imports.namespaces.insert(dotted, module.join("::"));
                        }
                        // `import a.b as ab` binds only the alias.
                        "aliased_import" => {
                            let Some(dotted) = child
                                .child_by_field_name("name")
                                .and_then(|name| self.text(name))
                            else {
                                continue;
                            };
                            let Some(alias) = child
                                .child_by_field_name("alias")
                                .and_then(|alias| self.text(alias))
                            else {
                                continue;
                            };
                            let module: Vec<&str> = dotted
                                .split('.')
                                .filter(|segment| !segment.is_empty())
                                .collect();
                            if module.is_empty() {
                                continue;
                            }
                            self.imports
                                .namespaces
                                .insert(alias.to_owned(), module.join("::"));
                        }
                        _ => {}
                    }
                }
            }
            "import_from_statement" => {
                let Some(module_node) = node.child_by_field_name("module_name") else {
                    return;
                };
                let Some(designator) = self.text(module_node) else {
                    return;
                };
                let Some(module) = self.resolve_from_module(designator) else {
                    return;
                };
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    let (imported, alias) = match child.kind() {
                        "dotted_name" => {
                            let Some(imported) = self.text(child) else {
                                continue;
                            };
                            (imported, imported)
                        }
                        "aliased_import" => {
                            let Some(imported) = child
                                .child_by_field_name("name")
                                .and_then(|name| self.text(name))
                            else {
                                continue;
                            };
                            let alias = child
                                .child_by_field_name("alias")
                                .and_then(|alias| self.text(alias))
                                .unwrap_or(imported);
                            (imported, alias)
                        }
                        _ => continue,
                    };
                    if imported.is_empty() || imported == "*" {
                        continue;
                    }
                    let mut target = module.clone();
                    target.push(imported.to_owned());
                    self.imports
                        .named
                        .insert(alias.to_owned(), target.join("::"));
                }
            }
            _ => {}
        }
    }

    fn visit(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        match node.kind() {
            "function_definition" => self.visit_function(node, context, node),
            "class_definition" => self.visit_class(node, context, node),
            "decorated_definition" => self.visit_decorated(node, context),
            "import_statement" | "import_from_statement" => self.record_import(node, context),
            "assignment" => self.visit_assignment(node, context),
            _ => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.visit(child, context)?;
                }
                Ok(())
            }
        }
    }

    fn visit_decorated(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(definition) = node.child_by_field_name("definition") else {
            return Ok(());
        };
        match definition.kind() {
            "function_definition" => self.visit_function(definition, context, node),
            "class_definition" => self.visit_class(definition, context, node),
            _ => Ok(()),
        }
    }

    fn visit_assignment(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(left) = node.child_by_field_name("left") else {
            return Ok(());
        };
        if left.kind() != "identifier" {
            // Attribute/subscript targets and destructuring bind no single
            // declaration name.
            return Ok(());
        }
        let Some(name) = self.text(left).map(str::to_owned) else {
            return Ok(());
        };
        let kind = if context.method_container {
            SymbolKind::Property
        } else {
            SymbolKind::Constant
        };
        self.add_symbol(context, &name, kind, node, self.signature(node, node))?;
        Ok(())
    }

    fn visit_function(
        &mut self,
        node: Node<'_>,
        context: &Context,
        decorated: Node<'_>,
    ) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        // pytest `test_*` functions and unittest `test_*` methods are tests;
        // every other class-body definition is a method.
        let kind = if name.starts_with("test_") {
            SymbolKind::Test
        } else if context.method_container {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };
        let caller = self.add_symbol(
            context,
            &name,
            kind,
            decorated,
            self.signature(decorated, node),
        )?;
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(
                body,
                caller,
                context.container.as_deref(),
                context.super_base.as_deref(),
            )?;
            let mut prefix = context.prefix.clone();
            prefix.push(name.clone());
            self.visit(
                body,
                &Context {
                    container: Some(name),
                    prefix,
                    parent: Some(caller),
                    method_container: false,
                    super_base: context.super_base.clone(),
                },
            )?;
        }
        Ok(())
    }

    fn visit_class(
        &mut self,
        node: Node<'_>,
        context: &Context,
        decorated: Node<'_>,
    ) -> Result<(), ParseError> {
        let Some(name) = self.node_name(node).map(str::to_owned) else {
            return Ok(());
        };
        let parent = self.add_symbol(
            context,
            &name,
            SymbolKind::Class,
            decorated,
            self.signature(decorated, node),
        )?;
        let bases = self.collect_bases(node, parent, context);
        let super_base = match bases.as_slice() {
            [only] => Some(only.rsplit("::").next().unwrap_or(only.as_str()).to_owned()),
            _ => None,
        };
        let Some(body) = node.child_by_field_name("body") else {
            return Ok(());
        };
        let mut child_context = context.clone();
        child_context.prefix.push(name.clone());
        child_context.container = Some(name);
        child_context.parent = Some(parent);
        child_context.method_container = true;
        child_context.super_base = super_base;
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            self.visit(child, &child_context)?;
        }
        Ok(())
    }

    /// Records `extends` relations for the written base classes and returns
    /// their syntactic names (plain or dotted, normalized to `::`).
    fn collect_bases(&mut self, node: Node<'_>, from: usize, context: &Context) -> Vec<String> {
        let mut names = Vec::new();
        let Some(superclasses) = node.child_by_field_name("superclasses") else {
            return names;
        };
        let mut cursor = superclasses.walk();
        for child in superclasses.named_children(&mut cursor) {
            if child.kind() == "keyword_argument" {
                // `metaclass=...` and friends are not base classes.
                continue;
            }
            let Some(name) = self.heritage_name(child) else {
                continue;
            };
            let candidates = self.heritage_candidates(&name, &context.prefix);
            if candidates.is_empty() {
                continue;
            }
            names.push(name);
            self.named_relations.push(NamedRelationDraft {
                from,
                candidates,
                target_kinds: vec![SymbolKind::Class],
                kind: EdgeKind::Extends,
            });
        }
        names
    }

    /// Syntactic name of a base class: plain (`Base`), dotted (`ns.Base`,
    /// normalized to `ns::Base`), or generic (`Base[T]`).
    fn heritage_name(&self, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "identifier" => self.text(node).map(str::to_owned),
            "attribute" => {
                let object = node.child_by_field_name("object")?;
                let attribute = node.child_by_field_name("attribute")?;
                let object = self.heritage_name(object)?;
                let attribute = self.text(attribute)?;
                Some(format!("{object}::{attribute}"))
            }
            "subscription" | "parenthesized_expression" => node
                .named_child(0)
                .and_then(|inner| self.heritage_name(inner)),
            _ => None,
        }
    }

    /// Ordered resolution candidates for a base-class name: the containing
    /// module prefix first, then aliases recorded from imports (named or
    /// namespace).
    fn heritage_candidates(&self, name: &str, prefix: &[String]) -> Vec<String> {
        let mut candidates = Vec::new();
        if !name.contains("::") {
            candidates.push(Self::qualified(prefix, name));
        }
        if let Some((namespace, member)) = name.rsplit_once("::") {
            if let Some(module) = self
                .imports
                .namespaces
                .get(namespace)
                .or_else(|| self.imports.named.get(namespace))
            {
                candidates.push(format!("{module}::{member}"));
            }
        } else if let Some(target) = self.imports.named.get(name) {
            candidates.push(target.clone());
        }
        candidates
    }

    fn collect_calls(
        &mut self,
        node: Node<'_>,
        caller: usize,
        current_container: Option<&str>,
        super_base: Option<&str>,
    ) -> Result<(), ParseError> {
        // Nested declarations own their calls and are visited separately;
        // walking through them here would attribute their calls to the
        // enclosing callable.
        match node.kind() {
            "function_definition"
            | "class_definition"
            | "decorated_definition"
            | "import_statement"
            | "import_from_statement"
            | "lambda" => return Ok(()),
            _ => {}
        }
        if node.kind() == "call"
            && let Some(function) = node.child_by_field_name("function")
            && let Some(target) = self.call_target(function, current_container, super_base)
        {
            self.calls.push(CallDraft {
                promoted: false,
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
            self.collect_calls(child, caller, current_container, super_base)?;
        }
        Ok(())
    }

    fn call_target<'tree>(
        &self,
        function: Node<'tree>,
        current_container: Option<&str>,
        super_base: Option<&str>,
    ) -> Option<CallTarget<'tree>> {
        match function.kind() {
            "identifier" => {
                let name = self.text(function)?.to_owned();
                if let Some(target) = self.imports.named.get(&name) {
                    if looks_like_type_name(&name) {
                        // An imported class invoked through its (aliased)
                        // name is a constructor call against the class
                        // container.
                        let class = target.rsplit("::").next().unwrap_or(target);
                        return Some(CallTarget {
                            form: CallForm::Scoped,
                            target_kind: CallTargetKind::Method,
                            name: "__init__".to_owned(),
                            qualifier: Some(class.to_owned()),
                            receiver_hint: Some(name),
                            location: function,
                        });
                    }
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
                if looks_like_type_name(&name) {
                    // `ClassName()` constructs an instance: the callable is
                    // the class's `__init__` method when it declares one.
                    return Some(CallTarget {
                        form: CallForm::Scoped,
                        target_kind: CallTargetKind::Method,
                        name: "__init__".to_owned(),
                        qualifier: Some(name.clone()),
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
            "attribute" => {
                let name_node = function.child_by_field_name("attribute")?;
                let object = function.child_by_field_name("object")?;
                let name = self.text(name_node)?.to_owned();
                if object.kind() == "identifier"
                    && let Some(object_name) = self.text(object)
                    && (object_name == "self" || object_name == "cls")
                {
                    return Some(CallTarget {
                        form: CallForm::Member,
                        target_kind: CallTargetKind::Method,
                        name,
                        qualifier: current_container.map(str::to_owned),
                        receiver_hint: Some(object_name.to_owned()),
                        location: name_node,
                    });
                }
                if object.kind() == "call"
                    && object
                        .child_by_field_name("function")
                        .is_some_and(|callee| {
                            callee.kind() == "identifier" && self.text(callee) == Some("super")
                        })
                {
                    // `super().method()` targets the sole written base when
                    // the enclosing class has exactly one.
                    return Some(CallTarget {
                        form: CallForm::Member,
                        target_kind: CallTargetKind::Method,
                        name,
                        qualifier: super_base.map(str::to_owned),
                        receiver_hint: Some("super()".to_owned()),
                        location: name_node,
                    });
                }
                if looks_like_type_name(&name) {
                    // `module.ClassName()` / `obj.ClassName()` constructs an
                    // instance of the attribute class.
                    return Some(CallTarget {
                        form: CallForm::Scoped,
                        target_kind: CallTargetKind::Method,
                        name: "__init__".to_owned(),
                        qualifier: Some(name.clone()),
                        receiver_hint: self.text(object).and_then(bounded_receiver_hint),
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
                    if let Some(module) = self.imports.named.get(object_name) {
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
                if let Some(object_text) = self.text(object)
                    && let Some(module) = self.imports.namespaces.get(object_text)
                {
                    // Dotted module access from `import a.b`: `a.b.func()`.
                    return Some(CallTarget {
                        form: CallForm::Scoped,
                        target_kind: CallTargetKind::Function,
                        name,
                        qualifier: Some(module.clone()),
                        receiver_hint: Some(object_text.to_owned()),
                        location: name_node,
                    });
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
            "parenthesized_expression" => function
                .named_child(0)
                .and_then(|inner| self.call_target(inner, current_container, super_base)),
            _ => None,
        }
    }
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
    name.chars().next().is_some_and(char::is_uppercase)
}

/// Module path of a Python source: `src` collapsed, `__init__.py` files
/// represented by their package directory.
pub(crate) fn module_path(path: &RepoRelativePath) -> Vec<String> {
    let mut components: Vec<&str> = path.as_str().split('/').collect();
    let file = components.pop().unwrap_or_default();
    let mut stem = file;
    for suffix in [".pyi", ".py"] {
        if let Some(stripped) = stem.strip_suffix(suffix) {
            stem = stripped;
            break;
        }
    }

    if components.first() == Some(&"src") {
        components.remove(0);
    }
    let mut module: Vec<String> = components.into_iter().map(str::to_owned).collect();
    if stem != "__init__" {
        module.push(stem.to_owned());
    }
    module
}

pub struct PythonParser {
    parser: Parser,
}

impl PythonParser {
    pub fn new() -> Result<Self, ParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
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
            super_base: None,
        };
        let mut extraction = Extraction {
            path: path.clone(),
            module_path: module_path.clone(),
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
            if matches!(child.kind(), "import_statement" | "import_from_statement") {
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
    fn derives_module_paths_from_python_layout() -> Result<(), Box<dyn std::error::Error>> {
        assert!(module_path(&RepoRelativePath::new("src/__init__.py")?).is_empty());
        assert_eq!(
            module_path(&RepoRelativePath::new("src/service.py")?),
            ["service"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("src/api/controller.py")?),
            ["api", "controller"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("src/api/__init__.py")?),
            ["api"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("tests/test_conformance_flow.py")?),
            ["tests", "test_conformance_flow"]
        );
        assert_eq!(
            module_path(&RepoRelativePath::new("src/types.pyi")?),
            ["types"]
        );
        Ok(())
    }
}
