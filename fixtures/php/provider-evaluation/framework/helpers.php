<?php

use Illuminate\Contracts\Container\Container;

/** @template T of object
 * @param class-string<T>|null $abstract
 * @return ($abstract is null ? Container : T)
 */
function app(?string $abstract = null): object
{
    throw new RuntimeException('Evaluation fixture only');
}

/** @template T of object
 * @param class-string<T> $abstract
 * @return T
 */
function resolve(string $abstract): object
{
    throw new RuntimeException('Evaluation fixture only');
}
