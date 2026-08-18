<?php

namespace Illuminate\Support;

abstract class ServiceProvider
{
    /** @var \Illuminate\Contracts\Container\Container */
    protected object $app;
}
