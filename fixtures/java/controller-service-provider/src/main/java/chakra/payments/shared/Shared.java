package chakra.payments.shared;

/** Shared targets: one high fan-in-style static callee and one uniquely named callee. */
public final class Shared {

    private Shared() {
    }

    public static void recordEvent(String kind) {
        if (kind.isEmpty()) {
            throw new IllegalArgumentException("event kind must not be empty");
        }
    }

    public static void sharedUniqueTarget() {
        // deliberately empty: the callers scenario asserts exactly one caller
    }
}
