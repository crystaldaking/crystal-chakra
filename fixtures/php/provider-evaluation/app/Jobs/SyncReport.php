<?php

namespace App\Jobs;

use App\Services\ReportService;
use App\Traits\Audits;

final class SyncReport
{
    use Audits;

    public function __construct(private ReportService $service)
    {
    }

    public function handle(): void
    {
        $this->service->generate();
        $this->audit('synchronized');
    }
}
