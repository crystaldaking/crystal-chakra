import { PaymentService, buildPaymentService } from "../service/paymentService";

export class PaymentController {
    private service: PaymentService;

    constructor() {
        this.service = buildPaymentService();
    }

    refund(amountCents: number): void {
        this.service.refund(amountCents);
    }
}
