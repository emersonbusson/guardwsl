# Release process

GuardWSL releases are **automatic on merge**, the same model as the
`ramshared` repository: `release-please` maintains the release PR, and
merging that PR creates the tag, the GitHub release, and the install
artifacts without any manual `git tag` step.

The hosted `Release` workflow refuses to publish unless every identity
check passes. There is no manual fallback tag.

## How a release is produced

```text
feature PR merges to main
        |
        v
release-please opens / updates the release PR
  (version bump + CHANGELOG.md + Cargo.toml + Cargo.lock)
        |
        v
you merge the release PR
        |
        v
release-please creates tag vX.Y.Z and the GitHub release
        |
        v
the same workflow run builds, verifies, packages, and attaches
guardwsl-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz + SHA256SUMS
```

Commit titles drive the version bump. Use Conventional Commits:

| Prefix | Effect |
| --- | --- |
| `feat:` | minor bump (0.1.2 -> 0.2.0) |
| `fix:` | patch bump (0.1.2 -> 0.1.3) |
| `perf:`, `refactor:`, `docs:`, `ci:` | patch bump, listed in the changelog |
| `chore:`, `build:`, `test:`, `style:` | patch bump, hidden from the changelog |

Everything before `0.2.0` stays on the `0.1.x` line only for `fix:`;
`feat:` would go to `0.2.0`. Pre-`1.0.0` majors do not auto-bump.

## What the workflow enforces

1. the tag is `v*` and equals `v` + `Cargo.toml` `version`;
2. `cargo test --locked` and `bash -n` on the installer scripts pass;
3. the release binary is built with the pinned toolchain (`1.98.0`);
4. `guard --version` reports the release commit **and** the tagged
   package version;
5. a complete install tree (`guard`, `scripts/`, `systemd/`, docs,
   licenses) is packaged as `guardwsl-<tag>-x86_64-unknown-linux-gnu.tar.gz`;
6. `SHA256SUMS` is written and self-checked against the archive;
7. the GitHub release is created or updated with those assets.

A failed check blocks publication.

## Bootstrap and recovery paths

Two extra paths feed the same build/publish job:

- **Bootstrap.** If `main` advances and the `Cargo.toml` version has no
  matching `v*` tag yet, the workflow tags and publishes that version.
  This covers the first release and any manual version bump merged
  without a release PR. It never re-publishes an existing tag.
- **Manual tag.** Pushing a `vX.Y.Z` tag runs the same build and
  publishes a release for it. Use this only to recover from a stuck
  release; the normal path needs no tag command.

## Cutting a release by hand

You normally do not. If you must drive it yourself:

```bash
# 1. Conventional commits land on main and release-please opens the release PR.
# 2. Review that PR (CHANGELOG + version bump), then merge it.
# 3. The Release workflow publishes. Nothing else to run.
```

## Pre-merge local checks

These are the same gates CI runs, and are worth running before opening
a PR:

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
bash tests/install_platform.sh tests/install_order.sh
bash -n scripts/install-linux.sh scripts/install-shims.sh scripts/platform.sh
```

## Client verification

Clients should verify the archive before extracting it:

```bash
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf guardwsl-VERSION-x86_64-unknown-linux-gnu.tar.gz
cd guardwsl-VERSION-x86_64-unknown-linux-gnu
./scripts/install-linux.sh
guard --version
```

`guard --version` after install must show the same commit that the release
notes and the tagged source report.

## After the release

- Confirm the GitHub release has both the archive and `SHA256SUMS`.
- Install the release on a clean machine (or a clean user account) using only
  the tarball path, to prove zero-friction install.
- Confirm `guard --version` on the client matches the release tag.

## Files release-please maintains

| File | Role |
| --- | --- |
| `release-please-config.json` | release-type `simple`, tag `vX.Y.Z`, changelog sections |
| `.release-please-manifest.json` | last released version per package (`"."`) |
| `Cargo.toml` | `version` line carries `# x-release-please-version` |
| `Cargo.lock` | `guardwsl` `version` line carries `# x-release-please-version` |
| `CHANGELOG.md` | release-please prepends each released version |
