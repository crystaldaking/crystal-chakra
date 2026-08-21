package chakra.payments.service;

import chakra.payments.provider.PaymentProvider;
import chakra.payments.provider.StripeProvider;
import chakra.payments.shared.Shared;

import static chakra.payments.shared.Shared.recordEvent;

/** Service coordinating refunds through the configured provider. */
public class PaymentService {

    private final PaymentProvider provider;

    public PaymentService(PaymentProvider provider) {
        this.provider = provider;
    }

    public boolean refund(long amountCents) {
        Shared.sharedUniqueTarget();
        return this.provider.refund(amountCents);
    }

    public static PaymentService buildPaymentService(String apiKey) {
        recordEvent("build");
        return new PaymentService(new StripeProvider(apiKey));
    }
}
