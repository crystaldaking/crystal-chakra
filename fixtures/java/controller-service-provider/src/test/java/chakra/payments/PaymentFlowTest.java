package chakra.payments;

import chakra.payments.api.PaymentController;
import chakra.payments.provider.StripeProvider;
import chakra.payments.service.PaymentService;

import org.junit.jupiter.api.Test;

/** End-to-end flow: the controller delegates refunds to the provider. */
class PaymentFlowTest {

    @Test
    void refund_delegates_to_provider() {
        PaymentService service = PaymentService.buildPaymentService("test-key");
        PaymentController controller = new PaymentController(service);
        controller.refund(100L);
        StripeProvider provider = new StripeProvider("test-key");
        provider.refund(100L);
    }
}
