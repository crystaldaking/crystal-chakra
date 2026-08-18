<?php

use App\Console\Commands\SendDigest;
use App\Http\Controllers\UserController;
use App\Jobs\SyncReport;
use Illuminate\Support\Facades\Route;
use Illuminate\Support\Facades\Schedule;

Route::get('/users', [UserController::class, 'show']);
Route::get('/invoke', UserController::class);
SyncReport::dispatch();
Schedule::job(new SyncReport);
Schedule::command(SendDigest::class);
