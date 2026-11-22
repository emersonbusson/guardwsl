#!/usr/bin/env python3
"""Preview or reclaim old BuildKit records on an explicitly dedicated builder."""

import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
from decimal import Decimal
import fcntl
import json
import os
from pathlib import Path
import re
import stat
import subprocess

MIN_AGE = "48h"
MAX_ESTIMATED_BYTES = 5 * 1024**3
MAX_RECORDS = 10
FILTERS = ("until=" + MIN_AGE, "inuse=false", "shared=false", "mutable=false",
           "type=regular")


class Skip(Exception):
    pass


def command(*args, timeout=15):
    try:
        return subprocess.run(
            args, stdin=subprocess.DEVNULL, capture_output=True, text=True,
            check=True, timeout=timeout,
        ).stdout.strip()
    except (OSError, subprocess.SubprocessError) as error:
        raise Skip(f"{' '.join(args[:3])} failed ({type(error).__name__})") from error


def free_space(run):
    status = json.loads(run("guard", "status", "--json"))
    host = status.get("host") or {}
    free = host.get("volume_free_bytes")
    if (status.get("service") != "guardwsl" or status.get("host_error")
            or type(free) is not int or free < 0):
        raise Skip("verified host space is unavailable")
    return free, status.get("pressure")


def fields(output, name):
    return re.findall(rf"^{name}:[ \t]*(.+)$", output, re.MULTILINE)


def builder_node(run, builder):
    output = run("docker", "buildx", "inspect", builder, "--timeout", "5s")
    names = fields(output, "Name")
    if (builder == "default" or fields(output, "Driver") != ["docker-container"]
            or len(names) != 2 or names[0] != builder
            or fields(output, "Status") != ["running"]):
        raise Skip("builder must be dedicated, running, and have one node")
    endpoints = fields(output, "Endpoint")
    if len(endpoints) != 1 or not re.fullmatch(r"[a-zA-Z0-9_.-]+", names[1]):
        raise Skip("builder node or endpoint could not be identified")
    endpoint = endpoints[0]
    if not endpoint.startswith("unix://"):
        endpoint = json.loads(run("docker", "context", "inspect", "--format",
                                  "{{json .Endpoints.docker.Host}}", "--", endpoint))
    if endpoint != "unix:///var/run/docker.sock":
        raise Skip("builder endpoint is outside the local Docker daemon")
    root = Path(run("docker", "--host", endpoint, "info", "--format", "{{.DockerRootDir}}"))
    if not root.is_absolute() or root.resolve().stat().st_dev != Path("/").stat().st_dev:
        raise Skip("Docker storage is outside the distribution filesystem")
    container = "buildx_buildkit_" + names[1]
    result = run("docker", "--host", endpoint, "inspect", "--type", "container",
                 "--format", "{{.Id}} {{json .Mounts}}", container)
    container_id, mounts_json = result.split(" ", 1)
    if not re.fullmatch(r"[a-f0-9]{64}", container_id):
        raise Skip("builder container identity is unavailable")
    mounts = json.loads(mounts_json)
    storage = [mount for mount in mounts if mount["Destination"] == "/var/lib/buildkit"]
    if len(storage) != 1 or storage[0]["Type"] != "volume":
        raise Skip("builder does not use a dedicated local cache volume")
    mount = storage[0]
    volumes = root.resolve() / "volumes"
    if (mount.get("Driver") != "local" or mount.get("Name") != container + "_state"
            or Path(mount["Source"]) != volumes / mount["Name"] / "_data"
            or volumes.resolve() != volumes):
        raise Skip("builder cache volume is outside the supported layout")
    volume = json.loads(run("docker", "--host", endpoint, "volume", "inspect",
                            "--format", "{{json .}}", mount["Name"]))
    if (volume["Driver"] != "local" or volume.get("Options")
            or volume["Mountpoint"] != mount["Source"]):
        raise Skip("builder volume uses a custom storage backend")
    return names[1], container_id


def project_idle(projects):
    for process in Path("/proc").iterdir():
        if not process.name.isdecimal() or int(process.name) == os.getpid():
            continue
        try:
            if process.stat().st_uid != os.getuid():
                continue
            cwd = Path(os.readlink(process / "cwd").removesuffix(" (deleted)"))
        except (FileNotFoundError, ProcessLookupError):
            continue
        if any(cwd.is_relative_to(project) for project in projects):
            raise Skip("a process has its working directory in the project")


def verify_idle(run, builder, project):
    worktrees = run("git", "-C", str(project), "worktree", "list", "--porcelain", "-z")
    roots = {Path(field.removeprefix("worktree ")).resolve()
             for field in worktrees.split("\0") if field.startswith("worktree ")}
    if project not in roots:
        raise Skip("Git worktrees could not be identified")
    project_idle(roots)
    if run("docker", "buildx", "history", "ls", "--builder", builder,
           "--filter", "status=running", "--format", "{{json .}}"):
        raise Skip("the builder has an active build")


def candidates(output):
    selected = {}
    for line in output.splitlines():
        record = json.loads(line)
        if (record.get("Reclaimable") is not True or record.get("Shared") is not False
                or record.get("Mutable") is not False or record.get("Type") != "regular"):
            continue
        entry_id = record.get("ID", "")
        age = re.fullmatch(r"(\d+) (hours?|days?|weeks?|months?|years?) ago",
                           record.get("LastUsedAt") or "")
        size = re.fullmatch(r"(\d+(?:\.\d+)?)\s*(B|kB|KB|MB|GB|TB)", record.get("Size") or "")
        if not re.fullmatch(r"[a-z0-9]{25}", entry_id) or not age or not size:
            continue
        hours = {"hour": 1, "day": 24, "week": 168, "month": 672, "year": 8760}
        age_hours = int(age[1]) * hours[age[2].rstrip("s")]
        units = {"B": 1, "KB": 1000, "MB": 1000**2, "GB": 1000**3, "TB": 1000**4}
        size_bytes = int(Decimal(size[1]) * units[size[2].upper()])
        if age_hours >= 48 and 0 < size_bytes <= MAX_ESTIMATED_BYTES:
            selected[entry_id] = (age_hours, size_bytes)
    remaining = MAX_ESTIMATED_BYTES
    result = []
    for entry_id, (_, size_bytes) in sorted(selected.items(), key=lambda entry: entry[1], reverse=True):
        if size_bytes <= remaining:
            result.append({"id": entry_id, "estimated_bytes": size_bytes})
            remaining -= size_bytes
        if len(result) == MAX_RECORDS:
            break
    return result


@contextmanager
def audit_file():
    state = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state")) / "guardwsl"
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    flags = os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK
    directory = os.open(state, os.O_RDONLY | os.O_DIRECTORY | flags)
    try:
        directory_info = os.fstat(directory)
        if directory_info.st_uid != os.getuid() or directory_info.st_mode & 0o022:
            raise Skip("audit directory permissions are unsafe")
        fd = os.open("docker-build-cache.jsonl", os.O_WRONLY | os.O_CREAT | os.O_APPEND | flags,
                     0o600, dir_fd=directory)
        with os.fdopen(fd, "a") as logfile:
            info = os.fstat(logfile.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise Skip("audit file permissions are unsafe")
            fcntl.flock(logfile, fcntl.LOCK_EX | fcntl.LOCK_NB)
            yield logfile
    finally:
        os.close(directory)


def write_audit(logfile, event, args, record, **details):
    entry = {"time": datetime.now(timezone.utc).isoformat(), "event": event,
             "builder": args.builder, "project": str(args.dedicated_project), **record, **details}
    logfile.write(json.dumps(entry) + "\n")
    logfile.flush()
    os.fsync(logfile.fileno())


def execute(args, report, run=command):
    if any(os.environ.get(name) for name in ("DOCKER_HOST", "DOCKER_CONTEXT", "BUILDKIT_HOST")):
        raise Skip("Docker endpoint overrides must be unset")
    builder = args.builder
    if not args.apply:
        inspection = run("docker", "buildx", "inspect", builder, "--timeout", "5s")
        states = fields(inspection, "Status")
        if not states or any(state != "running" for state in states):
            raise Skip("builder is not running")
        output = run("docker", "buildx", "du", "--builder", builder, "--format", "json")
        report["inventory_candidates"] = candidates(output)
        return

    project = args.dedicated_project.resolve(strict=True)
    if project == Path("/") or project == Path.home() or not (project / ".git").exists():
        raise Skip("dedicated project must be a Git worktree")
    args.dedicated_project = project
    report["host_free_before"], pressure = free_space(run)
    if pressure not in {"critical", "emergency"}:
        raise Skip("host disk is above critical pressure")
    node = builder_node(run, builder)
    verify_idle(run, builder, project)
    report["candidates"] = candidates(run("docker", "buildx", "du", "--builder", builder,
                                          "--format", "json"))
    if not report["candidates"]:
        return

    with audit_file() as logfile:
        for record in report["candidates"]:
            _, pressure = free_space(run)
            if pressure not in {"critical", "emergency"}:
                raise Skip("host disk recovered above critical pressure")
            if builder_node(run, builder) != node:
                raise Skip("builder node changed")
            verify_idle(run, builder, project)
            write_audit(logfile, "intent", args, record)
            filters = ("id=" + record["id"], *FILTERS)
            flags = [arg for value in filters for arg in ("--filter", value)]
            try:
                output = run("docker", "buildx", "prune", "--builder", builder, "--force",
                             "--timeout", "5s", *flags, timeout=60)
            except Skip:
                write_audit(logfile, "unconfirmed", args, record)
                raise
            totals = fields(output, "Total reclaimed space") or fields(output, "Total")
            total = totals[0] if len(totals) == 1 else None
            write_audit(logfile, "completed", args, record, buildkit_total=total)
            report["completed"].append({"id": record["id"], "buildkit_total": total})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--builder", default="default")
    parser.add_argument("--dedicated-project", type=Path,
                        help="Git worktree that exclusively uses the named builder")
    parser.add_argument("--apply", action="store_true", help="enable cleanup under critical pressure")
    args = parser.parse_args()
    if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_.-]*", args.builder):
        parser.error("invalid builder name")
    if args.apply and (args.builder == "default" or not args.dedicated_project):
        parser.error("--apply requires a named dedicated builder and --dedicated-project")
    report = {"builder": args.builder, "preview": not args.apply, "completed": []}
    try:
        execute(args, report)
    except (Skip, OSError, ValueError, TypeError, KeyError) as error:
        report["skipped"] = str(error) if isinstance(error, Skip) else type(error).__name__
    finally:
        if report.get("host_free_before") is not None and report["completed"]:
            try:
                report["host_free_after"], _ = free_space(command)
            except (Skip, OSError, ValueError, TypeError):
                report["host_free_after"] = None
    print(json.dumps(report))


if __name__ == "__main__":
    main()
