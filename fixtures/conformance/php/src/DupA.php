<?php
// First of two namespaces defining the same function name on purpose.
namespace Conf\DupA;

function colliding_helper(): string
{
    return "a";
}
