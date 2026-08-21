//! Production service module. The text-search scenario looks for the needle
//! comment below in exactly this file.

// CONFORMANCE_TEXT_NEEDLE: payment pipeline marker

use crate::shared::record_conformance_event as audit_event;
use crate::shared::shared_unique_target;

/// Uniquely named caller of `shared::shared_unique_target`.
pub fn dispatch_conformance_request() {
    shared_unique_target();
    audit_event("dispatch");
}
