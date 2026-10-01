# Changelog

All notable changes to GuardWSL will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project intends to use [Semantic Versioning](https://semver.org/).

## [0.2.0](https://github.com/emersonbusson/guardwsl/compare/v0.1.2...v0.2.0) (2026-10-01)


### Features

* add standalone GuardWSL v1 ([b056695](https://github.com/emersonbusson/guardwsl/commit/b0566959537d78e7a829168f0152c56ab550a46a))
* monitor host disk on Linux and WSL2 without build gating ([bb1aa14](https://github.com/emersonbusson/guardwsl/commit/bb1aa141d2e2bb2525aafd7af9750f910efdcc53))
* report release identity and install date ([ceec1cc](https://github.com/emersonbusson/guardwsl/commit/ceec1cc97d4f1be8e7ed9a1c5d11205e0c8835af))


### Bug Fixes

* clean below target and expand tool cache allowlist ([f9968f0](https://github.com/emersonbusson/guardwsl/commit/f9968f03aecd619b5ea42014e2a75510dd2f0e4c))
* **cleanup:** configure safe.directory in git commands for CI compatibility ([4ef7ecb](https://github.com/emersonbusson/guardwsl/commit/4ef7ecb259e3536f99ddf274e5723e62ede3f645))
* **cleanup:** handle CI runner daemons and defunct processes in path_is_in_use ([407c1dc](https://github.com/emersonbusson/guardwsl/commit/407c1dc6989911231267776052478606bc3a945d))
* **cleanup:** ignore self-process ephemeral permission race conditions in path_is_in_use ([e5d88d0](https://github.com/emersonbusson/guardwsl/commit/e5d88d0da5296e6ebadd50f9ac684a8a99a761a4))
* keep monitor active during release compilation ([829cc49](https://github.com/emersonbusson/guardwsl/commit/829cc49f262258d3c8435991af46d5546b603ab4))
* make release install zero-friction and verify release identity ([116cc81](https://github.com/emersonbusson/guardwsl/commit/116cc81400996437b4c430f7194d5ca8f303a1fe))
* reject group-writable cleanup parents ([0df6ebd](https://github.com/emersonbusson/guardwsl/commit/0df6ebd5f6a6a2d0824d08dcd278ce3c354f8105))
* remove build admission and preflight controls ([8c9b77d](https://github.com/emersonbusson/guardwsl/commit/8c9b77d2ecefed8286c50c3f65582e7cb1c8b16e))
* report the real cleanup allowlist and tighten distro names ([f9dca8b](https://github.com/emersonbusson/guardwsl/commit/f9dca8b4cc7d9a8d7746217d350e1e4659e5c192))


### Documentation

* add guard doctor to command reference in README ([8c10add](https://github.com/emersonbusson/guardwsl/commit/8c10addbab852b264eaba1bbe8ed6fb4ba2caef9))
* add workspace reference audit ([4eda803](https://github.com/emersonbusson/guardwsl/commit/4eda803b05fe79091099fbf49ddce12f0d50e5eb))
* clarify commands, admission toggling, and direct test execution ([722919f](https://github.com/emersonbusson/guardwsl/commit/722919f6ff16a9767d617cc7a4c1f5a3c6483b6f))
* close open-source documentation gaps ([bc43fc1](https://github.com/emersonbusson/guardwsl/commit/bc43fc13e9669e9d3fb0f6b5a9547ab6c21ffc39))
* correct status and remove retired build-gate claim ([525c98b](https://github.com/emersonbusson/guardwsl/commit/525c98b50e8c81976e5dab8f45b5ca9ec21de36b))
* describe direct command forwarding ([3e63a07](https://github.com/emersonbusson/guardwsl/commit/3e63a07bd779226427d78efe65673d9674474cf1))
* refresh status allowlist summary ([56fcf33](https://github.com/emersonbusson/guardwsl/commit/56fcf334c54462df8583f8dfa81d31f2886db74c))
* remove stale build gate claims ([9c49255](https://github.com/emersonbusson/guardwsl/commit/9c492557389eda051e33f6b1fc730d622ba94f1d))


### CI

* add scheduled audit, gitleaks, codeql, and automated release workflows ([adcfd8f](https://github.com/emersonbusson/guardwsl/commit/adcfd8f6183d1ce4ffeea2cc36d7c27b0cd4bac4))
* add workflow_dispatch recovery lever to release ([41b6e36](https://github.com/emersonbusson/guardwsl/commit/41b6e364bf964e4a1b3e554cfa8984786bfb0459))
* make releases automatic on merge like ramshared ([47c3095](https://github.com/emersonbusson/guardwsl/commit/47c30953cb98aee411b2dad74115a1d02fac40f6))
* set a committer identity for release tags ([7c33a7a](https://github.com/emersonbusson/guardwsl/commit/7c33a7aebc6f21c8ad0f8af413393c4857c26ba5))

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
