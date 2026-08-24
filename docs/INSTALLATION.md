# Installation and removal

## Requirements

- WSL2 with systemd enabled;
- Windows PowerShell interoperability from WSL;
- Rust 1.98.0, Cargo, Bash, and `flock`;
- enough physical disk and host RAM for the installation build.

Review `scripts/install-linux.sh` before running it. The installer is
user-scoped and does not require a Windows service or a root daemon.

## Install

```bash
git clone https://github.com/emersonbusson/guardwsl.git
cd guardwsl
./scripts/install-linux.sh
```

The installer:

1. backs up every managed user file;
2. runs the Rust tests with GuardWSL shims removed from `PATH`;
3. acquires the same build and maintenance locks used at runtime;
4. probes physical Windows disk and RAM headroom;
5. builds and installs `~/.local/bin/guard`;
6. installs the systemd user unit and tool shims;
7. initializes or strictly normalizes the private configuration;
8. enables the monitor and waits for `guard doctor` to become healthy;
9. rolls back all managed files if any activation step fails.

Verify the result:

```bash
guard doctor
guard status
guard clean --dry-run
```

## Files installed

```text
~/.local/bin/guard
~/.local/lib/guardwsl/shims/
~/.config/guardwsl/config.toml
~/.config/guardwsl/config.last-good.toml
~/.config/systemd/user/guardwsl.service
~/.config/environment.d/20-guardwsl.conf
~/.local/state/guardwsl/
```

The installer also adds one marked PATH block to existing `.profile`,
`.bashrc`, and `.zshrc` files. Backups live under
`~/.local/state/guardwsl/install-backups/`.

## Remove

Stop and disable the user service before removing installed files:

```bash
systemctl --user disable --now guardwsl.service
```

Then restore the desired installer backup or remove only the files listed
above and the marked `GuardWSL shims` blocks from shell startup files. Preserve
`~/.local/state/guardwsl/` until its audit and backup contents are no longer
needed. Never remove an entire home, state, or configuration root recursively.
