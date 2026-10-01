#!/usr/bin/env python3
"""Read-only audit for GuardWSL references in sibling repository workspaces."""

from __future__ import annotations

import argparse
import base64
import json
import shutil
import subprocess
import sys
from pathlib import Path

SEARCH_PATTERN = (
    r"(?i)(?:guard[\W_]*wsl)"
    r"|(?:\bguard[ \t]+(?:exec|status|monitor|clean|doctor|history|config|build-info)\b)"
    r"|(?:\bguard[-_]exec\b)"
    r"|(?:\bGUARDWSL_[A-Z0-9_]+\b|\bguard_executable\b|\.admission\.guard(?:\.|\b))"
    r"|(?:/usr/local/(?:bin|libexec)/guard(?:wsl)?(?:/guard)?\b)"
    r"|(?:(?:\$HOME|~)/\.local/bin/guard\b)"
    r"|(?:\bguard(?:wsl)?[ _-]+(?:to[ _-]+cargo|degrad\w*|wrapper|admission|identity|telemetry|binary|lock|integration|tool|service|global|external|real|active)\b)"
)

EXCLUDED_GLOBS = (
    "!**/.git/**",
    "!**/node_modules/**",
    "!**/target/**",
    "!**/dist/**",
    "!**/.next/**",
    "!**/.turbo/**",
    "!**/.cache/**",
    "!**/vendor/**",
    "!**/backups/**",
    "!**/.backups/**",
)


def decode_path(value: dict[str, str]) -> str:
    if "text" in value:
        return value["text"]
    return base64.b64decode(value["bytes"]).decode("utf-8", errors="replace")


def main() -> int:
    own_repo = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(
        description="Audit sibling repository files for GuardWSL product references."
    )
    parser.add_argument(
        "--workspace-root",
        type=Path,
        default=own_repo.parent,
        help="workspace containing this checkout (default: its parent directory)",
    )
    args = parser.parse_args()
    workspace = args.workspace_root.expanduser().resolve()

    if not workspace.is_dir() or not own_repo.is_relative_to(workspace):
        print("ERROR: workspace root must contain this checkout and be a directory.")
        return 2

    rg = shutil.which("rg")
    if not rg:
        print("ERROR: ripgrep (rg) is required for the workspace audit.")
        return 2

    own_relative = own_repo.relative_to(workspace).as_posix()
    globs = [
        f"!{own_relative}/**",
        "!vitae/**",
        "!Vitae/**",
        "!emersonbusson/README.md",
        *EXCLUDED_GLOBS,
    ]
    command = [rg, "--json", "--hidden", "--color", "never", "--no-heading"]
    for glob in globs:
        command.extend(("--glob", glob))
    command.extend(("--regexp", SEARCH_PATTERN, "."))

    result = subprocess.run(
        command,
        cwd=workspace,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )

    findings: set[tuple[str, int]] = set()
    for raw_line in result.stdout.splitlines():
        try:
            event = json.loads(raw_line)
        except json.JSONDecodeError:
            print("ERROR: ripgrep returned malformed JSON.")
            return 2
        if event.get("type") != "match":
            continue
        data = event["data"]
        path = decode_path(data["path"])
        findings.add((path, int(data["line_number"])))

    for path, line in sorted(findings):
        print(f"REFERENCE {path}:{line}")

    if result.returncode not in (0, 1):
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        print(f"ERROR: ripgrep could not complete the audit{': ' + detail if detail else '.'}")
        return 2
    if findings:
        print(f"Found {len(findings)} GuardWSL reference(s) outside the allowed locations.")
        return 1

    print("No GuardWSL references found outside the allowed locations.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
