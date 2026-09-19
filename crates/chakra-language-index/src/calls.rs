//! Bounded per-document call candidates used by semantic provider adapters.
//! A candidate is syntax only; a provider must independently establish binding.

use chakra_domain::location::SourceRange;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callable {
    pub name: String,
    pub declaration: SourceRange,
    pub identifier: SourceRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallExpression {
    pub callee: SourceRange,
    /// Qualified selector, including its receiver but excluding arguments.
    /// Some providers return this span for references to Java method calls.
    pub selector: SourceRange,
    pub expression: SourceRange,
    /// None for an initializer without a supported named callable owner.
    pub caller: Option<usize>,
}

#[derive(Debug)]
pub struct CallSyntax {
    pub callables: Vec<Callable>,
    pub calls: Vec<CallExpression>,
    pub unsupported_calls: Vec<SourceRange>,
}
