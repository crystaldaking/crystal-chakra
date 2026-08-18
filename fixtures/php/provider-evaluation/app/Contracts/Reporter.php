<?php

namespace App\Contracts;

interface Reporter
{
    public function report(string $payload): string;
}
