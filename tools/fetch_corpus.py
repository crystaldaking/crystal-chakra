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
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST = REPO_ROOT / "docs" / "support" / "corpus" / "manifest.json"
CACHE_ROOT = REPO_ROOT / "target" / "corpus"

# Retry budget for transient Git transport failures (issue #69). Bounded: at
# most FETCH_MAX_ATTEMPTS tries with a short fixed backoff schedule, then the
# fetch fails closed with the captured Git stderr attached.
FETCH_MAX_ATTEMPTS = 3
FETCH_BACKOFF_SECONDS = (2.0, 4.0)

# Lowercase stderr substrings that mark a transport-level failure worth one
# more attempt. Authentication, missing repositories, and unknown refs (for
# example an invalid pinned SHA) are deliberately absent: retrying those
# would only delay a permanent failure.
RETRYABLE_STDERR_PATTERNS = (
    "the remote end hung up unexpectedly",
    "early eof",
    "rpc failed",
    "connection reset",
    "connection timed out",
    "connection refused",
    "temporary failure in name resolution",
    "failed to connect",
    "could not resolve host",
    "operation timed out",
    "http/2 stream",
    "http 500",
    "http 502",
    "bad gateway",
    "http 503",
    "http 504",
    "error: 503",
    "ssl_read",
    "ssl syscall",
    "gnutls",
    "proxy error",
)


def is_retryable_fetch_error(stderr: str) -> bool:
    """Return True when captured Git stderr looks like a transient transport failure."""
    lowered = stderr.lower()
    return any(pattern in lowered for pattern in RETRYABLE_STDERR_PATTERNS)


def run_git(args: list[str], cwd: Path) -> None:
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True)


def fetch_with_retry(name: str, target: Path, sha: str, run=run_git, sleep=time.sleep) -> None:
    """Fetch one pinned SHA, retrying only transient transport failures.

    Fails closed after FETCH_MAX_ATTEMPTS attempts or immediately on a
    non-retryable error. The captured Git stderr is always surfaced so
    operators can distinguish transport failures from invalid SHAs or
    authentication problems.
    """
    attempt = 0
    while True:
        try:
            run(["fetch", "-q", "--depth", "1", "origin", sha], target)
            return
        except subprocess.CalledProcessError as exc:
            attempt += 1
            stderr = (exc.stderr or "").strip()
            if attempt >= FETCH_MAX_ATTEMPTS or not is_retryable_fetch_error(stderr):
                raise RuntimeError(
                    f"{name}: git fetch failed after {attempt} attempt(s); "
                    f"git stderr: {stderr or exc}"
                ) from exc
            delay = FETCH_BACKOFF_SECONDS[min(attempt - 1, len(FETCH_BACKOFF_SECONDS) - 1)]
            print(
                f"retry   {name}: transient fetch failure "
                f"(attempt {attempt}/{FETCH_MAX_ATTEMPTS}), retrying in {delay:.0f}s"
            )
            print(f"        git stderr: {stderr}", file=sys.stderr)
            sleep(delay)

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
    fetch_with_retry(name, target, sha)
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
