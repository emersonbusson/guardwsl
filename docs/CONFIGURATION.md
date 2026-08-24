# Configuration reference

GuardWSL stores its user configuration at:

```text
~/.config/guardwsl/config.toml
```

`guard config init` creates the file with mode `0600`. Every successful save
also refreshes `config.last-good.toml`. If the active file later becomes
invalid, read-only commands use the last-known-good copy in degraded mode and
all mutation fails closed.

## Commands

```bash
guard config init
guard config show
guard config validate
```

Edit the TOML file with a normal text editor, then run `guard config validate`.
Unknown fields and invalid values are rejected.

## Example

Fresh configuration is generated for the current user. This example uses a
generic home path:

```toml
schema_version = 1

[admission]
enabled = true
build_wait_seconds = 14400

[disk]
pressure_free_bytes = 51539607552
critical_free_bytes = 25769803776
emergency_free_bytes = 12884901888
target_free_bytes = 68719476736
host_probe_timeout_seconds = 10

[memory]
host_floor_bytes = 8589934592
build_headroom_bytes = 4294967296

[cleanup]
enabled = true
scan_roots = ["/home/example"]
protected_paths = [
  "/etc",
  "/var/lib/docker/volumes",
  "/home/example/.ssh",
  "/home/example/.gnupg",
  "/home/example/.config",
  "/home/example/.local/share/keyrings",
  "/home/example/.password-store",
  "/home/example/.aws",
  "/home/example/.azure",
  "/home/example/.kube",
]
cache_min_age_hours = 168
build_min_age_hours = 168
node_modules_min_age_hours = 720
critical_min_age_hours = 24
max_actions_per_cycle = 20

[monitor]
interval_seconds = 30
maintenance_interval_seconds = 21600
```

## Admission

`admission.enabled` controls only cooperative heavy-build serialization. Tests,
checks, lint, type checks, end-to-end tests, and installs remain direct. Disk
and RAM preflight remains mandatory for managed heavy builds even when
admission is disabled.

## Disk and memory thresholds

All sizes are bytes. Thresholds must satisfy:

```text
emergency < critical < pressure < target
```

The default heavy-build preflight requires both `target_free_bytes` on the
physical Windows volume and this much available Windows RAM:

```text
host_floor_bytes + build_headroom_bytes
```

GuardWSL discovers the backing volume for the current distribution. No drive
letter or VHDX path belongs in this configuration.

## Cleanup roots and protection

`scan_roots` contains absolute, canonical, current-user-owned directories under
which GuardWSL discovers Git repositories. Fresh configuration scans the
current user's home directory. Narrower roots reduce discovery work.

`protected_paths` contains absolute paths that must never intersect a cleanup
candidate. Keep credential, configuration, database, upload, and application
state directories protected. Protection does not turn an unknown path into a
cleanup candidate; the exact cleanup allowlist always applies first.

Age fields are hours. Under critical pressure, GuardWSL may reduce category age
requirements, but never below `critical_min_age_hours`. It still applies every
ownership, Git, mount, file-type, hard-link, process-use, and identity check.

Set `cleanup.enabled = false` to disable automatic and explicit cleanup without
changing heavy-build admission.

## Monitor

The systemd user monitor probes every `interval_seconds`. Scheduled maintenance
runs every `maintenance_interval_seconds`; pressure may trigger an earlier
cycle subject to a bounded cooldown.
