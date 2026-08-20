namespace Chakra.Payments.Provider;

public sealed class StripeProvider : IPaymentProvider
{
    private readonly string apiKey;

    public StripeProvider(string apiKey)
    {
        this.apiKey = apiKey;
    }

    public string ProviderLabel => "stripe";

    public async Task<string> RefundAsync(int amountCents)
    {
        await Task.Yield();
        return $"{apiKey}:{amountCents}";
    }
}
