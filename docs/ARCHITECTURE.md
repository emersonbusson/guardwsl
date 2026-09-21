# Architecture

GuardWSL is a user-scoped native Linux and WSL2 utility with three responsibilities:

1. observe host-disk pressure (local Linux filesystem or WSL2 backing Windows volume);
2. plan and execute allowlisted cleanup;
3. run a small systemd user monitor.

## Components

| Component | Responsibility |
| --- | --- |
| `maintenance_lock` | Serializes cleanup activity. |
| `host` | Selects a local `statvfs` probe on native Linux or a bounded, read-only PowerShell backing-volume probe on WSL2. |
| `repository` | Discovers authenticated Git repositories under configured roots. |
| `cleanup` | Plans, revalidates, quarantines, and removes exact allowlist entries. |
| `history` | Appends private JSONL audit records. |
| `config` | Validates strict TOML and preserves a last-known-good copy. |
| `guard` | Exposes the CLI and the systemd user monitor. |

## Data flow

```text
tool shim / guard exec -> resolve tool -> execute child directly

monitor -> fresh host probe -> pressure classification
                            -> exclusive maintenance lock
                            -> cleanup plan and revalidation
                            -> audit intent -> quarantine -> removal
```

## Trust boundaries

- Native Linux pressure uses available space on the filesystem containing the configured scan roots; cross-filesystem roots fail closed. WSL2 pressure uses the physical Windows backing volume, never guest virtual `df` capacity.
- Windows data is observational. The PowerShell adapter reads registry,
  volume, and VHD sparse state with a bounded timeout; it does not
  mutate Windows or WSL state.
- Linux cleanup runs as the current user and cannot intentionally cross its
  authenticated roots, devices, mounts, protected paths, or ownership boundary.
- Runtime cleanup locks and state files must be regular, single-link, current-user files
  in directories that are not writable by other users.
- Git metadata is used as evidence that a project artifact is ignored and
  contains no tracked path. Git data itself is never a cleanup candidate.

See [SAFETY.md](SAFETY.md) for deletion invariants and known limitations.
