<?php

namespace App\Services;

use App\Contracts\Reporter;

final class DatabaseReporter implements Reporter
{
    public function report(string $payload): string
    {
        return 'database:' . $payload;
    }
}
