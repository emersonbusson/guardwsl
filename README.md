# GuardWSL

Language: [Portuguese (Brazil)](README.pt-BR.md)

GuardWSL is a small, user-scoped safety tool for WSL2 development machines. It
observes the physical Windows volume that backs the current distribution,
removes only proven regenerable artifacts, and prevents recognized heavy builds
from starting at the same time.

The design is deliberately conservative: uncertainty preserves data.

## Status

The current source version is `0.1.0`. Its 50 Rust tests, formatter, Clippy,
shell syntax, dependency audit, and dependency-policy checks pass locally. No
stable public release has been published yet; review a dry run before enabling
real cleanup on any machine.

## What v1 does

1. `guard status` reports physical host disk, physical Windows RAM, the current
   WSL VHDX location and sparse attribute, monitor health, and build-gate state.
2. A systemd user monitor performs age-based maintenance and reacts to physical
   host-volume pressure.
3. An exact allowlist permits cleanup of known caches and build artifacts only
   after ownership, Git, age, mount, file-type, hard-link, process-use, and
   identity revalidation checks pass.
4. Cooperative tool shims serialize recognized heavy builds. Tests, lint, type
   checks, checks, end-to-end tests, and installs always run directly.
5. Every cleanup intent and outcome is written to a private JSONL audit log.

GuardWSL does **not** run a Windows service, control Hyper-V, compact or convert
VHDX files, shut down WSL, drop Linux caches, prune Docker, manage cgroups, or
install a privileged broker.

GuardWSL is standalone. It does not import configuration, instructions, or
policy from the repositories it scans. Repository discovery is used only to
prove that a candidate artifact is regenerable and safe to remove.

## Quick start

Requirements:

- WSL2 with systemd enabled;
- Windows PowerShell interoperability from WSL;
- Rust 1.98.0, Cargo, Bash, and `flock`;
- enough host disk and RAM for the installation build.

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
guard doctor
guard status
guard clean --dry-run
```

## Commands and configuration

Daily operation is automatic. Commands exist to inspect, diagnose, configure,
or explicitly toggle policies:

```text
guard doctor                           # Check host interop, backing volume, and locks
guard status                           # Inspect Windows disk/RAM pressure and gate state
guard clean --dry-run                  # Simulate cleanup without deleting files
guard clean                            # Run safe, allowlist-only cleanup on demand
guard admission status                 # Show whether heavy-build serialization is active
guard admission off                    # Disable heavy-build queuing (for uncoordinated parallel builds)
guard admission on                     # Re-enable heavy-build serialization
guard config show                      # Display active configuration and thresholds
guard config init                      # Create or reset ~/.config/guardwsl/config.toml
guard config validate                  # Validate configuration bounds and syntax
guard history                          # View recent cleanup audit log entries
guard exec -- <command> [args...]      # Run a command under Guard preflight and locks
```

### Key configuration notes

- **Heavy-Build Gate (`guard admission off / on`):** `guard admission off` disables the single-build queue. Use it if you prefer uncoordinated builds on your machine.
- **Tests & Linters Always Direct:** Commands such as `cargo test`, `npm test`, `pytest`, `cargo clippy`, `tsc`, `lint`, and `fmt` are classified as checks and **never acquire locks, wait in queues, or fail from admission controls** in any mode.
- **Custom Thresholds:** Adjust disk minimums, memory floors, scan roots, and protected paths in `~/.config/guardwsl/config.toml`. See the complete [configuration reference](docs/CONFIGURATION.md).

## Exact cleanup scope

The v1 allowlist contains:

- npm, Yarn, pnpm, Cargo, and Go caches;
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

## Heavy-build coordination

The default preflight requires 64 GiB free on the physical WSL backing volume
and 12 GiB of available physical Windows RAM: an 8 GiB host floor plus 4 GiB of
build headroom. Values are configurable in
`~/.config/guardwsl/config.toml`. See the complete
[configuration reference](docs/CONFIGURATION.md).

The build gate is cooperative. Normal tool entry points are covered by user
shims, but an absolute executable path outside those shims can bypass it. The
kernel releases locks when processes exit; GuardWSL has no distributed queue or
lease service. See [Build coordination](docs/BUILD-COORDINATION.md).

## Physical disk accounting and sparse VHDX

Windows physical free space is authoritative. Guest `df` output is diagnostic
because a dynamically growing ext4 VHDX can report free virtual capacity while
its physical Windows volume is nearly full.

`sparseVhd=true` in `.wslconfig` applies automatically to newly created VHDs;
it does not prove that an existing VHDX is sparse. GuardWSL queries the actual
file attribute and reports it. Logical deletion and observed physical host
delta are always reported separately.

GuardWSL never converts or compacts a VHDX. Existing-disk conversion is an
offline administrative operation that requires stopped WSL instances and a
verified backup.

## Development

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo audit --deny warnings
cargo deny check
bash -n scripts/install-linux.sh scripts/install-shims.sh
```

Tests that exercise deletion use isolated temporary directories. They never
mutate real WSL, Windows, Hyper-V, or project data.

See [Architecture](docs/ARCHITECTURE.md),
[Configuration](docs/CONFIGURATION.md), [Contributing](CONTRIBUTING.md), and
[Security Policy](SECURITY.md).

## License

GuardWSL is licensed under either the Apache License, Version 2.0 or the MIT
License, at your option. See `LICENSE-APACHE` and `LICENSE-MIT`.
