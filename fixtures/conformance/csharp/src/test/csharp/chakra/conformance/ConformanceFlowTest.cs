namespace chakra.conformance;

using Xunit;

/** Conformance flow tests (xUnit `[Fact]` methods). */
class ConformanceFlowTest {

    [Fact]
    void conformance_end_to_end_marker() {
        new Service().dispatch_conformance_request();
    }
}
