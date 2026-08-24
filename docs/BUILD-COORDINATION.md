# Build coordination

GuardWSL v1 uses a single `flock(2)` file in the current user's private
runtime directory. It has no socket, broker, lease, cgroup, distributed queue,
or privileged service.

## Rules

- A recognized heavy build acquires the exclusive build lock or waits for the
  previous build to finish.
- Tests, lint, type checks, checks, end-to-end tests, and installs run directly.
  They never acquire, wait for, or fail because of the build lock.
- Cleanup uses a separate maintenance lock. Managed heavy builds hold a shared
  maintenance lock, so cleanup cannot remove artifacts while a build uses them.
- `guard admission off` disables heavy-build serialization. It does not disable
  the fresh host disk and RAM preflight.

`flock(2)` provides mutual exclusion but does not promise FIFO ordering among
waiters. GuardWSL does not claim fairness that the kernel does not provide.

## Covered entry points

User shims cover Cargo, Go, npm/npx, pnpm, Yarn/Corepack, Bun,
Next/Vite/TypeScript, Docker/Compose, Make/CMake/Ninja, Gradle/Maven, and .NET.
Classification occurs before NVM or Corepack resolution. The internal `rustc`
compiler deliberately has no shim because Cargo also invokes it for tests and
checks. An explicit direct compiler invocation can use
`guard exec -- rustc ...`.

The mechanism is cooperative. A process can bypass it by invoking an absolute
tool path outside the shims. GuardWSL does not pause an already inflated
process and never counts retained memory as reclaimed memory.

## Host preflight

Before a managed heavy build starts, GuardWSL takes a fresh sample of the
physical Windows volume and host RAM. The default policy requires:

- at least 64 GiB free on the WSL backing volume;
- an 8 GiB free-RAM floor for Windows;
- 4 GiB of additional build headroom.

The default therefore requires 12 GiB of physical host RAM to be available at
admission time. These values are configurable. GuardWSL never starts, stops,
pauses, or resizes WSL or another virtual machine.

## Process exit

The child process inherits the terminal and signals normally. File descriptors
close on success, failure, or signal, and the kernel releases the lock. The
per-build scratch directory is removed best-effort. Larger artifacts enter the
normal cleanup path only after allowlist, age, Git, ownership, mount, and
in-use checks pass.
