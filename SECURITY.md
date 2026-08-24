# Security policy

## Supported versions

GuardWSL has not published a stable release yet. Security fixes currently
target the latest commit on the default branch.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub's
[private vulnerability reporting form](https://github.com/emersonbusson/guardwsl/security/advisories/new)
and include:

- the affected version or commit;
- the operating-system and WSL versions;
- a minimal reproduction that does not expose private data;
- the expected and observed safety behavior;
- any evidence of data loss, path traversal, privilege crossing, or host
  mutation.

The maintainer will acknowledge a complete report when it is reviewed, keep
the reporter informed of material progress, and coordinate disclosure after a
fix is available. No response or remediation deadline is guaranteed before the
project's first stable release.

## Security boundaries

- GuardWSL runs as the current Linux user and installs no privileged daemon.
- The Windows probe is read-only and bounded by a timeout and output limit.
- Cleanup is exact-allowlist, revalidates identity, and preserves data whenever
  evidence is incomplete.
- The build gate is cooperative and can be bypassed by deliberately avoiding
  GuardWSL shims.
- GuardWSL does not compact or convert VHDX files and does not start, stop, or
  control WSL or Hyper-V resources.

See [`docs/SAFETY.md`](docs/SAFETY.md) for the complete deletion model.
