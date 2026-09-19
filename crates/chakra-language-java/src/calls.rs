//! Bounded syntax candidates for semantic provider queries. These facts do
//! not resolve a call: the provider must independently confirm its target.

use std::ops::ControlFlow;

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use thiserror::Error;
use tree_sitter::{Node, ParseOptions, Parser, Point};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_NODES: usize = 100_000;

pub use chakra_language_index::calls::{CallExpression, CallSyntax, Callable};

#[derive(Debug, Error)]
pub enum CallSyntaxError {
    #[error("Java call syntax exceeds its bounded query budget")]
    Budget,
    #[error("Java call syntax query was cancelled")]
    Cancelled,
    #[error("Java call syntax is malformed")]
    Malformed,
    #[error("invalid Java source position")]
    Position,
    #[error("failed to load Java grammar: {0}")]
    Grammar(String),
}

/// Parses only one immutable document, with bounded input and traversal.
/// The cancellation callback also runs from Tree-sitter's parse loop.
pub fn analyze_calls(
    path: &RepoRelativePath,
    source: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<CallSyntax, CallSyntaxError> {
    if cancelled() {
        return Err(CallSyntaxError::Cancelled);
    }
    if source.len() > MAX_BYTES {
        return Err(CallSyntaxError::Budget);
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .map_err(|error| CallSyntaxError::Grammar(error.to_string()))?;
    let mut progress = |_: &tree_sitter::ParseState| {
        if cancelled() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let tree = parser
        .parse_with_options(
            &mut |offset, _| source.as_bytes().get(offset..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        )
        .ok_or(CallSyntaxError::Cancelled)?;
    if tree.root_node().has_error() {
        return Err(CallSyntaxError::Malformed);
    }
    let starts: Vec<_> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(byte, _)| byte + 1))
        .collect();
    let range = |node: Node<'_>| -> Result<SourceRange, CallSyntaxError> {
        SourceRange::new(
            path.clone(),
            position(source, &starts, node.start_position())?,
            position(source, &starts, node.end_position())?,
        )
        .map_err(|_| CallSyntaxError::Position)
    };
    let mut result = CallSyntax {
        callables: Vec::new(),
        calls: Vec::new(),
        unsupported_calls: Vec::new(),
    };
    let mut pending = vec![(tree.root_node(), None)];
    let mut visited = 0;
    while let Some((node, mut caller)) = pending.pop() {
        visited += 1;
        if visited > MAX_NODES {
            return Err(CallSyntaxError::Budget);
        }
        if visited % 128 == 0 && cancelled() {
            return Err(CallSyntaxError::Cancelled);
        }
        if matches!(
            node.kind(),
            "method_declaration" | "constructor_declaration" | "compact_constructor_declaration"
        ) {
            if let Some(name) = node.child_by_field_name("name") {
                caller = Some(result.callables.len());
                result.callables.push(Callable {
                    name: source[name.byte_range()].to_owned(),
                    declaration: range(node)?,
                    identifier: range(name)?,
                });
            }
        } else if matches!(
            node.kind(),
            "class_body"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "annotation_type_declaration"
                | "lambda_expression"
        ) {
            // Functions declared inside this type establish their own owner;
            // its initializers must not leak into an enclosing function.
            caller = None;
        }
        if node.kind() == "method_invocation" {
            if let Some(callee) = callee_identifier(node) {
                result.calls.push(CallExpression {
                    callee: range(callee)?,
                    selector: SourceRange::new(
                        path.clone(),
                        position(source, &starts, node.start_position())?,
                        position(source, &starts, callee.end_position())?,
                    )
                    .map_err(|_| CallSyntaxError::Position)?,
                    expression: range(node)?,
                    caller,
                });
            } else {
                result.unsupported_calls.push(range(node)?);
            }
        } else if matches!(
            node.kind(),
            "object_creation_expression" | "explicit_constructor_invocation"
        ) {
            result.unsupported_calls.push(range(node)?);
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        pending.extend(children.into_iter().rev().map(|child| (child, caller)));
    }
    Ok(result)
}

fn callee_identifier(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("name")
}

fn position(source: &str, starts: &[usize], point: Point) -> Result<TextPosition, CallSyntaxError> {
    let start = *starts.get(point.row).ok_or(CallSyntaxError::Position)?;
    let prefix = source
        .get(start..start + point.column)
        .ok_or(CallSyntaxError::Position)?;
    TextPosition::new(
        u32::try_from(point.row + 1).map_err(|_| CallSyntaxError::Position)?,
        u32::try_from(prefix.chars().count() + 1).map_err(|_| CallSyntaxError::Position)?,
    )
    .map_err(|_| CallSyntaxError::Position)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_owners_exclude_method_values_and_unowned_lambdas()
    -> Result<(), Box<dyn std::error::Error>> {
        let syntax = analyze_calls(
            &RepoRelativePath::new("C.java")?,
            r#"
class C {
    void outer() {
        MainKt.target();
        Runnable value = MainKt::target;
        class Local { void inner() { MainKt.target(); } }
        Runnable lambda = () -> MainKt.target();
        Object object = new Object() { void anonymous() { MainKt.target(); } };
    }
}
"#,
            &mut || false,
        )?;
        let owners: Vec<_> = syntax
            .calls
            .iter()
            .map(|call| {
                call.caller
                    .map(|index| syntax.callables[index].name.as_str())
            })
            .collect();
        assert_eq!(
            owners,
            [Some("outer"), Some("inner"), None, Some("anonymous")]
        );
        assert!(syntax.calls[0].selector.start() < syntax.calls[0].callee.start());
        assert_eq!(syntax.calls[0].selector.end(), syntax.calls[0].callee.end());
        assert!(syntax.calls[0].selector.end() < syntax.calls[0].expression.end());
        Ok(())
    }

    #[test]
    fn unicode_cancellation_and_malformed_input_are_bounded()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = RepoRelativePath::new("C.java")?;
        let syntax = analyze_calls(
            &path,
            "class C { void m() {\n    String s = \"😀\"; MainKt.target();\n} }",
            &mut || false,
        )?;
        assert_eq!(syntax.calls[0].callee.start(), TextPosition::new(2, 28)?);
        assert!(matches!(
            analyze_calls(&path, "class C {}", &mut || true),
            Err(CallSyntaxError::Cancelled)
        ));
        assert!(matches!(
            analyze_calls(&path, "class C { void m( {", &mut || false),
            Err(CallSyntaxError::Malformed)
        ));
        Ok(())
    }
}
