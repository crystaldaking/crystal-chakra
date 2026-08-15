//! Workspace revision identity.
//!
//! A revision identifies one atomically published workspace state
//! (SPEC §5). Queries observe exactly one revision.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Monotonically increasing workspace revision.
///
/// Revision 0 is the empty initial state; the first published update has
/// revision 1.
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
pub struct Revision(pub u64);

impl Revision {
    /// Revision of a fresh, never-updated workspace.
    pub const INITIAL: Revision = Revision(0);

    /// The revision that follows this one.
    pub fn next(self) -> Revision {
        Revision(self.0 + 1)
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_revision_precedes_first_publish() {
        assert!(Revision::INITIAL < Revision::INITIAL.next());
        assert_eq!(Revision::INITIAL.next(), Revision(1));
    }

    #[test]
    fn next_is_monotonic() {
        let mut rev = Revision::INITIAL;
        for expected in 1..=10 {
            rev = rev.next();
            assert_eq!(rev, Revision(expected));
        }
    }
}
