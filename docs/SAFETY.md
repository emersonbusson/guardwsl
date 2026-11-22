# Safety model

GuardWSL treats deletion safety as the primary product requirement. Unknown
space is preserved, even under disk pressure.

## Cleanup allowlist

The v1 allowlist is limited to:

- npm, Yarn (classic and Berry), pnpm (cache and store), Bun, Cargo, and Go caches;
- shared compiler and IDE tool caches (`sccache`, `vscode-cpptools`, Playwright browsers);
- Rust `target` directories;
- `.next`, `.turbo`, `.vite`, `.pytest_cache`, `.mypy_cache`, and `.ruff_cache`;
- `node_modules` with a recognized lockfile in the project directory or the
  Git repository root.

Generic `dist`, `build`, and `out` directories are never candidates. Source,
Git data, configuration, secrets, databases, uploads, media, Docker data, and
unknown paths are always preserved.

## Required evidence

A project candidate is removable only when every applicable invariant passes:

1. the scan root and repository are canonical, current-user-owned directories;
   group- and other-writable roots and candidate parents are rejected;
2. the candidate is a real directory, not a symlink or mount escape;
3. it is on the same filesystem as its authenticated parent;
4. its exact category, manifest, lockfile, and age requirements pass; a
   lockfile may sit in the project directory or anywhere up to the repository
   root (monorepo workspace);
5. Git reports it ignored and reports no tracked path inside it;
6. it contains no nested mount, special file, or hard link whose other name
   lies outside the candidate tree;
7. no same-user process references it through cwd, root, executable, maps, or
   open file descriptors;
8. identity, inode, device, and newest modification time still match the plan;
9. an audit intent is durably appended before rename;
10. quarantine stays on the same mount and is removed only if identity still
    matches.

Failure or uncertainty preserves the candidate.

A running Go command can use a private `GOCACHE` for a test run. When the
process explicitly selects a separate directory in the same filesystem view,
the unused default Go compiler cache remains eligible. Open files, mappings,
the module download cache, custom cache programs and unreadable process state
retain their normal protections.

Nested checkouts and linked worktrees own their own project outputs. Discovery
stops at each nested Git marker, and cleanup checks the boundary again before
removal. A generated directory containing Git metadata is preserved.

For project artifacts, another same-user process working in the repository or
referencing it by an absolute command-line argument also protects that
repository's artifacts. Recognized package or compiler-cache tools protect
their own shared cache family while running. This protects work that may need
files between individual reads; it does not mark unrelated caches as in use.

Some processes intentionally hide their `/proc` entries even from their owner.
When this happens, GuardWSL can use non-interactive `sudo` for short, read-only
`find` and `cat` inspections of that process's links, maps, and command line.
No candidate path is passed to a privileged deletion command, and there is no
privileged daemon. If inspection is unavailable or inconclusive, the candidate
is preserved and the monitor reports the failure rather than claiming cleanup
is healthy. Inspection output and execution time are bounded.
Within one plan, GuardWSL reuses a bounded in-memory snapshot for each
otherwise-inaccessible process only while its PID owner and start time match.
The snapshot cache is limited to 32 MiB; exact candidate contexts are
deduplicated, and a plan preserves remaining paths after 10,000 existing
candidate contexts. This snapshot is discarded after planning. Every actual
deletion performs a fresh process inspection during revalidation and another
after audit intent, just before quarantine. A process can still start between
the final inspection and rename; the checks reduce that race but cannot make it
atomic.
The user service does not set `NoNewPrivileges=true`, because that setting
prevents `sudo` from gaining the privileges needed for these read-only checks
and guest `fstrim`. GuardWSL itself remains an unprivileged user service; any
non-interactive `sudo` access is subject to the host's existing sudo policy.

## Explicit manual cache cleanup

`guard clean --all-caches --dry-run` previews a manual cleanup that includes
recent allowlisted package, compiler, tool, and project caches. Remove
`--dry-run` to execute it. This option waives only cache age; build-output and
`node_modules` retention still applies. Ownership, protected paths, Git state,
mounts, file types, hard links, process use, audit intent, quarantine and
identity revalidation remain mandatory. It never enables unknown paths.

The explicit request skips the host-target logical budget but keeps the
per-cycle action ceiling. Review another dry run before repeating if that
ceiling leaves candidates. Each audit intent records the candidate's minimum
age; cycle JSON records `cache_age_policy: "include_recent"`. Normal cleanup
records `"retention"`; older cycle history defaults to retention on read.
The flag is available only on manual `clean`, does not alter configuration,
and is never used by the automatic monitor. Cache regeneration can make the
next tool invocation slower; it does not prove physical host reclaim.

## Dry run and accounting

`guard clean --dry-run` executes discovery and safety checks without rename or
removal. GuardWSL reports logical bytes separately from the physical free-space
delta observed on Windows. It never promises that deleting a logical byte
immediately returns a physical byte to the host volume.
When cleanup runs under pressure, GuardWSL attempts a bounded guest `fstrim`;
a failure is reported in the cleanup result and monitor health instead of
being silently ignored. The attempt does not shut down WSL or compact a VHDX.

When a live Windows probe identifies a drive-letter-backed VHDX, GuardWSL can
retain its identity in a private state file for the current Linux boot. Later
checks verify the distribution, root device, DrvFs mount, and VHDX inode and
device before reading current physical volume capacity from that mount. A
changed identity, restart, inaccessible mount, or unsupported Windows volume
falls back to the Windows probe; neither an old free-space number nor a guessed
drive is used. Status names this source `verified_drvfs`. If neither probe
works, status reports unknown host capacity and unhealthy telemetry. Safe,
age-based allowlisted cleanup still runs independently on the pressure retry
cadence, without applying a host-space target or claiming physical recovery.
Manual cleanup also works in this mode and returns a nonzero exit status to
make the missing telemetry visible. Invalid configuration and uncertain
candidate safety continue to prevent deletion.

## Host reclaim and VHDX compaction

On WSL2 the physical Windows volume is authoritative. A dynamically growing
`ext4.vhdx` can report free virtual capacity inside Linux while its physical
file is nearly full. Two VHDX layouts behave differently on delete:

- **sparse VHDX** may return host blocks gradually as files are removed;
- **non-sparse VHDX** never shrinks on delete. Logical cleanup still prevents
  further growth by freeing ext4 blocks for reuse, but it cannot return space
  to the Windows volume.

GuardWSL observes this and never pretends otherwise:

- `guard status` divides allocated VHDX bytes by host volume size to report
  occupancy. The Windows probe reads allocated bytes with `GetCompressedFileSizeW`.
  If Windows restricts that query, the probe still reports host free space
  and leaves occupancy unknown. The verified DrvFS probe uses allocated blocks.
- When the backing VHDX is not sparse and host space cannot be recovered by
  cleanup alone, `guard status`, `guard doctor`, `guard clean`, and the monitor
  all surface `compaction_advised`. Cleanup continues in order to prevent
  further growth; the advisory names the real recovery step.
- A cleanup cycle that deletes logical bytes while observing a non-positive
  host free-space delta is treated as futile reclaim and raises the advisory.

GuardWSL does not compact, convert, or shut down WSL. Physical reclaim is an
offline administrative operation:

```text
1. wsl --shutdown
2. Back up the distribution (wsl --export, or copy the VHDX).
3. Compact the VHDX (Optimize-VHD -Mode Full, or diskpart compact vdisk).
4. Restart the distribution and re-run `guard status`.
```

`sparseVhd=true` in `.wslconfig` applies to newly created VHDs only. Converting
an existing VHDX is offline, requires a verified backup, and is out of scope
for GuardWSL.

## Boot integration

The monitor is a systemd user service. It must start with WSL2, not only at an
interactive login, so pressure is observed from the first moment the
distribution is up. The installer enables `guardwsl.service` and systemd
linger; `guard doctor` verifies `systemd=true` in `/etc/wsl.conf`, linger for
the current user, and that the unit is enabled.

After a successful scan under critical pressure, a fresh monitor can report
`monitor.no_safe_reclaim` when it found no eligible space to release.
`guard doctor` displays this as a disk warning. A failed process inspection,
failed cleanup, or stale heartbeat still fails the monitor check.

## Fail-closed behavior

- Invalid active configuration falls back to the last-known-good copy in
  degraded mode and blocks mutation.
- Cleanup is independently serialized and validates that candidate paths are not in use.
- A replaced lock, state file, candidate, or quarantine identity is rejected.
- Bounded scans and output limits prevent unbounded host-probe or audit reads.

## Explicit non-goals

GuardWSL does not compact or convert VHDX files, shut down WSL, drop caches,
prune Docker, control Hyper-V, manage cgroups, or provide a privileged broker.
It reports when compaction is required and how to recover; it never performs
that operation.

## Dedicated BuildKit cache

The optional helper previews BuildKit records and clears eligible cache on an
explicitly assigned builder. The monitor handles filesystem cleanup. The
helper's supported records, pressure thresholds and refusal conditions are
documented in the [BuildKit cache guide](DOCKER-BUILD-CACHE.md).
