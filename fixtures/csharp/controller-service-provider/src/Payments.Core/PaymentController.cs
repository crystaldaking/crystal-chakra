using Chakra.Payments.Provider;
using Chakra.Payments.Service;

namespace Chakra.Payments.Api;

public sealed class PaymentController
{
    private readonly PaymentService<StripeProvider> service;

    public PaymentController(PaymentService<StripeProvider> service)
    {
        this.service = service;
    }

    public Task<string> RefundAsync(int amountCents)
    {
        return service.RefundAsync(amountCents);
    }
}
