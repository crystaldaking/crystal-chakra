// Shared targets: one high fan-in callee and one uniquely named callee.

export function record_conformance_event(kind: string): void {
    if (kind.length === 0) {
        throw new Error("event kind must not be empty");
    }
}

export function shared_unique_target(): void {
    // deliberately empty: the callers scenario asserts exactly one caller
}
