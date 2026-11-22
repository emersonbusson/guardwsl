# GuardWSL

Language: [Portuguese (Brazil)](README.pt-BR.md)

GuardWSL is a small, user-scoped safety tool for native Linux and WSL2 development machines. It
observes the local Linux filesystem on native Linux or the physical Windows volume backing the current WSL2 distribution,
and removes only proven regenerable artifacts.

The design is deliberately conservative: uncertainty preserves data.

## Status

`guard --version` reports the version and full source commit.
`guard status` shows a compact version and short commit in text;
its JSON report includes full commit and installation identity. Release
archives ship `SHA256SUMS` and a complete install tree. No stable public
release has been published yet; review a dry run before enabling real cleanup
on any machine.

## What v1 does

1. `guard status` reports the GuardWSL version, short source commit, install
   date, host-disk pressure, and monitor health. Its JSON report includes the
   full commit. On WSL2 it also reports the current VHDX location and sparse
   attribute.
2. A systemd user monitor performs age-based maintenance and reacts to physical
   host-disk pressure.
3. An exact allowlist permits cleanup of known caches and build artifacts only
   after ownership, Git, age, mount, file-type, hard-link, process-use, and
   identity revalidation checks pass.
4. Every cleanup intent and outcome is written to a private JSONL audit log.

GuardWSL does **not** run a Windows service, control Hyper-V, compact or convert
VHDX files, shut down WSL, drop Linux caches, prune Docker, manage cgroups, or
install a privileged broker.

GuardWSL is standalone. It does not import configuration, instructions, or
policy from the repositories it scans. Repository discovery is used only to
prove that a candidate artifact is regenerable and safe to remove.

## Quick start

Requirements:

- Native Linux or WSL2 with systemd enabled;
- on WSL2, Windows PowerShell interoperability;
- Bash. A prebuilt release needs no Rust toolchain; building from source needs
  Rust 1.100.0 and Cargo.

### Install from a release (recommended for clients)

```bash
# Download the release and its checksum, then verify before extracting.
curl -fLO "https://github.com/emersonbusson/guardwsl/releases/latest/download/guardwsl-VERSION-x86_64-unknown-linux-gnu.tar.gz"
curl -fLO "https://github.com/emersonbusson/guardwsl/releases/latest/download/SHA256SUMS"
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf guardwsl-VERSION-x86_64-unknown-linux-gnu.tar.gz
cd guardwsl-VERSION-x86_64-unknown-linux-gnu
./scripts/install-linux.sh
```

Replace `VERSION` with the release you downloaded (for example `v0.1.2`). The
installer uses the bundled binary and does not require Cargo or a Git checkout.

### Install from source

Review the installer before running it:

```bash
git clone https://github.com/emersonbusson/guardwsl.git
cd guardwsl
./scripts/install-linux.sh
```

The installer is transactional: it backs up all managed user files and rolls
them back if the service does not become healthy. See
[Installation and removal](docs/INSTALLATION.md) for the exact file list.

Verify without deleting anything:

```bash
guard --version
guard doctor
guard status
guard clean --dry-run
```

## Commands and configuration

Daily operation is automatic. Commands exist to inspect, diagnose, configure,
or explicitly toggle policies:

```text
guard --version                        # Show the package version and full source commit
guard doctor                           # Check the relevant host-disk probe
guard status                           # Inspect host-disk pressure
guard clean --dry-run                  # Simulate cleanup without deleting files
guard clean                            # Run safe, allowlist-only cleanup on demand
guard clean --all-caches --dry-run     # Preview cleanup including recent caches
guard clean --all-caches               # Include recent caches in manual cleanup
guard config show                      # Display active configuration and thresholds
guard config init                      # Create or reset ~/.config/guardwsl/config.toml
guard config validate                  # Validate configuration bounds and syntax
guard history                          # View recent cleanup audit log entries
```

### Key configuration notes

- **Custom thresholds:** Adjust disk pressure, scan roots, and protected paths in `~/.config/guardwsl/config.toml`. See the complete [configuration reference](docs/CONFIGURATION.md).
- **Large toolchain caches:** Set `cleanup.critical_toolchain_min_bytes` to
  consider recognized Go and Rust caches, Rust `target` directories, and
  `sccache` sooner when physical disk pressure is critical. The threshold is
  per directory and every candidate must pass the existing safety checks.
- Upgrades remove GuardWSL-managed legacy tool shims and their marked PATH entries.
- **Host telemetry unavailable:** the monitor reports unhealthy and continues
  conservative age-based cleanup without inventing a physical free-space
  value. Manual cleanup can still run and returns a nonzero status until host
  telemetry is restored. Candidate safety checks still apply.
- **Manual full cache cleanup:** `--all-caches` removes the age requirement
  only for allowlisted package, compiler, tool, and project caches. It retains
  build-output and `node_modules` ages and every safety check, including
  protected paths and active processes. It skips the host-target logical
  budget, but keeps the action ceiling; repeat a reviewed cycle if that
  ceiling leaves candidates. It does not change configuration or automatic
  maintenance. Removed caches regenerate when tools need them again.

## Exact cleanup scope

The v1 allowlist contains:

- npm, Yarn (classic and Berry), pnpm (cache and store), Bun, Cargo, and Go caches;
- shared compiler and IDE tool caches (`sccache`, `vscode-cpptools`, Playwright browsers);
- Rust `target` directories;
- `.next`, `.turbo`, `.vite`, `.pytest_cache`, `.mypy_cache`, and `.ruff_cache`;
- `node_modules` when a recognized lockfile proves reproducibility.

Generic `dist`, `build`, and `out` directories are never removed. Source code,
`.git`, configuration, secrets, databases, uploads, media, Docker data, and
unknown paths are never candidates.

The default configuration discovers Git repositories under the current user's
home directory. Every root remains configurable, and protected paths are
checked before any candidate can be planned. Fresh configurations protect
common credential and control directories such as `.ssh`, `.gnupg`, `.config`,
`.aws`, `.azure`, `.kube`, keyrings, password stores, and Docker volumes.

Read the full [safety model](docs/SAFETY.md) before enabling real cleanup.
See [optional BuildKit cache cleanup](docs/DOCKER-BUILD-CACHE.md) for the
standalone helper for dedicated builders.

## Workspace reference audit

When this checkout is inside a multi-repository workspace, run the read-only
audit with:

    python3 scripts/audit-workspace-references.py

It checks sibling repository files and applies the documented Vitae and public
profile README exceptions. See [Workspace reference audit](docs/WORKSPACE_REFERENCE_AUDIT.md)
for scope and exit codes. The hosted CI for this standalone repository has no
sibling checkouts to inspect.

## Host-disk accounting and sparse VHDX

On native Linux, free space on the local filesystem containing the configured scan roots is authoritative; roots on different filesystems are rejected. On WSL2, Windows physical free space is authoritative. Guest `df` output is diagnostic
because a dynamically growing ext4 VHDX can report free virtual capacity while
its physical Windows volume is nearly full.

Disk thresholds are capped relative to the observed filesystem size, so a small disk does not remain permanently in pressure. Configured byte thresholds remain upper bounds. Normal cleanup stays bounded by the effective target; explicit `--all-caches` cleanup and emergency pressure skip that logical budget. All modes retain the action ceiling.

`sparseVhd=true` in `.wslconfig` applies automatically to newly created VHDs;
it does not prove that an existing VHDX is sparse. GuardWSL queries the actual
file attribute and reports it. Logical deletion and observed physical host
delta are always reported separately.

On a non-sparse VHDX, deleting Linux files prevents further VHDX growth but
cannot shrink the physical file. GuardWSL reports VHDX occupancy (physical
size over host volume size) and raises `compaction_advised` when host space
cannot be recovered by cleanup alone. It never converts or compacts a VHDX;
existing-disk conversion is an offline administrative operation that requires
stopped WSL instances and a verified backup. See the
[safety model](docs/SAFETY.md) for the recovery steps.

The monitor is a systemd user service enabled with linger, so it starts with
WSL2 rather than only at an interactive login. `guard doctor` verifies
`systemd=true`, linger, and the enabled unit.

## Troubleshooting

- **`guard status` says `Host: unavailable`.** On WSL2, check that Windows
  PowerShell interoperability works (`powershell.exe -NoProfile -Command 'echo ok'`).
  On native Linux, check that every `scan_roots` entry exists and lives on one
  filesystem. `guard doctor` names the failing check. Safe retention cleanup
  continues without an observed host target; it cannot guarantee physical free
  space until the backing volume is identified again.
- **The disk still looks full after cleanup.** GuardWSL reports logical bytes
  removed separately from physical host free space. A sparse VHDX does not
  shrink on every delete, and WSL `fstrim` often returns almost nothing. Trust
  `guard status` host free space, not guest `df`. When `guard status` prints
  `compaction_advised`, the backing VHDX is not sparse and host space can only
  come back through offline compaction (see [safety model](docs/SAFETY.md)).
  Occupancy at or above 75% of the host volume is the growth risk that cleanup
  cannot reverse.
- **Cleanup removed less than expected.** Age windows, the per-cycle action
  budget, and safety skips all bound each run. `guard history` shows what was
  attempted and why a candidate was skipped. Dry runs never delete.
- **`sha256sum -c SHA256SUMS` fails.** Re-download both files; do not install
  a release archive that does not verify.
- **The installer wants Cargo.** You are in a source checkout. Either install
  Rust 1.100.0 or use a release tarball, which ships a prebuilt binary.

## Development

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
bash tests/install_platform.sh
bash tests/install_order.sh
bash tests/retire_shims.sh
cargo audit --deny warnings
cargo deny --locked check
bash -n scripts/install-linux.sh scripts/retire-shims.sh
```

Tests that exercise deletion use isolated temporary directories. They never
mutate real Linux, WSL, Windows, Hyper-V, or project data.

See [Architecture](docs/ARCHITECTURE.md),
[Configuration](docs/CONFIGURATION.md),
[Release process](docs/RELEASE.md), [Contributing](CONTRIBUTING.md), and
[Security Policy](SECURITY.md).

## License

GuardWSL is licensed under either the Apache License, Version 2.0 or the MIT
License, at your option. See `LICENSE-APACHE` and `LICENSE-MIT`.
