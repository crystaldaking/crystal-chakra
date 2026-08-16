<?php

namespace ChakraFixture\Provider;

interface Provider
{
    public function refund(int $amountCents): void;
}
