//! Tree-sitter Bash extraction into language-neutral Chakra drafts.
//!
//! The syntax model is deliberately conservative. Shell functions are
//! declarations; `source`/`.` and `alias` commands are import facts; and a
//! statically spelled command inside a function is a bounded function-call
//! candidate. Dynamic command names, path invocations, and commands reached
//! through `command`/`builtin` are left to the optional precise provider.

use std::num::TryFromIntError;
use std::sync::Arc;

use chakra_domain::diagnostic::{
    MAX_SYNTAX_DIAGNOSTICS_PER_FILE, SyntaxDiagnostic, SyntaxDiagnosticCause, SyntaxDiagnosticKind,
};
use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{CallForm, CallTargetKind, Language, SymbolKey, SymbolKind};
pub(crate) use chakra_language_index::facts::{CallDraft, ParsedFile, SymbolDraft};
use thiserror::Error;
use tree_sitter::{Node, Parser, Point};

const MAX_SIGNATURE_CHARS: usize = 512;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("failed to load the Tree-sitter Bash grammar: {0}")]
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
    callable: Option<usize>,
}

#[derive(Debug)]
struct Extraction<'a> {
    path: RepoRelativePath,
    source: &'a str,
    line_starts: Vec<usize>,
    symbols: Vec<SymbolDraft>,
    calls: Vec<CallDraft>,
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
                        language: Language::Shell,
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
                            language: Language::Shell,
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
                language: Language::Shell,
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

    fn walk(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        match node.kind() {
            "function_definition" => self.function(node, context),
            "command" => {
                self.command(node, context)?;
                self.walk_named_children(node, context)
            }
            _ => self.walk_named_children(node, context),
        }
    }

    fn walk_named_children(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        for index in 0..node.named_child_count() {
            let index = u32::try_from(index).map_err(|error| ParseError::PositionInteger {
                path: self.path.clone(),
                source: error,
            })?;
            if let Some(child) = node.named_child(index) {
                self.walk(child, context)?;
            }
        }
        Ok(())
    }

    fn function(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name_node) = node.child_by_field_name("name") else {
            return Ok(());
        };
        let Some(name) = self
            .text(name_node)
            .map(str::trim)
            .filter(|name| is_static_name(name))
        else {
            return Ok(());
        };
        let name = name.to_owned();
        let kind = if is_test_function(&self.path, &name) {
            SymbolKind::Test
        } else {
            SymbolKind::Function
        };
        let symbol = self.add_symbol(context, &name, kind, name_node, self.signature(node))?;
        let qualified = Self::qualified(&context.prefix, &name);
        let mut prefix = context.prefix.clone();
        prefix.push(name);
        let nested = Context {
            prefix,
            container: Some(qualified),
            parent: Some(symbol),
            callable: Some(symbol),
        };
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, &nested)?;
        }
        Ok(())
    }

    fn command(&mut self, node: Node<'_>, context: &Context) -> Result<(), ParseError> {
        let Some(name_node) = node.child_by_field_name("name") else {
            return Ok(());
        };
        let Some(raw_name) = self.text(name_node).map(str::trim).map(str::to_owned) else {
            return Ok(());
        };
        if matches!(raw_name.as_str(), "source" | "." | "alias") {
            self.import(node, context, &raw_name)?;
            return Ok(());
        }
        let Some(caller) = context.callable else {
            return Ok(());
        };
        if !is_call_candidate(&raw_name) {
            return Ok(());
        }
        self.calls.push(CallDraft {
            promoted: false,
            caller,
            form: CallForm::Function,
            target_kind: CallTargetKind::Function,
            name: raw_name,
            qualifier: None,
            receiver_hint: None,
            location: self.range(name_node)?,
        });
        Ok(())
    }

    fn import(
        &mut self,
        node: Node<'_>,
        context: &Context,
        command: &str,
    ) -> Result<(), ParseError> {
        let Some(argument) = node.child_by_field_name("argument") else {
            return Ok(());
        };
        let Some(raw) = self.text(argument).map(str::trim).map(str::to_owned) else {
            return Ok(());
        };
        let raw = if command == "alias" {
            raw.split_once('=').map_or(raw.as_str(), |(name, _)| name)
        } else {
            raw.as_str()
        };
        let name = import_name(raw);
        if name.is_empty() {
            return Ok(());
        }
        let ordinal = self.import_ordinal;
        self.import_ordinal = self.import_ordinal.saturating_add(1);
        let key_name = format!("{name}_{ordinal}");
        self.add_symbol(
            context,
            &key_name,
            SymbolKind::Import,
            argument,
            Some(format!("{command} {raw}")),
        )?;
        Ok(())
    }
}

fn is_static_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ':')
        })
}

fn is_call_candidate(name: &str) -> bool {
    is_static_name(name)
        && !matches!(
            name,
            "alias"
                | "bg"
                | "break"
                | "builtin"
                | "cd"
                | "command"
                | "continue"
                | "declare"
                | "dirs"
                | "disown"
                | "echo"
                | "enable"
                | "eval"
                | "exec"
                | "exit"
                | "export"
                | "false"
                | "fc"
                | "fg"
                | "getopts"
                | "hash"
                | "help"
                | "history"
                | "jobs"
                | "kill"
                | "let"
                | "local"
                | "logout"
                | "mapfile"
                | "popd"
                | "printf"
                | "pushd"
                | "pwd"
                | "read"
                | "readonly"
                | "return"
                | "set"
                | "shift"
                | "shopt"
                | "source"
                | "suspend"
                | "test"
                | "times"
                | "trap"
                | "true"
                | "type"
                | "typeset"
                | "ulimit"
                | "umask"
                | "unalias"
                | "unset"
                | "wait"
        )
}

fn import_name(raw: &str) -> String {
    raw.trim_matches(|character| matches!(character, '\'' | '"'))
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_owned()
}

fn is_test_function(path: &RepoRelativePath, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let named_test = lower.starts_with("test_")
        || lower.ends_with("_test")
        || lower.starts_with("spec_")
        || lower.ends_with("_spec");
    let test_path = path.as_str().split('/').any(|component| {
        component.eq_ignore_ascii_case("test")
            || component.eq_ignore_ascii_case("tests")
            || component.eq_ignore_ascii_case("spec")
            || component.eq_ignore_ascii_case("specs")
    });
    named_test || (test_path && lower.contains("test"))
}

fn module_path(path: &RepoRelativePath) -> Vec<String> {
    let mut components: Vec<String> = path
        .as_str()
        .split('/')
        .filter(|component| !component.is_empty())
        .map(str::to_owned)
        .collect();
    if let Some(last) = components.last_mut()
        && let Some((stem, _)) = last.rsplit_once('.')
    {
        *last = stem.to_owned();
    }
    if components.is_empty() {
        components.push("script".to_owned());
    }
    components
}

/// Stateful parser; each bounded worker owns one instance.
pub struct ShellParser {
    parser: Parser,
}

impl ShellParser {
    pub fn new() -> Result<Self, ParseError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
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
        let module_path = module_path(&path);
        let mut extraction = Extraction {
            path,
            source: source.as_ref(),
            line_starts: std::iter::once(0)
                .chain(source.match_indices('\n').map(|(index, _)| index + 1))
                .collect(),
            symbols: Vec::new(),
            calls: Vec::new(),
            import_ordinal: 0,
        };
        let module_name = module_path
            .last()
            .cloned()
            .unwrap_or_else(|| "script".to_owned());
        let module_index = extraction.symbols.len();
        extraction.symbols.push(SymbolDraft {
            key: SymbolKey {
                language: Language::Shell,
                qualified_name: module_path.join("::"),
                container: module_path
                    .split_last()
                    .and_then(|(_, parent)| (!parent.is_empty()).then(|| parent.join("::"))),
                kind: SymbolKind::Module,
                path: extraction.path.clone(),
            },
            location: extraction.range(root)?,
            signature: Some(format!("shell script {module_name}")),
            parent: None,
        });
        let context = Context {
            prefix: module_path.clone(),
            container: Some(module_path.join("::")),
            parent: Some(module_index),
            callable: None,
        };
        extraction.walk(root, &context)?;
        let (diagnostics, diagnostic_count) = extraction.diagnostics(root)?;
        Ok(ParsedFile {
            source: source.clone(),
            module_path,
            symbols: extraction.symbols,
            calls: extraction.calls,
            named_relations: Vec::new(),
            has_errors: root.has_error(),
            diagnostics,
            diagnostic_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(symbol: &SymbolDraft) -> &str {
        symbol
            .key
            .qualified_name
            .rsplit("::")
            .next()
            .unwrap_or(symbol.key.qualified_name.as_str())
    }

    #[test]
    fn parses_functions_sources_aliases_tests_and_calls() -> Result<(), Box<dyn std::error::Error>>
    {
        let path = RepoRelativePath::new("src/tools/refund.sh")?;
        let source: Arc<str> = Arc::from(
            r#"#!/usr/bin/env bash
source "lib/shared_import.sh"
alias refund_alias='refund_impl'

refund_impl() {
  printf '%s\n' "$1"
}

dispatch_refund() {
  refund_impl "$@"
  command refund_impl "$@"
  "$DYNAMIC_COMMAND" "$@"
}

test_refund_flow() {
  dispatch_refund sample
}
"#,
        );
        let mut parser = ShellParser::new()?;
        let parsed = parser.parse(path, source)?;
        assert!(!parsed.has_errors, "diagnostics: {:?}", parsed.diagnostics);
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.kind == SymbolKind::Module
                && symbol.key.qualified_name == "src::tools::refund"
        }));
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.kind == SymbolKind::Function && name(symbol) == "dispatch_refund"
        }));
        assert!(parsed.symbols.iter().any(|symbol| {
            symbol.key.kind == SymbolKind::Test && name(symbol) == "test_refund_flow"
        }));
        assert_eq!(
            parsed
                .symbols
                .iter()
                .filter(|symbol| symbol.key.kind == SymbolKind::Import)
                .count(),
            2
        );
        let calls: Vec<&str> = parsed.calls.iter().map(|call| call.name.as_str()).collect();
        assert_eq!(calls, ["refund_impl", "dispatch_refund"]);
        Ok(())
    }

    #[test]
    fn nested_functions_keep_syntax_containers() -> Result<(), Box<dyn std::error::Error>> {
        let path = RepoRelativePath::new("nested.sh")?;
        let source: Arc<str> =
            Arc::from("outer_container() { inner_marker() { true; }; inner_marker; }\n");
        let mut parser = ShellParser::new()?;
        let parsed = parser.parse(path, source)?;
        let inner = parsed
            .symbols
            .iter()
            .find(|symbol| name(symbol) == "inner_marker")
            .ok_or("inner function missing")?;
        assert_eq!(
            inner.key.container.as_deref(),
            Some("nested::outer_container")
        );
        assert!(inner.parent.is_some());
        Ok(())
    }

    #[test]
    fn malformed_shell_reports_bounded_diagnostics() -> Result<(), Box<dyn std::error::Error>> {
        let path = RepoRelativePath::new("broken.sh")?;
        let source: Arc<str> = Arc::from("retained_marker() { true; }\nbroken() { if true; then\n");
        let mut parser = ShellParser::new()?;
        let parsed = parser.parse(path, source)?;
        assert!(parsed.has_errors);
        assert!(parsed.diagnostic_count > 0);
        assert!(parsed.diagnostics.len() <= MAX_SYNTAX_DIAGNOSTICS_PER_FILE);
        assert!(
            parsed
                .symbols
                .iter()
                .any(|symbol| name(symbol) == "retained_marker")
        );
        Ok(())
    }
}
