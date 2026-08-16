<?php

namespace ChakraFixture\Tests;

use ChakraFixture\Service\PaymentService;

final class PaymentServiceTest
{
    public function testRefundDelegatesToProvider(): void
    {
        $service = makePaymentService();
        $service->refund(100);
    }
}

function makePaymentService(): PaymentService
{
    return createPaymentService();
}
