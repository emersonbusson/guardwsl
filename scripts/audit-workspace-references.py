#!/usr/bin/env python3
"""Read-only audit for GuardWSL references in sibling repository workspaces."""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
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

# Directory names never walked by the Python fallback (mirrors EXCLUDED_GLOBS).
EXCLUDED_DIR_NAMES = frozenset(
    {".git", "node_modules", "target", "dist", ".next", ".turbo", ".cache", "vendor",
     "backups", ".backups"}
)

# Files larger than this are skipped by the Python fallback; ripgrep has no such
# limit but the audit only needs ordinary text files.
MAX_FALLBACK_FILE_BYTES = 5 * 1024 * 1024


def decode_path(value: dict[str, str]) -> str:
    if "text" in value:
        return value["text"]
    return base64.b64decode(value["bytes"]).decode("utf-8", errors="replace")


def is_excluded_path(relative: Path, own_relative: str) -> bool:
    parts = relative.parts
    if not parts:
        return True
    if parts[0] == own_relative.split("/", 1)[0] and relative.as_posix().startswith(
        f"{own_relative}/"
    ):
        return True
    posix = relative.as_posix()
    if posix == "emersonbusson/README.md":
        return True
    if parts[0] in {"vitae", "Vitae"}:
        return True
    return any(part in EXCLUDED_DIR_NAMES for part in parts)


def search_with_python(
    workspace: Path, own_relative: str, pattern: re.Pattern[str]
) -> set[tuple[str, int]]:
    findings: set[tuple[str, int]] = set()
    for root, dir_names, file_names in os.walk(workspace):
        root_path = Path(root)
        dir_names[:] = [
            name
            for name in dir_names
            if name not in EXCLUDED_DIR_NAMES
            and not (root_path / name).is_symlink()
        ]
        for file_name in file_names:
            file_path = root_path / file_name
            relative = file_path.relative_to(workspace)
            if is_excluded_path(relative, own_relative):
                continue
            try:
                if file_path.stat().st_size > MAX_FALLBACK_FILE_BYTES:
                    continue
                text = file_path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            for line_number, line in enumerate(text.splitlines(), start=1):
                if pattern.search(line):
                    findings.add((relative.as_posix(), line_number))
    return findings


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

    own_relative = own_repo.relative_to(workspace).as_posix()
    globs = [
        f"!{own_relative}/**",
        "!vitae/**",
        "!Vitae/**",
        "!emersonbusson/README.md",
        *EXCLUDED_GLOBS,
    ]

    findings: set[tuple[str, int]] = set()
    if rg:
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

        if result.returncode not in (0, 1):
            detail = result.stderr.decode("utf-8", errors="replace").strip()
            print(
                f"ERROR: ripgrep could not complete the audit{': ' + detail if detail else '.'}"
            )
            return 2
    else:
        print("NOTE: ripgrep (rg) not found; using the built-in Python scanner.")
        pattern = re.compile(SEARCH_PATTERN)
        findings = search_with_python(workspace, own_relative, pattern)

    for path, line in sorted(findings):
        print(f"REFERENCE {path}:{line}")

    if findings:
        print(f"Found {len(findings)} GuardWSL reference(s) outside the allowed locations.")
        return 1

    print("No GuardWSL references found outside the allowed locations.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
