<?php
// Second of two namespaces defining the same function name on purpose.
namespace Conf\DupB;

function colliding_helper(): string
{
    return "b";
}
