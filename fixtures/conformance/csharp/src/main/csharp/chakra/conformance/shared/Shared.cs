namespace chakra.conformance.shared;

/** Shared targets: one high fan-in callee and one uniquely named callee. */
public sealed class Shared {

    private Shared() {
    }

    public static void record_conformance_event(string kind) {
        if (string.IsNullOrEmpty(kind)) {
            throw new ArgumentException("event kind must not be empty");
        }
    }

    public static void shared_unique_target() {
        // deliberately empty: the callers scenario asserts exactly one caller
    }
}
