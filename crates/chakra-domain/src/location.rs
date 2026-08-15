//! Source positions, ranges, and repository-relative paths.
//!
//! Positions are 1-based: `line` counts lines, `column` counts Unicode
//! scalar values from the start of the line. Language-adapter output
//! (Tree-sitter rows, LSP UTF-16 positions) is converted at the adapter
//! boundary; core code never sees 0-based or UTF-16 positions.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A repository-relative file path using forward slashes.
///
/// Validated at construction: non-empty, not absolute, no `.`/`..`
/// components, no backslashes, no trailing slash (SPEC §38 traversal
/// protection).
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub struct RepoRelativePath(String);

/// Why a string is not a valid [`RepoRelativePath`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RepoPathError {
    #[error("path is empty")]
    Empty,
    #[error("path must be relative, got: {0}")]
    Absolute(String),
    #[error("path contains a `.` or `..` component: {0}")]
    DotComponent(String),
    #[error("path must use forward slashes: {0}")]
    Backslash(String),
    #[error("path must not end with a slash: {0}")]
    TrailingSlash(String),
}

impl RepoRelativePath {
    pub fn new(raw: impl Into<String>) -> Result<Self, RepoPathError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(RepoPathError::Empty);
        }
        if raw.starts_with('/') || raw.starts_with("~/") {
            return Err(RepoPathError::Absolute(raw));
        }
        if raw.contains('\\') {
            return Err(RepoPathError::Backslash(raw));
        }
        if raw.ends_with('/') {
            return Err(RepoPathError::TrailingSlash(raw));
        }
        if raw
            .split('/')
            .any(|component| component == "." || component == "..")
        {
            return Err(RepoPathError::DotComponent(raw));
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<RepoRelativePath> for String {
    fn from(path: RepoRelativePath) -> Self {
        path.0
    }
}

impl TryFrom<String> for RepoRelativePath {
    type Error = RepoPathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl fmt::Display for RepoRelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A 1-based position in a text file.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
pub struct TextPosition {
    pub line: u32,
    pub column: u32,
}

impl TextPosition {
    pub fn new(line: u32, column: u32) -> Result<Self, PositionError> {
        if line == 0 || column == 0 {
            return Err(PositionError { line, column });
        }
        Ok(Self { line, column })
    }
}

/// Why a position is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("positions are 1-based, got line {line}, column {column}")]
pub struct PositionError {
    pub line: u32,
    pub column: u32,
}

/// A half-open range within one file: `start` inclusive, `end` exclusive.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SourceRange {
    pub file: RepoRelativePath,
    pub start: TextPosition,
    pub end: TextPosition,
}

/// Why a range is invalid.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RangeError {
    #[error("invalid start/end position: {0}")]
    Position(#[from] PositionError),
    #[error("range end {end:?} precedes start {start:?}")]
    EndBeforeStart {
        start: TextPosition,
        end: TextPosition,
    },
}

impl SourceRange {
    pub fn new(
        file: RepoRelativePath,
        start: TextPosition,
        end: TextPosition,
    ) -> Result<Self, RangeError> {
        if end < start {
            return Err(RangeError::EndBeforeStart { start, end });
        }
        Ok(Self { file, start, end })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_relative_paths() -> Result<(), RepoPathError> {
        let path = RepoRelativePath::new("src/api/refund.rs")?;
        assert_eq!(path.as_str(), "src/api/refund.rs");
        Ok(())
    }

    #[test]
    fn rejects_unsafe_or_noncanonical_paths() {
        for raw in [
            "",
            "/etc/passwd",
            "../outside.rs",
            "src/../secret.rs",
            "./src/lib.rs",
            "src/./lib.rs",
            "src\\lib.rs",
            "src/",
        ] {
            assert!(
                RepoRelativePath::new(raw).is_err(),
                "expected rejection: {raw}"
            );
        }
    }

    #[test]
    fn positions_are_one_based() {
        assert!(TextPosition::new(0, 1).is_err());
        assert!(TextPosition::new(1, 0).is_err());
        assert!(TextPosition::new(1, 1).is_ok());
    }

    #[test]
    fn range_must_not_end_before_it_starts() -> Result<(), RepoPathError> {
        let file = RepoRelativePath::new("src/lib.rs")?;
        let start = TextPosition { line: 5, column: 3 };
        let end = TextPosition { line: 5, column: 2 };
        assert!(SourceRange::new(file.clone(), start, end).is_err());
        let same_line_ok = TextPosition { line: 5, column: 3 };
        assert!(SourceRange::new(file, start, same_line_ok).is_ok());
        Ok(())
    }

    #[test]
    fn path_serde_rejects_invalid_values() {
        let result = serde_json::from_str::<RepoRelativePath>("\"../nope.rs\"");
        assert!(result.is_err());
    }
}
