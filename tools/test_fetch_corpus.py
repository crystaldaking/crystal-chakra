#!/usr/bin/env python3
"""Hermetic tests for the corpus fetcher retry policy (issue #69).

These tests never touch the network or Git: the runner and sleeper are
injected fakes. They pin the retry classification, the attempt bound, the
backoff schedule, and the fail-closed error surface.
"""

from __future__ import annotations

import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import fetch_corpus  # noqa: E402


def make_failure(stderr: str) -> subprocess.CalledProcessError:
    return subprocess.CalledProcessError(128, ["git", "fetch"], stderr=stderr)


class RetryClassificationTests(unittest.TestCase):
    def test_transient_transport_failures_are_retryable(self):
        for stderr in (
            "fatal: the remote end hung up unexpectedly",
            "error: RPC failed; HTTP 503 curl 22",
            "fatal: early EOF",
            "fatal: unable to connect: Connection refused",
            "ssh: connect to host: Connection timed out",
            "fatal: unable to access: Failed to connect to github.com",
            "fatal: unable to access: Could not resolve host: github.com",
            "error: 502 Bad Gateway",
            "OpenSSL SSL_read: Connection reset by peer",
        ):
            with self.subTest(stderr=stderr):
                self.assertTrue(fetch_corpus.is_retryable_fetch_error(stderr))

    def test_permanent_failures_are_not_retryable(self):
        for stderr in (
            "remote: Repository not found.",
            "fatal: Authentication failed for 'https://github.com/x/y.git'",
            "fatal: couldn't find remote ref 0123456789abcdef",
            "error: pathspec 'deadbeef' did not match any file(s) known to git",
            "fatal: dumb http transport does not support shallow capabilities",
            "",
        ):
            with self.subTest(stderr=stderr):
                self.assertFalse(fetch_corpus.is_retryable_fetch_error(stderr))


class FetchWithRetryTests(unittest.TestCase):
    def test_succeeds_on_first_attempt_without_sleep(self):
        calls = []
        sleeps = []
        fetch_corpus.fetch_with_retry(
            "org/repo",
            Path("/unused"),
            "a" * 40,
            run=lambda args, cwd: calls.append(args),
            sleep=sleeps.append,
        )
        self.assertEqual(len(calls), 1)
        self.assertEqual(sleeps, [])

    def test_retries_transient_failure_then_succeeds(self):
        calls = []
        sleeps = []

        def run(args, cwd):
            calls.append(args)
            if len(calls) < 3:
                raise make_failure("error: RPC failed; HTTP 503 curl 22")

        fetch_corpus.fetch_with_retry(
            "org/repo", Path("/unused"), "a" * 40, run=run, sleep=sleeps.append
        )
        self.assertEqual(len(calls), 3)
        self.assertEqual(sleeps, list(fetch_corpus.FETCH_BACKOFF_SECONDS))

    def test_non_retryable_failure_raises_immediately(self):
        calls = []

        def run(args, cwd):
            calls.append(args)
            raise make_failure("remote: Repository not found.")

        with self.assertRaises(RuntimeError) as ctx:
            fetch_corpus.fetch_with_retry(
                "org/repo", Path("/unused"), "a" * 40, run=run, sleep=lambda d: None
            )
        self.assertEqual(len(calls), 1)
        self.assertIn("Repository not found", str(ctx.exception))
        self.assertIn("1 attempt", str(ctx.exception))

    def test_retryable_failure_fails_closed_after_attempt_budget(self):
        calls = []
        sleeps = []

        def run(args, cwd):
            calls.append(args)
            raise make_failure("fatal: the remote end hung up unexpectedly")

        with self.assertRaises(RuntimeError) as ctx:
            fetch_corpus.fetch_with_retry(
                "org/repo", Path("/unused"), "a" * 40, run=run, sleep=sleeps.append
            )
        self.assertEqual(len(calls), fetch_corpus.FETCH_MAX_ATTEMPTS)
        self.assertEqual(
            len(sleeps), fetch_corpus.FETCH_MAX_ATTEMPTS - 1
        )
        message = str(ctx.exception)
        self.assertIn(f"{fetch_corpus.FETCH_MAX_ATTEMPTS} attempt", message)
        self.assertIn("remote end hung up", message)

    def test_backoff_schedule_is_bounded(self):
        self.assertLessEqual(max(fetch_corpus.FETCH_BACKOFF_SECONDS), 30.0)
        self.assertLessEqual(fetch_corpus.FETCH_MAX_ATTEMPTS, 5)
        self.assertGreaterEqual(
            len(fetch_corpus.FETCH_BACKOFF_SECONDS),
            fetch_corpus.FETCH_MAX_ATTEMPTS - 1,
        )


if __name__ == "__main__":
    unittest.main()
