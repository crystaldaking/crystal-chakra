<?php

namespace App\Support;

function runUnknownHandler(object $handler): void
{
    $handler->handle();
}
