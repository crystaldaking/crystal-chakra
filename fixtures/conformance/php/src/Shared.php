<?php
// Shared helpers with deliberately unique names for caller/callee assertions.
namespace Conf;

/** Uniquely named callee: exactly one caller (Conf::dispatch_conformance_request). */
function shared_unique_target(): void {}

/** High-degree callee: called by every Conf::fan_in_caller_* function. */
function record_conformance_event(string $label): void {}
