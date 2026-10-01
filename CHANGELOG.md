# Changelog

All notable changes to GuardWSL will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project intends to use [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.2] - 2026-10-01

### Added

- `CleanupKind::ALL` and `allowlist_names()` so `guard status` reports the real
  cleanup allowlist instead of a hand-maintained summary list.
- `guard doctor` now prints the same version and commit identity as
  `guard status`, in both text and JSON.
- Release tarballs ship a complete install tree (`guard`, installer scripts,
  systemd unit, documentation, and licenses) so a client can install without a
  Rust toolchain or a Git checkout.
- `scripts/install-linux.sh` installs a bundled release binary when present and
  only builds from Cargo in a source checkout.
- Release workflow verifies the tag against `Cargo.toml`, runs the test suite,
  checks that the binary commit matches the source commit, and publishes
  `SHA256SUMS`. The sloppy `v0.1.0-manual` fallback tag is gone.
- Releases are automatic on merge, matching `ramshared`: `release-please`
  maintains the release PR (version bump, changelog, `Cargo.toml`,
  `Cargo.lock`), and merging that PR creates the `vX.Y.Z` tag, the GitHub
  release, and the install artifacts in the same workflow run. No GitHub App
  credentials and no manual `git tag` step.
- A release identity test keeps `.release-please-manifest.json`, the
  `# x-release-please-version` markers, and the release-please config in
  lockstep with `Cargo.toml`.
- CI runs the installer shell tests.
- Workspace reference audit falls back to a built-in Python scanner when
  `ripgrep` is unavailable.

### Changed

- `guard status` JSON `cleanup_policy.allowlist` now lists every cleanup kind
  (`javascript_cache`, `rust_cache`, `go_cache`, `tool_cache`, `project_cache`,
  `rust_target`, `next_build`, `node_modules`).
- WSL distribution names are validated against the same character class as the
  installer (`[A-Za-z0-9._-]`), not just for length and control characters.
- Documentation states that age windows compress whenever free space is below
  the target **or** pressure is worse than Healthy.

## [0.1.1] - 2026-09-27

### Added

- Host-aware disk status for native Linux filesystems and the current WSL2 distribution's physical Windows backing volume.
- Conservative cleanup with exact allowlists, dry run, revalidation, and audit
  records.
- Direct forwarding of development commands without build admission.
- A user-scoped systemd monitor and transactional installer.
- English canonical documentation and a Portuguese (Brazil) README.
- Full SHA in `guard --version`, status JSON, and the install record; the
  status text shows the version and short SHA with the install timestamp.

### Changed

- Cleanup recognizes lockfiles from monorepo project directories through the
  repository root and permits hard links only when every name is inside the
  candidate tree.
- Group- and other-writable cleanup roots are rejected to prevent concurrent
  changes by peers during cleanup.
