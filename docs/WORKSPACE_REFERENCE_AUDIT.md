# Workspace reference audit

Run the workspace audit from this repository:

    python3 scripts/audit-workspace-references.py

It reads tracked and non-ignored text files under the workspace and does not
modify them. Ripgrep applies repository ignore rules. Git metadata, common
dependency/build output directories, backup trees, and binary files are
excluded. The audit reports only paths and line numbers.

The shared workspace root documents local cleanup. The audit includes sibling
repositories' own `AGENTS.md` files and honors the exclusions in the script.
A clean scan exits with 0, findings return 1 and incomplete scans return 2.

Python 3 and ripgrep are required. This check runs in the local workspace. A hosted CI job for this repository
does not contain the sibling checkouts it needs to inspect.
