// Shared helpers imported through both plain and aliased imports.

export function sharedUniqueTarget(): void {
    // deliberately empty: the fixture asserts callers of this function
}

export function recordEvent(kind: string): void {
    if (kind.length === 0) {
        throw new Error("event kind must not be empty");
    }
}
