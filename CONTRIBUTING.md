# Contributing to GuardWSL

GuardWSL welcomes focused fixes, tests, documentation, and carefully scoped
features. Safety and predictable behavior take priority over cleanup coverage.

## Before opening a change

1. Read [`AGENTS.md`](AGENTS.md), [`docs/SAFETY.md`](docs/SAFETY.md), and
   [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
2. Search existing issues and pull requests.
3. Open an issue before proposing a new cleanup category, host mutation, or
   architectural subsystem.

GuardWSL is standalone. Contributions must not depend on private repositories,
product-specific policy, hard-coded user paths, drive letters, distribution
names, credentials, or unpublished infrastructure.

## Development setup

Requirements are listed in [`docs/INSTALLATION.md`](docs/INSTALLATION.md).

Run the complete local verification set before opening a pull request:

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo audit --deny warnings
cargo deny check
bash -n scripts/install-linux.sh scripts/install-shims.sh
```

Deletion tests must use isolated temporary directories. Tests must never mutate
real WSL distributions, Windows state, Hyper-V, VHDX files, Docker data, user
projects, or user configuration.

## Safety requirements

A cleanup change must remain exact-allowlist and fail closed. It needs tests for
the positive case and for relevant refusal paths, including symlinks, mounts,
hard links, tracked files, active processes, identity changes, and missing
reproducibility evidence.

Host adapters are observational and bounded. A pull request that starts, stops,
pauses, compacts, converts, or otherwise mutates host resources is outside the
v1 scope.

## Commits and pull requests

- Use an English Conventional Commit title, for example
  `fix: reject replaced cleanup candidates`.
- Keep each pull request focused and explain its safety impact.
- Update the English canonical documentation for behavior changes.
- Update `README.pt-BR.md` when the changed behavior is described there.
- Do not commit generated build output, credentials, machine-specific paths, or
  private incident data.

By contributing, you agree that your contribution is licensed under the
project's dual Apache-2.0-or-MIT terms.
