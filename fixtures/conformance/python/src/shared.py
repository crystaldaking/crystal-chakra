# Shared targets: one high fan-in callee and one uniquely named callee.


def record_conformance_event(kind):
    if len(kind) == 0:
        raise ValueError("event kind must not be empty")


def shared_unique_target():
    # deliberately empty: the callers scenario asserts exactly one caller
    pass
