# Safety model

GuardWSL treats deletion safety as the primary product requirement. Unknown
space is preserved, even under disk pressure.

## Cleanup allowlist

The v1 allowlist is limited to:

- npm, Yarn, pnpm, Cargo, and Go caches;
- Rust `target` directories;
- `.next`, `.turbo`, `.vite`, `.pytest_cache`, `.mypy_cache`, and `.ruff_cache`;
- `node_modules` with a recognized lockfile.

Generic `dist`, `build`, and `out` directories are never candidates. Source,
Git data, configuration, secrets, databases, uploads, media, Docker data, and
unknown paths are always preserved.

## Required evidence

A project candidate is removable only when every applicable invariant passes:

1. the scan root and repository are canonical, current-user-owned directories;
2. the candidate is a real directory, not a symlink or mount escape;
3. it is on the same filesystem as its authenticated parent;
4. its exact category, manifest, lockfile, and age requirements pass;
5. Git reports it ignored and reports no tracked path inside it;
6. it contains no nested mount, special file, or hard link;
7. no same-user process references it through cwd, root, executable, maps, or
   open file descriptors;
8. identity, inode, device, and newest modification time still match the plan;
9. an audit intent is durably appended before rename;
10. quarantine stays on the same mount and is removed only if identity still
    matches.

Failure or uncertainty preserves the candidate.

## Dry run and accounting

`guard clean --dry-run` executes discovery and safety checks without rename or
removal. GuardWSL reports logical bytes separately from the physical free-space
delta observed on Windows. It never promises that deleting a logical byte
immediately returns a physical byte to the host volume.

## Fail-closed behavior

- Invalid active configuration falls back to the last-known-good copy in
  degraded mode and blocks mutation.
- Missing or stale host telemetry blocks managed heavy builds.
- Cleanup cannot overlap a managed heavy build.
- A replaced lock, state file, candidate, or quarantine identity is rejected.
- Bounded scans and output limits prevent unbounded host-probe or audit reads.

## Explicit non-goals

GuardWSL does not compact or convert VHDX files, shut down WSL, drop caches,
prune Docker, control Hyper-V, manage cgroups, or provide a privileged broker.
It cannot protect commands that deliberately bypass its cooperative shims.
