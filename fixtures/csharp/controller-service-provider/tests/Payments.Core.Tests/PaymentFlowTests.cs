using Chakra.Payments.Api;
using Chakra.Payments.Service;
using Chakra.Payments.Shared;
using Microsoft.VisualStudio.TestTools.UnitTesting;
using NUnit.Framework;
using Xunit;

namespace Chakra.Payments.Tests;

public sealed class PaymentFlowTests
{
    [Fact]
    public async Task Refund_delegates_to_provider()
    {
        var service = PaymentService<StripeProvider>.BuildPaymentService("test-key");
        var controller = new PaymentController(service);
        await controller.RefundAsync(100);
    }

    [Test]
    public void NUnit_relationship()
    {
        Shared.SharedUniqueTarget("nunit");
    }

    [TestMethod]
    public void MSTest_relationship()
    {
        Shared.SharedUniqueTarget("mstest");
    }
}
