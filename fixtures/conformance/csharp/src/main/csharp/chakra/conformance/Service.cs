// Production service class. The text-search scenario looks for the needle
// comment below in exactly this file.

// CONFORMANCE_TEXT_NEEDLE: payment pipeline marker

namespace chakra.conformance;

using chakra.conformance.shared;

using record_conformance_event = chakra.conformance.shared.Shared;

using static chakra.conformance.shared.Shared;

/** Uniquely named caller of `Shared::shared_unique_target`. */
public class Service {

    public string dispatch_conformance_request() {
        Shared.shared_unique_target();
        record_conformance_event("dispatch");
        return "dispatched";
    }
}
