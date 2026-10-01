# Release process

GuardWSL releases are built from annotated version tags. The hosted
`Release` workflow refuses to publish unless every identity check passes.

## Preconditions

- `CHANGELOG.md` has a dated section for the version being released, and
  `## [Unreleased]` is empty.
- `Cargo.toml` `version` matches the intended tag without the leading `v`.
- `cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings`,
  and `cargo test --locked` pass locally.
- `bash tests/install_platform.sh tests/install_order.sh` passes.

## Cut a release

```bash
# 1. Confirm the tree is clean and the changelog is ready.
git status
git diff origin/main

# 2. Tag the release. The tag must be v<version> and match Cargo.toml.
git tag -a v0.1.1 -m "GuardWSL v0.1.1"
git push origin v0.1.1
```

The `Release` workflow then:

1. verifies the tag is `v*` and equals `v` + `Cargo.toml` `version`;
2. runs `cargo test --locked` and `bash -n` on the installer scripts;
3. builds the release binary with the pinned toolchain;
4. verifies `guard --version` reports the checked-out commit and the tagged
   package version;
5. packages a complete install tree (`guard`, `scripts/`, `systemd/`, docs,
   licenses) into `guardwsl-<tag>-x86_64-unknown-linux-gnu.tar.gz`;
6. writes `SHA256SUMS` and self-checks it against the archive;
7. creates the GitHub release with `--verify-tag`.

There is no manual fallback tag. A failed identity check blocks publication.

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
- Move the changelog entries into a dated `## [<version>]` section if the
  workflow did not already do so.
