# Optional BuildKit cache cleanup

The standalone helper uses Docker's BuildKit API through Buildx. The GuardWSL
monitor continues filesystem cleanup independently. This helper uses Python
3.10+, an installed `guard`, and Docker with the Buildx history command.

Preview the running local builder with:

```sh
python3 scripts/docker-build-cache.py --builder default
```

Preview reports inventory candidates without deleting data. A candidate is an
immutable, unshared regular build record that BuildKit marks reclaimable and
reports as unused for at least 48 hours. The output omits build descriptions.
The shared `default` builder is supported only for inventory.

To opt in, assign an existing local single-node `docker-container` builder
exclusively to one project. Keep any project requiring warm caches or persistent
credentials on a different builder. Explicit assignment is an operator
prerequisite since BuildKit records do not reliably identify their source
repository. From outside the project and its worktrees, run:

```sh
python3 scripts/docker-build-cache.py --builder project-builder \
  --dedicated-project /absolute/path/to/project --apply
```

An operator can schedule that exact command with a user-level timer once the
dedicated builder has been validated in preview. Include `guard`, `git`,
`docker`, and `python3` in the timer's PATH. The monitor and installer leave
this helper opt-in.

Apply requires critical or emergency pressure from GuardWSL and the standard
local Docker storage layout on the distribution filesystem. Each record is
checked against current pressure and builder identity before deletion. Build
history and same-user process working directories in every registered Git
worktree are checked again before each request. A failed check skips further
requests, and BuildKit's server-side filters keep in-use, recent, shared, and
mutable records. A newly started build can still encounter a cold cache if it
has yet to acquire a record already selected for cleanup.

Each run attempts at most 10 exact records and budgets up to 5 GiB based on
rounded BuildKit estimates. Audit intents and outcomes go to
`${XDG_STATE_HOME:-$HOME/.local/state}/guardwsl/docker-build-cache.jsonl`.
The output reports BuildKit's own estimate and host free-space observations
separately. WSL can return physical space later. Images, containers, volumes,
and project files are outside this helper's deletion scope.
