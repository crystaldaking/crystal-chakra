//! Shared helpers with deliberately unique names for caller/callee assertions.

/// Uniquely named callee: exactly one caller (`service::dispatch_conformance_request`).
pub fn shared_unique_target() {}

/// High-degree callee: called by every `fan_in::fan_in_caller_*` function.
pub fn record_conformance_event(label: &str) {
    let _ = label;
}
