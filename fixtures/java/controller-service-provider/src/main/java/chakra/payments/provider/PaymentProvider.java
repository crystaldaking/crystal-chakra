package chakra.payments.provider;

/** Provider abstraction implemented by concrete payment providers. */
public interface PaymentProvider {

    String PROVIDER_LABEL = "payment";

    boolean refund(long amountCents);
}
