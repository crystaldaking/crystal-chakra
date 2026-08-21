package chakra.payments.api;

import chakra.payments.service.PaymentService;

/** HTTP controller delegating refunds to the payment service. */
public class PaymentController {

    private final PaymentService service;

    public PaymentController(PaymentService service) {
        this.service = service;
    }

    public boolean refund(long amountCents) {
        return this.service.refund(amountCents);
    }
}
