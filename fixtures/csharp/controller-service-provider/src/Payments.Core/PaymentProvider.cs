namespace Chakra.Payments.Provider;

public interface IPaymentProvider
{
    string ProviderLabel { get; }

    Task<string> RefundAsync(int amountCents);
}

public enum PaymentStatus
{
    Pending,
    Paid,
    Refunded,
}
