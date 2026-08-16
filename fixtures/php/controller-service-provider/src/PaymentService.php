<?php

namespace ChakraFixture\Service;

use ChakraFixture\Provider\Provider;

final class PaymentService
{
    public function __construct(private Provider $provider)
    {
    }

    public function refund(int $amountCents): void
    {
        $this->provider->refund($amountCents);
    }
}
