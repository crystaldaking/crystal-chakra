<?php

namespace App\Providers;

use App\Console\Commands\SendDigest;
use App\Contracts\Reporter;
use App\Events\UserCreated;
use App\Listeners\SendWelcome;
use App\Models\User;
use App\Policies\UserPolicy;
use App\Services\DatabaseReporter;
use Illuminate\Support\Facades\Event;
use Illuminate\Support\Facades\Gate;

final class AppServiceProvider
{
    public function __construct(private Reporter $reporter) {}

    public function register(): void
    {
        $this->app->bind(Reporter::class, DatabaseReporter::class);
        app(Reporter::class);
        $this->commands([SendDigest::class]);
    }

    public function boot(): void
    {
        Event::listen(UserCreated::class, SendWelcome::class);
        Gate::policy(User::class, UserPolicy::class);
    }
}
