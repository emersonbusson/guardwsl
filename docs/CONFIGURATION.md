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

Current-schema files may still contain `[admission]` and `[memory]` from the
retired build gate. GuardWSL validates their old shape when reading for safe
migration, but neither table affects commands or appears in `guard config show`.
`guard config normalize` removes these tables while preserving active disk,
cleanup, and monitor settings.

Older legacy files using `version = 1` may also contain tables such as
`[scan]`, `[categories]`, `[intervals]`, `[reserve]`, `[workloads]`,
`[host_memory]`, and `[archive]`. Normalization carries forward supported
cleanup ages, protected paths, and monitor intervals, then writes only the
current schema. Legacy scan roots use the current default; reserve, workload,
host-memory, and archive controls are not activated. GuardWSL v0.1.1 does not
create or manage a `reserve.bin` file, and configuration normalization never
deletes files. The installer runs `guard config normalize` on the active config.

## Example

Fresh configuration is generated for the current user. This example uses a
generic home path:

```toml
schema_version = 1

[disk]
pressure_free_bytes = 51539607552
critical_free_bytes = 25769803776
emergency_free_bytes = 12884901888
target_free_bytes = 68719476736
host_probe_timeout_seconds = 10

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

## Disk thresholds

All sizes are bytes. Thresholds must satisfy:

```text
emergency < critical < pressure < target
```

On native Linux GuardWSL probes the filesystem containing `scan_roots`; all roots must be on one filesystem. On WSL2 it discovers the Windows backing volume for the current distribution. The byte thresholds are upper bounds: effective emergency, critical, pressure, and target thresholds are capped at 5%, 10%, 20%, and 30% of observed total capacity respectively. This prevents small disks from remaining permanently in pressure. The thresholds classify disk pressure and bound cleanup selection; they never block
or queue a development command. No drive letter or VHDX path belongs in this
configuration.

## Cleanup roots and protection

`scan_roots` contains absolute, canonical, current-user-owned directories under
which GuardWSL discovers Git repositories. Fresh configuration scans the
current user's home directory. Narrower roots reduce discovery work.

`protected_paths` contains absolute paths that must never intersect a cleanup
candidate. Keep credential, configuration, database, upload, and application
state directories protected. Protection does not turn an unknown path into a
cleanup candidate; the exact cleanup allowlist always applies first.

Age fields are hours. When free space is below `target_free_bytes` **or** disk
pressure is anything other than Healthy, GuardWSL treats the volume as needing
space and may compress category age requirements, but never below
`critical_min_age_hours`. It still applies every ownership, Git, mount,
file-type, hard-link, process-use, and identity check.

Set `cleanup.enabled = false` to disable automatic and explicit cleanup without
affecting command forwarding.

## Monitor

The systemd user monitor probes every `interval_seconds`. A cleanup cycle is
due when free space is below `target_free_bytes` or pressure is worse than
Healthy (subject to a bounded cooldown), or when the scheduled maintenance
interval has elapsed. Scheduled maintenance runs every
`maintenance_interval_seconds`.
