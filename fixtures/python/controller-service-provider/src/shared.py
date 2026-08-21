# Shared helpers imported through both plain and aliased imports.


def shared_unique_target():
    # deliberately empty: the fixture asserts callers of this function
    pass


def record_event(kind):
    if len(kind) == 0:
        raise ValueError("event kind must not be empty")
