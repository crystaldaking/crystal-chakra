//! Bounded syntax candidates for semantic provider queries. These facts do
//! not resolve a call: the provider must independently confirm its target.

use std::ops::ControlFlow;

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use thiserror::Error;
use tree_sitter::{Node, ParseOptions, Parser, Point};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_NODES: usize = 100_000;

pub use chakra_language_index::calls::{
    CallExpression as KotlinCallExpression, CallSyntax as KotlinCallSyntax,
    Callable as KotlinCallable,
};

#[derive(Debug, Error)]
pub enum CallSyntaxError {
    #[error("Kotlin call syntax exceeds its bounded query budget")]
    Budget,
    #[error("Kotlin call syntax query was cancelled")]
    Cancelled,
    #[error("Kotlin call syntax is malformed")]
    Malformed,
    #[error("invalid Kotlin source position")]
    Position,
    #[error("failed to load Kotlin grammar: {0}")]
    Grammar(String),
}

/// Parses only one immutable document, with bounded input and traversal.
/// The cancellation callback also runs from Tree-sitter's parse loop.
pub fn analyze_calls(
    path: &RepoRelativePath,
    source: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<KotlinCallSyntax, CallSyntaxError> {
    if cancelled() {
        return Err(CallSyntaxError::Cancelled);
    }
    if source.len() > MAX_BYTES {
        return Err(CallSyntaxError::Budget);
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
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
    let mut result = KotlinCallSyntax {
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
        if node.kind() == "function_declaration" {
            if let Some(name) = node.child_by_field_name("name") {
                caller = Some(result.callables.len());
                result.callables.push(KotlinCallable {
                    name: source[name.byte_range()].trim_matches('`').to_owned(),
                    declaration: range(node)?,
                    identifier: range(name)?,
                });
            }
        } else if matches!(node.kind(), "lambda_literal" | "anonymous_function") {
            // These bodies may execute after the enclosing function returns.
            // Without a separately represented callable, do not attribute
            // their invocations to the enclosing named function.
            caller = None;
            result.unsupported_calls.push(range(node)?);
        } else if matches!(node.kind(), "class_declaration" | "object_declaration") {
            // Functions declared inside this type establish their own owner;
            // its initializers must not leak into an enclosing function.
            caller = None;
        }
        if node.kind() == "call_expression" {
            if let Some(callee) = callee_identifier(node) {
                result.calls.push(KotlinCallExpression {
                    callee: range(callee)?,
                    selector: range(callee)?,
                    expression: range(node)?,
                    caller,
                });
            } else {
                result.unsupported_calls.push(range(node)?);
            }
        } else if matches!(
            node.kind(),
            "infix_expression"
                | "constructor_invocation"
                | "binary_expression"
                | "unary_expression"
                | "index_expression"
                | "assignment"
                | "for_statement"
                | "multi_variable_declaration"
        ) {
            // These forms can invoke Kotlin operators implicitly. The explicit
            // call driver cannot resolve them by identifier; report incomplete
            // coverage instead of a precise complete empty result.
            result.unsupported_calls.push(range(node)?);
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        pending.extend(children.into_iter().rev().map(|child| (child, caller)));
    }
    Ok(result)
}

fn callee_identifier(node: Node<'_>) -> Option<Node<'_>> {
    let callee = node.named_child(0)?;
    match callee.kind() {
        "identifier" => Some(callee),
        "navigation_expression" => {
            let mut cursor = callee.walk();
            callee
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "identifier")
                .last()
        }
        _ => None,
    }
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
    fn nested_call_owners_and_function_values_are_distinct()
    -> Result<(), Box<dyn std::error::Error>> {
        let syntax = analyze_calls(
            &RepoRelativePath::new("sample.kt")?,
            "fun outer() { target(::target); fun inner() { receiver?.target() }; inner() }",
            &mut || false,
        )?;
        assert_eq!(syntax.calls.len(), 3);
        let owners: Vec<_> = syntax
            .calls
            .iter()
            .map(|call| {
                call.caller
                    .map(|index| syntax.callables[index].name.as_str())
            })
            .collect();
        assert_eq!(owners, [Some("outer"), Some("inner"), Some("outer")]);
        assert!(syntax.calls[0].callee.end() < syntax.calls[0].expression.end());
        Ok(())
    }

    #[test]
    fn deferred_function_bodies_do_not_inherit_the_enclosing_owner()
    -> Result<(), Box<dyn std::error::Error>> {
        let syntax = analyze_calls(
            &RepoRelativePath::new("sample.kt")?,
            "fun factory() { val lambda = { target() }; val anonymous = fun() { target() }; target() }",
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
        assert_eq!(owners, [None, None, Some("factory")]);
        Ok(())
    }

    #[test]
    fn unicode_columns_and_cancellation_are_checked() -> Result<(), Box<dyn std::error::Error>> {
        let path = RepoRelativePath::new("unicode.kt")?;
        let syntax = analyze_calls(
            &path,
            "fun caller() { val emoji = \"😀\"; target() }",
            &mut || false,
        )?;
        assert_eq!(syntax.calls[0].callee.start().column(), 33);
        assert!(matches!(
            analyze_calls(&path, "fun x() {}", &mut || true),
            Err(CallSyntaxError::Cancelled)
        ));
        assert!(matches!(
            analyze_calls(&path, "fun x( {", &mut || false),
            Err(CallSyntaxError::Malformed)
        ));
        Ok(())
    }
}
