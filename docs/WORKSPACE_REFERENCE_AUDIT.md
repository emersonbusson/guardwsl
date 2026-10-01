# Workspace reference audit

Run the workspace audit from this repository:

    python3 scripts/audit-workspace-references.py

It reads tracked and non-ignored text files under the workspace and does not
modify them. Ripgrep applies repository ignore rules. Git metadata, common
dependency/build output directories, backup trees, and binary files are
excluded. The audit reports only paths and line numbers.

The audit excludes this checkout, the complete Vitae checkout, and the single
public profile file emersonbusson/README.md. Findings return exit code 1;
unreadable paths or other incomplete scans return exit code 2. A clean audit
returns 0.

Python 3 and ripgrep are required. This check runs in the local workspace. A hosted CI job for this repository
does not contain the sibling checkouts it needs to inspect.
