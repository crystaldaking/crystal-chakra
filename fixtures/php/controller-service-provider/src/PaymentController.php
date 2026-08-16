<?php

namespace ChakraFixture\Api;

use ChakraFixture\Service\PaymentService;

final class PaymentController
{
    public function __construct(private PaymentService $service)
    {
    }

    public function refund(int $amountCents): void
    {
        $this->service->refund($amountCents);
    }
}
