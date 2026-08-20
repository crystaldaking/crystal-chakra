// Production service module. The text-search scenario looks for the needle
// comment below in exactly this file.

// CONFORMANCE_TEXT_NEEDLE: payment pipeline marker

import { record_conformance_event as audit_event } from "./shared";
import { shared_unique_target } from "./shared";

/** Uniquely named caller of `shared::shared_unique_target`. */
export function dispatch_conformance_request(): void {
    shared_unique_target();
    audit_event("dispatch");
}
