# Architecture

GuardWSL is a user-scoped WSL2 utility with four responsibilities:

1. observe physical Windows disk and RAM headroom;
2. plan and execute allowlisted cleanup;
3. serialize recognized heavy-build entry points;
4. run a small systemd user monitor.

## Components

| Component | Responsibility |
| --- | --- |
| `admission` | Deterministically classifies command intent. |
| `build_gate` | Holds the exclusive heavy-build `flock(2)`. |
| `maintenance_lock` | Prevents cleanup from overlapping managed builds. |
| `host` | Runs a bounded, read-only PowerShell host probe. |
| `repository` | Discovers authenticated Git repositories under configured roots. |
| `cleanup` | Plans, revalidates, quarantines, and removes exact allowlist entries. |
| `history` | Appends private JSONL audit records. |
| `config` | Validates strict TOML and preserves a last-known-good copy. |
| `guard` | Exposes the CLI and the systemd user monitor. |

## Data flow

```text
tool shim -> classify command
              | test/check/install/other -> execute directly
              ` heavy build
                  -> optional exclusive build lock
                  -> shared maintenance lock
                  -> fresh host disk/RAM preflight
                  -> execute child

monitor -> fresh host probe -> pressure classification
                            -> exclusive maintenance lock
                            -> cleanup plan and revalidation
                            -> audit intent -> quarantine -> removal
```

## Trust boundaries

- Windows data is observational. The PowerShell adapter reads memory, registry,
  volume, process, and VHD sparse state with a bounded timeout; it does not
  mutate Windows or WSL state.
- Linux cleanup runs as the current user and cannot intentionally cross its
  authenticated roots, devices, mounts, protected paths, or ownership boundary.
- Runtime locks and state files must be regular, single-link, current-user files
  in directories that are not writable by other users.
- Git metadata is used as evidence that a project artifact is ignored and
  contains no tracked path. Git data itself is never a cleanup candidate.

See [SAFETY.md](SAFETY.md) for deletion invariants and known limitations.
