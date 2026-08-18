<?php

namespace App\Services;

use App\Contracts\Reporter;

final class ReportService
{
    public function __construct(private Reporter $reporter)
    {
    }

    public function generate(): string
    {
        return $this->reporter->report('report');
    }
}
