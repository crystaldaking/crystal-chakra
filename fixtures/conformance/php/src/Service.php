<?php
// Production service source. The text-search scenario looks for the needle
// comment below in exactly this file.
// CONFORMANCE_TEXT_NEEDLE: payment pipeline marker
namespace Conf;

use Conf\Util\FormatHelper as FormatHelperAlias;

/** Uniquely named caller of Conf::shared_unique_target. */
function dispatch_conformance_request(): void
{
    shared_unique_target();
    (new FormatHelperAlias())->format('dispatch');
}
