import { PaymentService, buildPaymentService } from "../service/paymentService.js";

export class PaymentController {
    service;

    constructor() {
        this.service = buildPaymentService();
    }

    refund(amountCents) {
        this.service.refund(amountCents);
    }
}
