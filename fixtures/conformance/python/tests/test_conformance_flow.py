# Conformance flow tests (pytest-style functions).

from service import dispatch_conformance_request


def test_conformance_end_to_end_marker():
    dispatch_conformance_request()
