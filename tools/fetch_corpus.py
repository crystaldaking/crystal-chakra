#!/usr/bin/env python3
"""Fetch the pinned public evaluation corpus into a local cache (issue #25).

Opt-in tooling: the default test suite and CI never fetch the network corpus.
Clones are shallow checkouts of the exact pinned SHA from
`docs/support/corpus/manifest.json`, cached under `target/corpus/` (already
git-ignored with the Cargo target directory). Re-running is a no-op when the
cached checkout already matches the pinned SHA.

Usage:
    tools/fetch_corpus.py                  # fetch every repository
    tools/fetch_corpus.py --language php   # fetch one language's repositories
    tools/fetch_corpus.py --list           # print manifest status, fetch nothing
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST = REPO_ROOT / "docs" / "support" / "corpus" / "manifest.json"
CACHE_ROOT = REPO_ROOT / "target" / "corpus"

# Primary-language source extensions used for cache metadata counts.
LANGUAGE_EXTENSIONS = {
    "rust": {".rs"},
    "php": {".php"},
    "typescript": {".ts", ".tsx"},
    "javascript": {".js", ".jsx", ".mjs", ".cjs"},
    "python": {".py"},
    "java": {".java"},
    "csharp": {".cs"},
    "shell": {".sh", ".bash", ".zsh"},
    "cpp": {".cc", ".cpp", ".cxx", ".c", ".h", ".hpp"},
    "hcl": {".tf", ".tfvars", ".hcl"},
    "go": {".go"},
}


def run_git(args: list[str], cwd: Path) -> None:
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True)


def current_head(path: Path) -> str | None:
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=path,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip() if result.returncode == 0 else None


def fetch_repository(name: str, url: str, sha: str) -> Path:
    target = CACHE_ROOT / name.replace("/", "__")
    if target.is_dir() and current_head(target) == sha:
        print(f"cached  {name} @ {sha[:12]}")
        return target
    target.mkdir(parents=True, exist_ok=True)
    if not (target / ".git").is_dir():
        run_git(["init", "-q"], target)
        run_git(["remote", "add", "origin", url], target)
    print(f"fetch   {name} @ {sha[:12]} (shallow)")
    run_git(["fetch", "-q", "--depth", "1", "origin", sha], target)
    run_git(["checkout", "-q", "FETCH_HEAD"], target)
    if current_head(target) != sha:
        raise RuntimeError(f"{name}: checkout did not land on pinned SHA {sha}")
    return target


def collect_metadata(path: Path, language: str, name: str, sha: str) -> dict:
    extensions = LANGUAGE_EXTENSIONS.get(language, set())
    files = 0
    lines = 0
    for candidate in path.rglob("*"):
        if not candidate.is_file() or ".git" in candidate.parts:
            continue
        if candidate.suffix.lower() in extensions:
            files += 1
            try:
                lines += candidate.read_bytes().count(b"\n")
            except OSError:
                continue
    metadata = {
        "repository": name,
        "sha": sha,
        "language": language,
        "source_files": files,
        "source_lines": lines,
    }
    (path / ".chakra-corpus.json").write_text(
        json.dumps(metadata, indent=2) + "\n", encoding="utf-8"
    )
    return metadata


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--language", help="fetch only this language's repositories")
    parser.add_argument("--list", action="store_true", help="print status without fetching")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    languages = manifest["languages"]
    if args.language:
        if args.language not in languages:
            print(f"error: unknown language {args.language!r}", file=sys.stderr)
            return 2
        languages = {args.language: languages[args.language]}

    failures = 0
    for language, entry in languages.items():
        for repo in entry["repositories"]:
            name, url, sha = repo["name"], repo["url"], repo["sha"]
            target = CACHE_ROOT / name.replace("/", "__")
            if args.list:
                cached = target.is_dir() and current_head(target) == sha
                print(f"{'cached ' if cached else 'missing'} {language:10} {name} @ {sha[:12]}")
                continue
            try:
                checkout = fetch_repository(name, url, sha)
                metadata = collect_metadata(checkout, language, name, sha)
                print(
                    f"        {metadata['source_files']} source files, "
                    f"{metadata['source_lines']} lines"
                )
            except (RuntimeError, subprocess.CalledProcessError) as exc:
                print(f"error: {name}: {exc}", file=sys.stderr)
                failures += 1
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
