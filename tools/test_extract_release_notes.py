#!/usr/bin/env python3

from __future__ import annotations

import unittest

from extract_release_notes import extract_release_notes


class ExtractReleaseNotesTests(unittest.TestCase):
    def test_extracts_only_the_requested_section(self) -> None:
        changelog = """# Changelog

## [Unreleased]

Pending.

## [1.2.3] - 2026-09-02

Release body.

### Fixed

- One fix.

## [1.2.2] - 2026-08-01

Older body.
"""

        self.assertEqual(
            extract_release_notes(changelog, "1.2.3"),
            """## [1.2.3] - 2026-09-02

Release body.

### Fixed

- One fix.
""",
        )

    def test_accepts_a_heading_without_a_date(self) -> None:
        self.assertEqual(
            extract_release_notes("## [1.2.3]\n\nBody.\n", "1.2.3"),
            "## [1.2.3]\n\nBody.\n",
        )

    def test_rejects_a_missing_version(self) -> None:
        with self.assertRaisesRegex(ValueError, "no release section for 2.0.0"):
            extract_release_notes("## [1.2.3]\n", "2.0.0")


if __name__ == "__main__":
    unittest.main()
