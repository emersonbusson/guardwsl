# GuardWSL engineering rules

- English is the canonical language for source code, comments, CLI output,
  issues, pull requests, and documentation. `README.pt-BR.md` is the only
  maintained Portuguese translation.
- GuardWSL is a standalone project. Do not add links, imports, policies,
  fixtures, examples, paths, or names from unrelated products or repositories.
- Keep v1 small: host-aware status, safe cleanup, and a user monitor. Do not add a Windows service, VM/Hyper-V control, VHDX compaction, shutdown, cache dropping, Docker pruning, cgroups, sockets, brokers or a reserve file.
- Safety is the product. Never delete source code, Git data, configuration, secrets, databases, uploads, Docker data or any unclassified path.
- Cleanup is exact-allowlist only. Every action supports dry-run, writes an audit intent before mutation, revalidates identity, rejects symlink/mount traversal and skips paths used by active same-user processes.
- Report logical bytes removed separately from physical host space observed. With WSL sparse VHD enabled, Linux deletion may return host blocks gradually; Guard must never claim instant physical reclaim.
- Host physical free space is authoritative for WSL2 pressure decisions; guest `df` is diagnostic only. Discover the current distribution backing volume dynamically and never hardcode a drive letter.
- Invalid configuration fails closed and preserves the last-known-good configuration. Destructive tests use isolated temporary directories only.
- Shims and `guard exec` forward every command directly. GuardWSL never serializes or blocks development commands based on host telemetry, disk pressure, RAM, or cleanup state.
- Every user-visible behavior and recovery procedure stays documented in this repository. GuardWSL does not own application-domain policy.
