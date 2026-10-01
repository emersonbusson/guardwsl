# Installation and removal

## Requirements

- Native Linux or WSL2 with systemd enabled;
- on WSL2, Windows PowerShell interoperability;
- Bash;
- for a source install only: Rust 1.98.0, Cargo, and a clean Git checkout.

Review `scripts/install-linux.sh` before running it. The installer is
user-scoped and does not require a Windows service or a root daemon.

## Install from a release (no Rust required)

1. Download the release archive and `SHA256SUMS` from the GitHub release page.
2. Verify the archive before extracting it:

   ```bash
   sha256sum -c SHA256SUMS --ignore-missing
   ```

3. Extract and run the bundled installer:

   ```bash
   tar -xzf guardwsl-VERSION-x86_64-unknown-linux-gnu.tar.gz
   cd guardwsl-VERSION-x86_64-unknown-linux-gnu
   ./scripts/install-linux.sh
   ```

The release tree contains `guard`, `scripts/`, `systemd/`, documentation, and
licenses. When the installer finds a bundled `guard` binary and no `.git`
directory, it installs that binary directly and does not invoke Cargo.

## Install from source

```bash
git clone https://github.com/emersonbusson/guardwsl.git
cd guardwsl
./scripts/install-linux.sh
```

Run it from a clean Git checkout so the installed binary can identify the
exact source commit.

## What the installer does

1. backs up every managed user file;
2. runs the Rust tests with GuardWSL shims removed from `PATH`
   (source install only);
3. builds the release binary while the existing monitor remains active
   (source install only);
4. briefly stops the old monitor, then installs `~/.local/bin/guard`, the
   systemd user unit, and tool shims;
5. initializes or strictly normalizes the private configuration;
   on WSL2 it also records the current distribution name for backing-volume discovery;
6. enables the monitor and waits for `guard doctor` to become healthy;
7. records the installed version, source commit, and UTC install time;
8. rolls back all managed files if any activation step fails.

Verify the result:

```bash
guard --version
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
~/.local/state/guardwsl/install.json
```

`guard --version` shows the package version and full source commit.
`guard status` shows the version and short commit in text; JSON includes the
full commit and the install timestamp recorded in `install.json`.

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
