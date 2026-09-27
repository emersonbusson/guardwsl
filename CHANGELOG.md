# Changelog

All notable changes to GuardWSL will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project intends to use [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
- Group-writable cleanup roots are accepted only when owned by the current
  user and group-writable by the effective group; other-writable roots remain
  rejected.
