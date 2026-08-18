<?php

namespace Illuminate\Contracts\Container;

interface Container
{
    /** @template T of object
     * @param class-string<T> $abstract
     * @return T
     */
    public function make(string $abstract): object;
}
