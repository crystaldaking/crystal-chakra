using Chakra.Payments.Provider;
using Chakra.Payments.Shared;

namespace Chakra.Payments.Service;

public partial class PaymentService<TProvider>
    where TProvider : IPaymentProvider
{
    private readonly TProvider provider;

    public PaymentService(TProvider provider)
    {
        this.provider = provider;
    }

    public async Task<string> RefundAsync(int amountCents)
    {
        Shared.RecordEvent("refund");
        Shared.SharedUniqueTarget("payment".NormalizePayment());
        return await provider.RefundAsync(amountCents);
    }

    public static PaymentService<StripeProvider> BuildPaymentService(string apiKey)
    {
        return new PaymentService<StripeProvider>(new StripeProvider(apiKey));
    }
}
