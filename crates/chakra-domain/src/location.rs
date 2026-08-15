//! Source positions, ranges, and repository-relative paths.
//!
//! Positions are 1-based: `line` counts lines, `column` counts Unicode
//! scalar values from the start of the line. Language-adapter output
//! (Tree-sitter rows, LSP UTF-16 positions) is converted at the adapter
//! boundary; core code never sees 0-based or UTF-16 positions.
//!
//! Invariants are enforced by construction: fields are private and
//! deserialization validates, so a zero position or a reversed range cannot
//! exist in core code regardless of origin.

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
        if raw.split('/').any(|c| c == "." || c == "..") {
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

/// A 1-based position in a text file. Fields are private; construct via
/// [`TextPosition::new`] or validated deserialization.
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
#[serde(try_from = "TextPositionWire")]
#[schemars(with = "TextPositionWire")]
pub struct TextPosition {
    line: u32,
    column: u32,
}

/// Deserialization shape for [`TextPosition`]; validated on conversion.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
struct TextPositionWire {
    line: u32,
    column: u32,
}

/// Why a position is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("positions are 1-based, got line {line}, column {column}")]
pub struct PositionError {
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

    pub fn line(&self) -> u32 {
        self.line
    }

    pub fn column(&self) -> u32 {
        self.column
    }
}

impl TryFrom<TextPositionWire> for TextPosition {
    type Error = PositionError;

    fn try_from(wire: TextPositionWire) -> Result<Self, Self::Error> {
        Self::new(wire.line, wire.column)
    }
}

/// A half-open range within one file: `start` inclusive, `end` exclusive.
/// Construct via [`SourceRange::new`] or validated deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(try_from = "SourceRangeWire")]
#[schemars(with = "SourceRangeWire")]
pub struct SourceRange {
    file: RepoRelativePath,
    start: TextPosition,
    end: TextPosition,
}

/// Deserialization shape for [`SourceRange`]; validated on conversion
/// (nested positions validate themselves first).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct SourceRangeWire {
    file: RepoRelativePath,
    start: TextPosition,
    end: TextPosition,
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

    pub fn file(&self) -> &RepoRelativePath {
        &self.file
    }

    pub fn start(&self) -> TextPosition {
        self.start
    }

    pub fn end(&self) -> TextPosition {
        self.end
    }
}

impl TryFrom<SourceRangeWire> for SourceRange {
    type Error = RangeError;

    fn try_from(wire: SourceRangeWire) -> Result<Self, Self::Error> {
        Self::new(wire.file, wire.start, wire.end)
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
    fn path_serde_rejects_invalid_values() {
        let result = serde_json::from_str::<RepoRelativePath>("\"../nope.rs\"");
        assert!(result.is_err());
    }

    #[test]
    fn positions_are_one_based() {
        assert!(TextPosition::new(0, 1).is_err());
        assert!(TextPosition::new(1, 0).is_err());
        assert!(TextPosition::new(1, 1).is_ok());
    }

    #[test]
    fn zero_position_is_rejected_by_deserialization() {
        let result = serde_json::from_str::<TextPosition>(r#"{"line":0,"column":4}"#);
        assert!(result.is_err());
        let ok = serde_json::from_str::<TextPosition>(r#"{"line":2,"column":4}"#);
        assert!(ok.is_ok());
    }

    #[test]
    fn range_must_not_end_before_it_starts() -> Result<(), Box<dyn std::error::Error>> {
        let file = RepoRelativePath::new("src/lib.rs")?;
        let start = TextPosition::new(5, 3)?;
        let end = TextPosition::new(5, 2)?;
        assert!(matches!(
            SourceRange::new(file.clone(), start, end),
            Err(RangeError::EndBeforeStart { .. })
        ));
        assert!(SourceRange::new(file, start, TextPosition::new(5, 3)?).is_ok());
        Ok(())
    }

    #[test]
    fn reversed_range_is_rejected_by_deserialization() -> Result<(), Box<dyn std::error::Error>> {
        let raw =
            r#"{"file":"src/lib.rs","start":{"line":9,"column":1},"end":{"line":2,"column":1}}"#;
        let result = serde_json::from_str::<SourceRange>(raw);
        assert!(result.is_err());

        let ok = serde_json::from_str::<SourceRange>(
            r#"{"file":"src/lib.rs","start":{"line":2,"column":1},"end":{"line":9,"column":1}}"#,
        )?;
        assert_eq!(ok.start().line(), 2);
        assert_eq!(ok.end().line(), 9);
        assert_eq!(ok.file().as_str(), "src/lib.rs");
        Ok(())
    }

    #[test]
    fn range_serialization_shape_is_unchanged() -> Result<(), Box<dyn std::error::Error>> {
        let range = SourceRange::new(
            RepoRelativePath::new("src/lib.rs")?,
            TextPosition::new(2, 1)?,
            TextPosition::new(9, 5)?,
        )?;
        let json = serde_json::to_value(&range)?;
        assert_eq!(
            json,
            serde_json::json!({
                "file": "src/lib.rs",
                "start": { "line": 2, "column": 1 },
                "end": { "line": 9, "column": 5 }
            })
        );
        Ok(())
    }
}
