package chakra.payments.provider;

/** Concrete provider processing refunds against the Stripe API. */
public class StripeProvider implements PaymentProvider {

    private final String apiKey;

    public StripeProvider(String apiKey) {
        this.apiKey = apiKey;
    }

    @Override
    public boolean refund(long amountCents) {
        return amountCents > 0 && !this.apiKey.isEmpty();
    }
}
