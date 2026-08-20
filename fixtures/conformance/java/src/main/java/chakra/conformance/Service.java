// Production service class. The text-search scenario looks for the needle
// comment below in exactly this file.

// CONFORMANCE_TEXT_NEEDLE: payment pipeline marker

package chakra.conformance;

import chakra.conformance.shared.Shared;

import static chakra.conformance.shared.Shared.record_conformance_event;

/** Uniquely named caller of `Shared::shared_unique_target`. */
public class Service {

    public String dispatch_conformance_request() {
        Shared.shared_unique_target();
        record_conformance_event("dispatch");
        return "dispatched";
    }
}
