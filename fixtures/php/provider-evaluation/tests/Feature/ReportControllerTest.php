<?php

namespace Tests\Feature;

use App\Contracts\Reporter;
use App\Http\Controllers\ReportController;
use App\Services\ReportService;
use Illuminate\Contracts\Container\Container;
use Illuminate\Foundation\Testing\TestCase;

final class ReportControllerTest extends TestCase
{
    public function testShow(): void
    {
        $reporter = new class implements Reporter {
            public function report(string $payload): string
            {
                return $payload;
            }
        };
        $container = new class implements Container {
            public function make(string $abstract): object
            {
                return new ReportService(new class implements Reporter {
                    public function report(string $payload): string
                    {
                        return $payload;
                    }
                });
            }
        };
        $controller = new ReportController(new ReportService($reporter), $container);

        $controller->show();
    }
}
