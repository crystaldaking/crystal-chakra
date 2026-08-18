<?php

namespace App\Providers;

use App\Contracts\Reporter;
use App\Services\DatabaseReporter;
use Illuminate\Support\ServiceProvider;

final class AppServiceProvider extends ServiceProvider
{
    public function register(): void
    {
        $this->app->bind(Reporter::class, DatabaseReporter::class);
    }
}
