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
- Development commands are forwarded directly; GuardWSL does not gate or
  control their execution.
- GuardWSL does not compact or convert VHDX files and does not start, stop, or
  control WSL or Hyper-V resources.

## Threat model

GuardWSL is a user-scoped cleanup and observability tool. It assumes the
current user account is trusted and treats data loss as the primary risk.

**Assets in scope.** User files under configured scan roots; the private
configuration, audit log, and install record under `~/.config/guardwsl` and
`~/.local/state/guardwsl`; the integrity of the installed binary and systemd
user unit.

**In scope.** A candidate path that is not truly regenerable; symlink or
mount escape from a cleanup tree; hard links that escape the tree; a race
between planning and deletion; a hostile or corrupted configuration that
widens cleanup; an unbounded or spoofed Windows host probe; a distro name
that could influence the host probe; a compromised release archive.

**Out of scope.** An attacker who already has the user's shell, root, or
Windows administrator access; kernel or WSL2 hypervisor compromise; malicious
content inside an allowlisted cache directory that the user themselves
created; denial of service against disk space; confidentiality of files the
user can already read.

**Mitigations.** Exact allowlist only; fail-closed configuration with a
last-known-good copy; owner, mode, Git, mount, file-type, hard-link,
process-use, and identity revalidation before every removal; quarantine via
`renameat2` `RENAME_NOREPLACE` with post-rename purge verification; bounded
host probe with schema and freshness validation; distro names restricted to
`[A-Za-z0-9._-]`; release archives published with `SHA256SUMS` and tag
identity checks in CI.

See [`docs/SAFETY.md`](docs/SAFETY.md) for the complete deletion model.
