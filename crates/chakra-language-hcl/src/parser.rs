//! Tree-sitter HCL extraction into language-neutral Chakra drafts.
//!
//! Terraform/OpenTofu blocks are retained as configuration entities while
//! modules, variables, outputs, locals, resources, data sources, providers,
//! imports, and test `run` blocks receive stable qualified names. Traversals
//! such as `aws_s3_bucket.logs.id`, `var.region`, and `module.vpc.id` are
//! bounded syntax call candidates; terraform-ls may later confirm them.

use std::num::TryFromIntError;
use std::sync::Arc;

use chakra_domain::diagnostic::{
    MAX_SYNTAX_DIAGNOSTICS_PER_FILE, SyntaxDiagnostic, SyntaxDiagnosticCause, SyntaxDiagnosticKind,
};
use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{CallForm, CallTargetKind, EdgeKind, Language, SymbolKey, SymbolKind};
pub(crate) use chakra_language_index::facts::{
    CallDraft, NamedRelationDraft, ParsedFile, SymbolDraft,
};
use thiserror::Error;
use tree_sitter::{Node, Parser, Point};

const MAX_SIGNATURE_CHARS: usize = 512;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("failed to load the Tree-sitter HCL grammar: {0}")]
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
struct Context {
    prefix: Vec<String>,
    container: Option<String>,
    parent: Option<usize>,
    caller: Option<usize>,
    block_type: Option<String>,
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

    fn position_for_byte(&self, byte: usize) -> Result<TextPosition, ParseError> {
        if byte > self.source.len() || !self.source.is_char_boundary(byte) {
            return Err(ParseError::InvalidPoint {
                path: self.path.clone(),
                row: 0,
                column: byte,
            });
        }
        let line_index = self
            .line_starts
            .partition_point(|line_start| *line_start <= byte)
            .saturating_sub(1);
        let line_start = self.line_starts.get(line_index).copied().unwrap_or(0);
        let line = u32::try_from(line_index + 1).map_err(|source| ParseError::PositionInteger {
            path: self.path.clone(),
            source,
        })?;
        let column =
            u32::try_from(self.source[line_start..byte].chars().count() + 1).map_err(|source| {
                ParseError::PositionInteger {
                    path: self.path.clone(),
                    source,
                }
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

    fn byte_range(&self, start: usize, end: usize) -> Result<SourceRange, ParseError> {
        SourceRange::new(
            self.path.clone(),
            self.position_for_byte(start)?,
            self.position_for_byte(end)?,
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
                        language: Language::Hcl,
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
                            language: Language::Hcl,
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

    fn qualified(prefix: &[String], segments: &[String]) -> String {
        prefix
            .iter()
            .chain(segments)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("::")
    }

    fn signature(&self, node: Node<'_>) -> Option<String> {
        let raw = self.text(node)?.trim();
        if raw.is_empty() {
            return None;
        }
        let raw = raw.split_once('{').map_or(raw, |(header, _)| header.trim());
        let mut signature = String::new();
        let mut chars = 0_usize;
        for word in raw.split_whitespace() {
            if !signature.is_empty() && chars < MAX_SIGNATURE_CHARS {
                signature.push(' ');
                chars += 1;
            }
            for character in word.chars() {
                if chars == MAX_SIGNATURE_CHARS {
                    signature.push('…');
                    return Some(signature);
                }
                signature.push(character);
                chars += 1;
            }
        }
        Some(signature)
    }

    fn add_symbol(
        &mut self,
        context: &Context,
        segments: Vec<String>,
        kind: SymbolKind,
        node: Node<'_>,
        signature: Option<String>,
    ) -> Result<usize, ParseError> {
        let qualified_name = Self::qualified(&context.prefix, &segments);
        let index = self.symbols.len();
        self.symbols.push(SymbolDraft {
            key: SymbolKey {
                language: Language::Hcl,
                qualified_name,
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

    fn walk(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        match node.kind() {
            "block" => self.block(node, context),
            "attribute" => self.attribute(node, context),
            "function_call" => {
                self.function_call(node, context)?;
                self.walk_children(node, context)
            }
            "variable_expr" => self.traversal(node, context),
            _ => self.walk_children(node, context),
        }
    }

    fn walk_children(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child, context)?;
        }
        Ok(())
    }

    fn block(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let (block_type, labels, _name_node, body) = self.block_header(node)?;
        let (segments, kind) = block_identity(&block_type, &labels, &self.path);
        let signature = self.signature(node);
        let symbol = self.add_symbol(context, segments.clone(), kind, node, signature)?;
        let full = Self::qualified(&context.prefix, &segments);
        let child_prefix = if block_type == "locals" {
            vec!["local".to_owned()]
        } else {
            full.split("::").map(str::to_owned).collect()
        };
        let nested = Context {
            prefix: child_prefix,
            container: Some(full),
            parent: Some(symbol),
            caller: Some(symbol),
            block_type: Some(block_type),
        };
        if let Some(body) = body {
            self.walk(body, &nested)?;
        }
        Ok(())
    }

    fn block_header<'tree>(
        &self,
        node: Node<'tree>,
    ) -> Result<(String, Vec<String>, Node<'tree>, Option<Node<'tree>>), ParseError> {
        let mut cursor = node.walk();
        let mut children = node.named_children(&mut cursor);
        let Some(name_node) = children.find(|child| child.kind() == "identifier") else {
            return Err(ParseError::Range {
                path: self.path.clone(),
                message: "HCL block has no identifier".to_owned(),
            });
        };
        let block_type = self.text(name_node).unwrap_or_default().trim().to_owned();
        let mut labels = Vec::new();
        let mut body = None;
        for child in children {
            match child.kind() {
                "body" => body = Some(child),
                "identifier" | "string_lit" => {
                    if let Some(label) = self.text(child).and_then(normalize_label) {
                        labels.push(label);
                    }
                }
                _ => {}
            }
        }
        Ok((block_type, labels, name_node, body))
    }

    fn attribute(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let mut cursor = node.walk();
        let mut children = node.named_children(&mut cursor);
        let Some(name_node) = children.find(|child| child.kind() == "identifier") else {
            return self.walk_children(node, context);
        };
        let name = self.text(name_node).unwrap_or_default().trim().to_owned();
        if name.is_empty() {
            return self.walk_children(node, context);
        }
        let is_import = (name == "source"
            && matches!(context.block_type.as_deref(), Some("module" | "provider")))
            || context.block_type.as_deref() == Some("required_providers");
        let kind = if is_import {
            SymbolKind::Import
        } else {
            SymbolKind::Property
        };
        let mut attribute_context = context.clone();
        if context.parent.is_none() && is_tfvars(&self.path) {
            attribute_context.prefix = vec!["tfvars".to_owned()];
            attribute_context.container = Some("tfvars".to_owned());
        }
        let symbol = self.add_symbol(
            &attribute_context,
            vec![name],
            kind,
            name_node,
            self.signature(node),
        )?;
        if is_import && let Some(from) = context.caller {
            let target = self.symbols[symbol].key.qualified_name.clone();
            self.named_relations.push(NamedRelationDraft {
                from,
                candidates: vec![target],
                target_kinds: vec![SymbolKind::Import],
                kind: EdgeKind::Imports,
            });
        }
        let nested = Context {
            caller: context.caller.or(Some(symbol)),
            ..context.clone()
        };
        for child in children {
            self.walk(child, &nested)?;
        }
        Ok(())
    }

    fn function_call(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(caller) = context.caller else {
            return Ok(());
        };
        let mut cursor = node.walk();
        let Some(name_node) = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "identifier")
        else {
            return Ok(());
        };
        let Some(name) = self
            .text(name_node)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            return Ok(());
        };
        self.calls.push(CallDraft {
            promoted: false,
            caller,
            form: CallForm::Function,
            target_kind: CallTargetKind::Function,
            name: name.rsplit("::").next().unwrap_or(name).to_owned(),
            qualifier: name
                .rsplit_once("::")
                .map(|(qualifier, _)| qualifier.to_owned()),
            receiver_hint: None,
            location: self.range(name_node)?,
        });
        Ok(())
    }

    fn traversal(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(caller) = context.caller else {
            return Ok(());
        };
        let segments = scan_traversal(self.source, node.start_byte());
        let Some(target) = traversal_target(&segments) else {
            return Ok(());
        };
        self.calls.push(CallDraft {
            promoted: false,
            caller,
            form: CallForm::Scoped,
            target_kind: CallTargetKind::Configuration,
            name: target.name,
            qualifier: Some(target.qualifier),
            receiver_hint: segments.first().map(|segment| segment.name.clone()),
            location: self.byte_range(target.start, target.end)?,
        });
        Ok(())
    }
}

#[derive(Debug)]
struct TraversalSegment {
    name: String,
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct TraversalTarget {
    name: String,
    qualifier: String,
    start: usize,
    end: usize,
}

fn scan_traversal(source: &str, start: usize) -> Vec<TraversalSegment> {
    let Some(rest) = source.get(start..) else {
        return Vec::new();
    };
    let mut segments = Vec::new();
    let mut offset = 0_usize;
    while let Some(segment_source) = rest.get(offset..) {
        let mut end = 0_usize;
        for (index, character) in segment_source.char_indices() {
            if character.is_alphanumeric() || matches!(character, '_' | '-' | ':') {
                end = index + character.len_utf8();
            } else {
                break;
            }
        }
        if end == 0 {
            break;
        }
        let absolute_start = start + offset;
        segments.push(TraversalSegment {
            name: segment_source[..end].to_owned(),
            start: absolute_start,
            end: absolute_start + end,
        });
        offset += end;
        if rest.as_bytes().get(offset) != Some(&b'.') {
            break;
        }
        offset += 1;
    }
    segments
}

fn traversal_target(segments: &[TraversalSegment]) -> Option<TraversalTarget> {
    if segments.len() < 2 {
        return None;
    }
    let root = segments.first()?.name.as_str();
    if matches!(
        root,
        "count" | "each" | "self" | "path" | "terraform" | "toset" | "null"
    ) {
        return None;
    }
    let (qualifier, target_index) = match root {
        "var" | "local" | "module" | "output" | "provider" | "dependency" | "include" => {
            (root.to_owned(), 1)
        }
        "data" | "resource" if segments.len() >= 3 => (format!("{root}::{}", segments[1].name), 2),
        "remote_state" | "generate" | "inputs" => (root.to_owned(), 1),
        _ => (format!("resource::{root}"), 1),
    };
    let target = segments.get(target_index)?;
    Some(TraversalTarget {
        name: target.name.clone(),
        qualifier,
        start: target.start,
        end: target.end,
    })
}

fn normalize_label(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(raw)
        .trim();
    (!raw.is_empty()).then(|| raw.to_owned())
}

fn block_identity(
    block_type: &str,
    labels: &[String],
    path: &RepoRelativePath,
) -> (Vec<String>, SymbolKind) {
    let label = |index: usize, fallback: &str| {
        labels
            .get(index)
            .cloned()
            .unwrap_or_else(|| fallback.to_owned())
    };
    match block_type {
        "resource" => (
            vec![
                "resource".to_owned(),
                label(0, "unknown"),
                label(1, "unnamed"),
            ],
            SymbolKind::Configuration,
        ),
        "data" => (
            vec!["data".to_owned(), label(0, "unknown"), label(1, "unnamed")],
            SymbolKind::Configuration,
        ),
        "module" => (
            vec!["module".to_owned(), label(0, "unnamed")],
            SymbolKind::Module,
        ),
        "variable" => (
            vec!["var".to_owned(), label(0, "unnamed")],
            SymbolKind::Property,
        ),
        "output" => (
            vec!["output".to_owned(), label(0, "unnamed")],
            SymbolKind::Property,
        ),
        "provider" => (
            vec!["provider".to_owned(), label(0, "unnamed")],
            SymbolKind::Configuration,
        ),
        "run" if is_test_file(path) => (
            vec!["run".to_owned(), label(0, "unnamed")],
            SymbolKind::Test,
        ),
        "locals" | "terraform" => (vec![block_type.to_owned()], SymbolKind::Module),
        _ => {
            let mut segments = vec![block_type.to_owned()];
            segments.extend(labels.iter().cloned());
            (segments, SymbolKind::Configuration)
        }
    }
}

fn is_tfvars(path: &RepoRelativePath) -> bool {
    path.as_str().ends_with(".tfvars")
}

fn is_test_file(path: &RepoRelativePath) -> bool {
    let path = path.as_str();
    path.ends_with(".tftest.hcl")
        || path
            .split('/')
            .any(|component| matches!(component, "test" | "tests"))
}

pub(crate) fn module_path(path: &RepoRelativePath) -> Vec<String> {
    let mut components: Vec<String> = path
        .as_str()
        .split('/')
        .filter(|component| !component.is_empty())
        .map(str::to_owned)
        .collect();
    if let Some(last) = components.last_mut() {
        for suffix in [".tftest.hcl", ".tfvars", ".hcl", ".tf"] {
            if let Some(stem) = last.strip_suffix(suffix) {
                *last = stem.to_owned();
                break;
            }
        }
    }
    if components.is_empty() {
        components.push("configuration".to_owned());
    }
    components
}

pub struct HclParser {
    parser: Parser,
}

impl HclParser {
    pub fn new() -> Result<Self, ParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_hcl::LANGUAGE.into())
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
        let mut extraction = Extraction {
            path,
            source: source.as_ref(),
            line_starts: std::iter::once(0)
                .chain(source.match_indices('\n').map(|(index, _)| index + 1))
                .collect(),
            symbols: Vec::new(),
            calls: Vec::new(),
            named_relations: Vec::new(),
        };
        let module = extraction.symbols.len();
        extraction.symbols.push(SymbolDraft {
            key: SymbolKey {
                language: Language::Hcl,
                qualified_name: module_path.join("::"),
                container: module_path
                    .split_last()
                    .and_then(|(_, parent)| (!parent.is_empty()).then(|| parent.join("::"))),
                kind: SymbolKind::Module,
                path: extraction.path.clone(),
            },
            location: extraction.range(root)?,
            signature: Some("HCL configuration file".to_owned()),
            parent: None,
        });
        let context = Context {
            prefix: Vec::new(),
            container: Some(module_path.join("::")),
            parent: Some(module),
            caller: None,
            block_type: None,
        };
        extraction.walk(root, &context)?;
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

    #[test]
    fn parses_terraform_entities_imports_tests_and_references()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = RepoRelativePath::new("tests/main.tftest.hcl")?;
        let source: Arc<str> = Arc::from(
            r#"variable "region" { type = string }
locals { bucket_name = "chakra" }
resource "aws_s3_bucket" "logs" { bucket = local.bucket_name }
data "aws_ami" "ubuntu" { owners = ["self"] }
module "network" {
  source = "../network"
  region = var.region
}
output "bucket_id" { value = aws_s3_bucket.logs.id }
run "plans_bucket" {
  command = plan
  assert { condition = aws_s3_bucket.logs.id != "" }
}
"#,
        );
        let parsed = HclParser::new()?.parse(path, source)?;
        assert!(!parsed.has_errors, "diagnostics: {:?}", parsed.diagnostics);
        for expected in [
            "var::region",
            "local::bucket_name",
            "resource::aws_s3_bucket::logs",
            "data::aws_ami::ubuntu",
            "module::network",
            "output::bucket_id",
            "run::plans_bucket",
        ] {
            assert!(
                parsed
                    .symbols
                    .iter()
                    .any(|symbol| symbol.key.qualified_name == expected),
                "missing {expected}: {:?}",
                parsed.symbols
            );
        }
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| symbol.key.kind == SymbolKind::Import
                    && symbol.key.qualified_name.contains("module::network"))
        );
        assert!(parsed.calls.iter().any(|call| {
            call.name == "logs" && call.qualifier.as_deref() == Some("resource::aws_s3_bucket")
        }));
        assert!(
            parsed
                .calls
                .iter()
                .any(|call| { call.name == "region" && call.qualifier.as_deref() == Some("var") })
        );
        Ok(())
    }

    #[test]
    fn malformed_hcl_retains_valid_symbols_and_reports_diagnostics()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = RepoRelativePath::new("main.tf")?;
        let parsed = HclParser::new()?.parse(
            path,
            Arc::<str>::from(
                "resource \"null_resource\" \"retained_marker\" {}\nresource \"broken\" \"x\" {\n",
            ),
        )?;
        assert!(parsed.has_errors);
        assert!(parsed.diagnostic_count > 0);
        assert!(parsed.diagnostics.len() <= MAX_SYNTAX_DIAGNOSTICS_PER_FILE);
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.qualified_name == "resource::null_resource::retained_marker"
        }));
        Ok(())
    }
}
