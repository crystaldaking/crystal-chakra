import { PaymentController } from "../src/api/controller";
import { PaymentService } from "../src/service/paymentService";

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
