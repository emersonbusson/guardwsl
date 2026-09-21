# Development command forwarding

GuardWSL does not coordinate builds. Installed shims and `guard exec` resolve
the underlying tool and execute it directly, without locks, queues, host-disk
preflight, temporary build directories, or active-build markers.

Host-disk samples drive the independent cleanup monitor and are shown by
`guard status`. Cleanup continues to use its
own maintenance lock and its allowlist, identity, age, Git, ownership, mount,
hard-link, and in-use checks.
