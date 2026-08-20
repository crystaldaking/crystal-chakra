// Shared helpers imported through both plain and aliased imports.

export function sharedUniqueTarget() {
    // deliberately empty: the fixture asserts callers of this function
}

export function recordEvent(kind) {
    if (kind.length === 0) {
        throw new Error("event kind must not be empty");
    }
}
