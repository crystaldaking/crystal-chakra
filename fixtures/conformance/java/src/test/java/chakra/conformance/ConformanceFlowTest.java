package chakra.conformance;

import org.junit.jupiter.api.Test;

/** Conformance flow tests (JUnit-style `@Test` methods). */
class ConformanceFlowTest {

    @Test
    void conformance_end_to_end_marker() {
        new Service().dispatch_conformance_request();
    }
}
