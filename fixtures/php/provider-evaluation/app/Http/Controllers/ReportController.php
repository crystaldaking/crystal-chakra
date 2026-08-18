<?php

namespace App\Http\Controllers;

use App\Contracts\Reporter;
use App\Services\ReportService;
use Illuminate\Contracts\Container\Container;

final class ReportController
{
    /** @var ReportService */
    private object $documentedService;

    public function __construct(
        private ReportService $service,
        private Container $container,
    ) {
        $this->documentedService = $service;
    }

    public function show(): string
    {
        return $this->service->generate();
    }

    public function viaFactory(): string
    {
        $service = $this->serviceFactory();
        return $service->generate();
    }

    public function viaContainer(): string
    {
        $service = $this->container->make(ReportService::class);
        return $service->generate();
    }

    public function viaDocblock(): string
    {
        return $this->documentedService->generate();
    }

    public function viaInterface(): string
    {
        return app(Reporter::class)->report('controller');
    }

    public function dynamic(string $method): mixed
    {
        return $this->service->{$method}();
    }

    private function serviceFactory(): ReportService
    {
        return $this->service;
    }
}
