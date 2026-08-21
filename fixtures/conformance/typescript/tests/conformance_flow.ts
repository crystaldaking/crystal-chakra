// Conformance flow tests (jest/vitest/mocha-style blocks).

import { dispatch_conformance_request } from "../src/service";

describe("conformance flow", () => {
    it("conformance_end_to_end_marker", () => {
        dispatch_conformance_request();
    });
});
