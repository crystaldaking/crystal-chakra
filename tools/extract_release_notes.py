#!/usr/bin/env python3
"""Extract one versioned Markdown section from CHANGELOG.md."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


def extract_release_notes(changelog: str, version: str) -> str:
    """Return the complete ``## [version]`` section, including its heading."""

    heading = re.compile(rf"^## \[{re.escape(version)}\](?: - .+)?\s*$")
    lines = changelog.splitlines(keepends=True)
    start = next(
        (index for index, line in enumerate(lines) if heading.fullmatch(line.rstrip("\r\n"))),
        None,
    )
    if start is None:
        raise ValueError(f"CHANGELOG.md has no release section for {version}")

    end = next(
        (
            index
            for index in range(start + 1, len(lines))
            if lines[index].startswith("## [")
        ),
        len(lines),
    )
    return "".join(lines[start:end]).rstrip() + "\n"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("version", help="release version without the leading v")
    parser.add_argument(
        "--changelog",
        type=Path,
        default=Path("CHANGELOG.md"),
        help="path to the changelog (default: CHANGELOG.md)",
    )
    args = parser.parse_args()

    try:
        notes = extract_release_notes(
            args.changelog.read_text(encoding="utf-8"), args.version
        )
    except (OSError, ValueError) as error:
        parser.error(str(error))

    print(notes, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
