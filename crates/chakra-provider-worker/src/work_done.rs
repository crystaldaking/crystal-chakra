//! Bounded observation of server-owned LSP work. Readiness policy belongs to
//! each adapter; an idle epoch proves only that observed work has completed.

use lsp_types::ProgressToken;
use serde_json::Value;

const MAX_ACTIVE_TOKENS: usize = 64;
const MAX_TOKEN_BYTES: usize = 256;

#[derive(Default)]
pub(crate) struct WorkDoneTracker {
    active: Vec<ProgressToken>,
    epoch: u64,
    completed: bool,
    overflowed: bool,
}

impl WorkDoneTracker {
    pub(crate) fn idle_epoch(&self) -> Option<u64> {
        (self.completed && self.active.is_empty() && !self.overflowed).then_some(self.epoch)
    }

    pub(crate) fn observe(&mut self, params: &Value) {
        let Some(kind) = params.pointer("/value/kind").and_then(Value::as_str) else {
            return;
        };
        if !matches!(kind, "begin" | "end") {
            return;
        }
        let Some(token) = params.get("token") else {
            return;
        };
        if token
            .as_str()
            .is_some_and(|token| token.len() > MAX_TOKEN_BYTES)
        {
            self.overflowed = true;
            return;
        }
        let Ok(token) = serde_json::from_value::<ProgressToken>(token.clone()) else {
            return;
        };
        if kind == "begin" {
            if self.active.contains(&token) {
                return;
            }
            if self.active.len() == MAX_ACTIVE_TOKENS || self.epoch == u64::MAX {
                self.overflowed = true;
                return;
            }
            self.epoch += 1;
            self.active.push(token);
        } else if let Some(index) = self.active.iter().position(|active| *active == token) {
            self.active.swap_remove(index);
            self.completed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_work_requires_all_tokens_to_end_and_changes_the_epoch() {
        let mut tracker = WorkDoneTracker::default();
        assert_eq!(tracker.idle_epoch(), None);
        tracker.observe(&json!({"token": "unknown", "value": {"kind": "end"}}));
        assert_eq!(tracker.idle_epoch(), None);
        tracker.observe(&json!({"token": "import", "value": {"kind": "begin"}}));
        tracker.observe(&json!({"token": 1, "value": {"kind": "begin"}}));
        tracker.observe(&json!({"token": 1, "value": {"kind": "begin"}}));
        tracker.observe(&json!({"token": "import", "value": {"kind": "end"}}));
        assert_eq!(tracker.idle_epoch(), None);
        tracker.observe(&json!({"token": 1, "value": {"kind": "end"}}));
        assert_eq!(tracker.idle_epoch(), Some(2));
        tracker.observe(&json!({"token": "import", "value": {"kind": "begin"}}));
        assert_eq!(tracker.idle_epoch(), None);
        tracker.observe(&json!({"token": "import", "value": {"kind": "end"}}));
        assert_eq!(tracker.idle_epoch(), Some(3));
    }

    #[test]
    fn token_overflow_is_bounded_and_cannot_establish_readiness() {
        let mut tracker = WorkDoneTracker::default();
        for token in 0..=MAX_ACTIVE_TOKENS {
            tracker.observe(&json!({"token": token, "value": {"kind": "begin"}}));
        }
        assert_eq!(tracker.active.len(), MAX_ACTIVE_TOKENS);
        for token in 0..=MAX_ACTIVE_TOKENS {
            tracker.observe(&json!({"token": token, "value": {"kind": "end"}}));
        }
        assert_eq!(tracker.idle_epoch(), None);
        let mut tracker = WorkDoneTracker::default();
        tracker.observe(
            &json!({"token": "x".repeat(MAX_TOKEN_BYTES + 1), "value": {"kind": "begin"}}),
        );
        assert!(tracker.active.is_empty());
        assert_eq!(tracker.idle_epoch(), None);
    }
}
