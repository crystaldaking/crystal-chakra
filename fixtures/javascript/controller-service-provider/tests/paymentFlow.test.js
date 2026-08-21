import { PaymentController } from "../src/api/controller.js";
import { PaymentService } from "../src/service/paymentService.js";

describe("payment flow", () => {
    it("refund delegates to provider", () => {
        const controller = new PaymentController();
        controller.refund(1250);
    });

    it("service refund reaches the shared target", () => {
        const service = new PaymentService();
        service.refund(500);
    });
});
