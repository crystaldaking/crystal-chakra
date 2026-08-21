// Production service module. The text-search scenario looks for the needle
// comment below in exactly this file.

// CONFORMANCE_TEXT_NEEDLE: payment pipeline marker

import { shared_unique_target } from "./shared.js";
// Deliberate hard case: a CommonJS require binding with a destructured
// alias, mixed into an ES module file (ADR-0034).
const { record_conformance_event: audit_event } = require("./shared.js");

/** Uniquely named caller of `shared::shared_unique_target`. */
export function dispatch_conformance_request() {
    shared_unique_target();
    audit_event("dispatch");
}
