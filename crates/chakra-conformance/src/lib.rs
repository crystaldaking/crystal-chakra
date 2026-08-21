//! Cross-language conformance harness (CONFORM-01, issue #24).
//!
//! The harness replays a fixed catalog of conformance scenarios against a
//! real temporary Git worktree seeded from `fixtures/conformance/<language>`,
//! driving everything through the public
//! [`chakra_domain::query::QueryService`] surface of a live
//! [`chakra_engine::WorkspaceEngine`]. Per-language expectations come from the
//! data-driven `manifest.json` inside each fixture directory, so adding a
//! language requires no code changes.
//!
//! ## Result file schema (version 1)
//!
//! `chakra-conformance emit <dir>` writes one `<language>.json` per language:
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "language": "rust",
//!   "scenario_count": 14,
//!   "passed": 14,
//!   "failed": 0,
//!   "scenarios": [
//!     {
//!       "id": "ambiguity",
//!       "description": "Duplicate symbol names ...",
//!       "capability_ids": ["AMBIG-01", "PROV-01"],
//!       "status": "pass",
//!       "provenance_assertions": ["symbol facts: tree_sitter/syntax"],
//!       "details": ""
//!     }
//!   ]
//! }
//! ```
//!
//! - `status` is `pass` or `fail`; `details` is empty on pass and carries the
//!   failure message on fail.
//! - `provenance_assertions` records the provenance/precision checks the
//!   scenario actually performed (PROV-01 evidence).
//! - Emission is deterministic: fixed field order, scenario catalog order, and
//!   no timestamps, so re-running `emit` is byte-identical.

pub mod corpus;
mod fixture;
mod manifest;
mod provider;
mod report;
mod runner;
mod scenarios;

use std::error::Error;
use std::fmt;

pub use fixture::LiveFixture;
pub use manifest::{Expectations, Manifest, ScenarioSpec};
pub use provider::FlakyProvider;
pub use report::{LanguageReport, ScenarioReport, ScenarioStatus};
pub use runner::{
    fixtures_root, languages, load_manifest, run_language, run_scenario, validate_manifest,
};

/// Result type used across the harness. Scenario failures are ordinary
/// errors; the runner converts them into `fail` scenario reports.
pub type Check<T> = Result<T, Box<dyn Error>>;

/// A single failed conformance expectation.
#[derive(Debug)]
pub struct CheckFailure(String);

impl fmt::Display for CheckFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CheckFailure {}

/// Returns `Err(CheckFailure)` with `message` when `condition` does not hold.
pub fn ensure(condition: bool, message: impl Into<String>) -> Result<(), CheckFailure> {
    if condition {
        Ok(())
    } else {
        Err(CheckFailure(message.into()))
    }
}

/// Builds a [`CheckFailure`] from a formatted message.
pub fn failure(message: impl Into<String>) -> CheckFailure {
    CheckFailure(message.into())
}
