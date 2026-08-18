#!/usr/bin/env python3
"""Validate per-language support manifests and regenerate the support matrix.

Implements the enforcement half of docs/language-parity-contract.md (#22):

- validates docs/support/languages/*.json against matrix.schema.json semantics
  (structural checks only; stdlib, no jsonschema dependency);
- requires evidence pointers to exist for pass/equivalent capabilities;
- refuses advertised status unless every mandatory capability (and every
  triggered conditional one) is satisfied with conformance/corpus evidence;
- regenerates docs/support/matrix.json and docs/support/SUPPORT_MATRIX.md.

Usage:
    tools/check_support_matrix.py           # validate + regenerate artifacts
    tools/check_support_matrix.py --check   # validate + fail if artifacts are stale
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SUPPORT_DIR = REPO_ROOT / "docs" / "support"
LANGUAGES_DIR = SUPPORT_DIR / "languages"
MATRIX_JSON = SUPPORT_DIR / "matrix.json"
MATRIX_MD = SUPPORT_DIR / "SUPPORT_MATRIX.md"
TARGET_FILE = SUPPORT_DIR / "target_languages.json"

# Keep in sync with docs/language-parity-contract.md §3.
MANDATORY_CAPABILITIES = [
    "DISC-01", "DISC-02", "DISC-03",
    "SYNTAX-01", "SYNTAX-02", "SYNTAX-03", "SYNTAX-04", "SYNTAX-05",
    "SYNTAX-06", "SYNTAX-07", "SYNTAX-08",
    "PRECISE-01",
    "QUERY-01", "QUERY-02", "QUERY-03",
    "FRESH-01", "FRESH-02",
    "PROV-01", "AMBIG-01", "BUDGET-01", "CANCEL-01", "DEGRADE-01",
    "CONFORM-01", "CORPUS-01", "DOCS-01",
]

# Conditional capabilities triggered when a precise provider is integrated.
PROVIDER_CONDITIONAL = ["PRECISE-02", "PRECISE-03", "PRECISE-04", "PRECISE-05"]

VALID_STATUSES = {"pass", "fail", "equivalent", "missing", "not-applicable"}
VALID_TIERS = {"first-class", "in-progress", "syntax", "none"}
KNOWN_CAPABILITY_IDS = set(MANDATORY_CAPABILITIES) | set(PROVIDER_CONDITIONAL)


class ValidationError(Exception):
    pass


def fail(errors: list[str], where: str, message: str) -> None:
    errors.append(f"{where}: {message}")


def load_json(path: Path, errors: list[str]) -> dict | None:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        fail(errors, str(path), "file not found")
    except json.JSONDecodeError as exc:
        fail(errors, str(path), f"invalid JSON: {exc}")
    return None


def validate_manifest(path: Path, manifest: dict, errors: list[str]) -> None:
    where = path.name
    language = manifest.get("language")
    if not isinstance(language, str) or not language:
        fail(errors, where, "missing 'language'")
        return
    if path.stem != language:
        fail(errors, where, f"file name {path.stem!r} does not match language {language!r}")

    advertised = manifest.get("advertised")
    if not isinstance(advertised, bool):
        fail(errors, where, "'advertised' must be a boolean")
        advertised = False

    tier = manifest.get("tier")
    if tier not in VALID_TIERS:
        fail(errors, where, f"'tier' must be one of {sorted(VALID_TIERS)}")
    elif advertised and tier != "first-class":
        fail(errors, where, "advertised language must have tier 'first-class'")
    elif not advertised and tier == "first-class":
        fail(errors, where, "tier 'first-class' requires advertised: true")

    capabilities = manifest.get("capabilities")
    if not isinstance(capabilities, dict) or not capabilities:
        fail(errors, where, "'capabilities' must be a non-empty object")
        return

    for cap_id in capabilities:
        if cap_id not in KNOWN_CAPABILITY_IDS:
            fail(errors, where, f"unknown capability id {cap_id!r} (sync with the parity contract)")

    provider = manifest.get("precise_provider")
    provider_integrated = isinstance(provider, dict) and provider.get("status") == "integrated"

    required = list(MANDATORY_CAPABILITIES)
    if provider_integrated:
        required += PROVIDER_CONDITIONAL

    for cap_id, record in capabilities.items():
        if not isinstance(record, dict):
            fail(errors, where, f"{cap_id}: record must be an object")
            continue
        status = record.get("status")
        if status not in VALID_STATUSES:
            fail(errors, where, f"{cap_id}: invalid status {status!r}")
            continue
        if status in ("pass", "equivalent"):
            evidence = record.get("evidence")
            if not isinstance(evidence, list) or not evidence:
                fail(errors, where, f"{cap_id}: {status} requires non-empty evidence")
            else:
                for pointer in evidence:
                    if not (REPO_ROOT / pointer).exists():
                        fail(errors, where, f"{cap_id}: evidence path does not exist: {pointer}")

    if advertised:
        for cap_id in required:
            record = capabilities.get(cap_id)
            status = record.get("status") if isinstance(record, dict) else None
            if status not in ("pass", "equivalent"):
                fail(errors, where, f"advertised but {cap_id} is {status or 'absent'!r}")
        for field in ("conformance_results", "corpus_results", "docs"):
            value = manifest.get(field)
            if not isinstance(value, str) or not value:
                fail(errors, where, f"advertised but {field} is not set")
            elif not (REPO_ROOT / value).exists():
                fail(errors, where, f"advertised but {field} does not exist: {value}")


def render_markdown(matrix: dict) -> str:
    lines = [
        "# Language Support Matrix",
        "",
        "Generated by `tools/check_support_matrix.py` from `docs/support/languages/*.json`.",
        "Do not edit by hand. Contract: `docs/language-parity-contract.md`.",
        "",
        f"Target list reviewed: {matrix['target_list_reviewed_at']} "
        f"({matrix['target_list_source']})",
        "",
        "## Summary",
        "",
        "| Language | Tier | Advertised | Grammar | Precise provider |",
        "|----------|------|-----------|---------|------------------|",
    ]
    for entry in matrix["languages"]:
        grammar = entry.get("grammar") or {}
        provider = entry.get("precise_provider") or {}
        grammar_cell = f"{grammar.get('name', '-')} {grammar.get('version', '')}".strip()
        provider_cell = provider.get("name", "-")
        if provider.get("status"):
            provider_cell += f" ({provider['status']})"
        lines.append(
            f"| {entry['language']} | {entry['tier']} | "
            f"{'yes' if entry['advertised'] else 'no'} | {grammar_cell} | {provider_cell} |"
        )
    lines += ["", "## Capability detail", ""]
    for entry in matrix["languages"]:
        lines.append(f"### {entry['language']}")
        lines.append("")
        lines.append("| Capability | Status | Mechanism |")
        lines.append("|------------|--------|-----------|")
        for cap_id in sorted(entry["capabilities"]):
            record = entry["capabilities"][cap_id]
            mechanism = record.get("mechanism") or record.get("note") or ""
            lines.append(f"| {cap_id} | {record['status']} | {mechanism} |")
        lines.append("")
    return "\n".join(lines)


def write_artifact(path: Path, content: str, check: bool, errors: list[str]) -> None:
    if check:
        current = path.read_text(encoding="utf-8") if path.exists() else None
        if current != content:
            fail(errors, path.name, "stale artifact; run tools/check_support_matrix.py")
    else:
        path.write_text(content, encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail if generated artifacts are stale")
    args = parser.parse_args()

    errors: list[str] = []

    target = load_json(TARGET_FILE, errors) or {}
    manifests: list[dict] = []
    if not LANGUAGES_DIR.is_dir():
        fail(errors, str(LANGUAGES_DIR), "manifest directory missing")
    else:
        for path in sorted(LANGUAGES_DIR.glob("*.json")):
            manifest = load_json(path, errors)
            if manifest is not None:
                validate_manifest(path, manifest, errors)
                manifests.append(manifest)

    matrix = {
        "schema_version": 1,
        "contract": "docs/language-parity-contract.md",
        "target_list_source": target.get("source", ""),
        "target_list_reviewed_at": target.get("reviewed_at", ""),
        "target_languages": target.get("languages", []),
        "languages": manifests,
    }

    write_artifact(
        MATRIX_JSON,
        json.dumps(matrix, indent=2, sort_keys=False) + "\n",
        args.check,
        errors,
    )
    write_artifact(MATRIX_MD, render_markdown(matrix), args.check, errors)

    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    advertised = [m["language"] for m in manifests if m.get("advertised")]
    print(f"support matrix OK: {len(manifests)} language manifests, advertised: {advertised or 'none'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
