<?php

namespace App\Services;

use App\Contracts\Reporter;

final class MemoryReporter implements Reporter
{
    public function report(string $payload): string
    {
        return 'memory:' . $payload;
    }
}
