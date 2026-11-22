"""Exercise the Docker cache helper without a running daemon or any deletion."""

from argparse import Namespace
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SOURCE = Path(__file__).parents[1] / "scripts/docker-build-cache.py"
SPEC = importlib.util.spec_from_file_location("docker_build_cache", SOURCE)
cache = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cache)


def record(entry_id="a" * 25, **overrides):
    return json.dumps({"ID": entry_id, "LastUsedAt": "3 days ago", "Size": "1GB",
                       "Shared": False, "Reclaimable": True, "Mutable": False,
                       "Type": "regular", **overrides})


class DockerCacheTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.project = Path(self.directory.name) / "project"
        self.project.mkdir()
        (self.project / ".git").mkdir()
        self.args = Namespace(apply=True, builder="project", dedicated_project=self.project)
        self.report = {"completed": []}
        self.audit = Path(self.directory.name) / "guardwsl/docker-build-cache.jsonl"
        self.enterContext(patch.dict(cache.os.environ, {"XDG_STATE_HOME": self.directory.name,
                                                        "DOCKER_HOST": "", "DOCKER_CONTEXT": "",
                                                        "BUILDKIT_HOST": ""}))
        self.enterContext(patch.object(cache, "project_idle"))
        self.enterContext(patch.object(cache, "builder_node", return_value=("node", "container")))
        self.status = self.enterContext(patch.object(cache, "free_space", return_value=(1, "critical")))
        self.commands = []
        self.histories = iter(())

    def run_docker(self, *args, **kwargs):
        if args[0] == "git":
            return f"worktree {self.project}\0HEAD 1234\0\0"
        self.commands.append(args)
        if args[2] == "inspect":
            return "Status: running"
        if args[2] == "du":
            return record()
        if args[2] == "history":
            return next(self.histories, "")
        if args[2] == "prune":
            self.assertEqual(json.loads(self.audit.read_text().splitlines()[-1])["event"], "intent")
            return "Total reclaimed space: 1GB"
        self.fail(f"Unexpected command {args}")

    def test_preview_only_queries_running_builder(self):
        self.args.apply = False
        self.args.builder = "default"
        cache.execute(self.args, self.report, self.run_docker)
        self.assertEqual([cmd[2] for cmd in self.commands], ["inspect", "du"])
        self.assertEqual(len(self.report["inventory_candidates"]), 1)
        self.assertFalse(self.audit.exists())

    def test_pressure_above_critical_skips_docker(self):
        self.status.return_value = (1, "pressure")
        with self.assertRaisesRegex(cache.Skip, "above critical"):
            cache.execute(self.args, self.report, self.run_docker)
        self.assertFalse(self.commands)

    def test_running_build_skips_inventory(self):
        self.histories = iter(("{}",))
        with self.assertRaisesRegex(cache.Skip, "active build"):
            cache.execute(self.args, self.report, self.run_docker)
        self.assertEqual([cmd[2] for cmd in self.commands], ["history"])
        self.assertFalse(self.audit.exists())

    def test_build_started_after_inventory_skips_prune(self):
        self.histories = iter(("", "{}"))
        with self.assertRaisesRegex(cache.Skip, "active build"):
            cache.execute(self.args, self.report, self.run_docker)
        self.assertEqual([cmd[2] for cmd in self.commands], ["history", "du", "history"])
        self.assertEqual(self.audit.read_text(), "")

    def test_project_session_skips_prune(self):
        cache.project_idle.side_effect = cache.Skip("project active")
        with self.assertRaisesRegex(cache.Skip, "project active"):
            cache.execute(self.args, self.report, self.run_docker)
        self.assertFalse(self.commands)
        self.assertFalse(self.audit.exists())

    def test_related_worktrees_are_checked_for_active_sessions(self):
        peer = Path(self.directory.name) / "peer"
        worktrees = f"worktree {self.project}\0HEAD 1234\0\0worktree {peer}\0HEAD 5678\0\0"

        def run(*args, **kwargs):
            return worktrees if args[0] == "git" else self.run_docker(*args, **kwargs)

        cache.verify_idle(run, self.args.builder, self.project)
        cache.project_idle.assert_called_once_with({self.project, peer})

    def test_prune_targets_exact_id_and_is_logged(self):
        cache.execute(self.args, self.report, self.run_docker)
        self.assertEqual([cmd[2] for cmd in self.commands], ["history", "du", "history", "prune"])
        self.assertEqual(self.commands[-1][-12:],
                         ("--filter", "id=" + "a" * 25, "--filter", "until=48h",
                          "--filter", "inuse=false", "--filter", "shared=false",
                          "--filter", "mutable=false", "--filter", "type=regular"))
        entries = [json.loads(line) for line in self.audit.read_text().splitlines()]
        self.assertEqual([entry["event"] for entry in entries], ["intent", "completed"])
        self.assertEqual(self.report["completed"][0]["buildkit_total"], "1GB")

    def test_candidate_inventory_keeps_only_old_private_regular_records_under_budget(self):
        entries = [
            record(),
            record("b" * 25, Shared=True),
            record("c" * 25, Reclaimable=False),
            record("d" * 25, Mutable=True),
            record("e" * 25, LastUsedAt="47 hours ago"),
            record("f" * 25, LastUsedAt=""),
            record("g" * 25, Size="4.9GB"),
            record("h" * 25, Type="exec.cachemount"),
            record("i" * 25, Size="6GB"),
        ]
        self.assertEqual(cache.candidates("\n".join(entries)),
                         [{"id": "g" * 25, "estimated_bytes": 4_900_000_000}])


class BuilderIdentityTest(unittest.TestCase):
    def test_default_builder_refused_for_apply(self):
        def run(*args):
            self.assertEqual(args[2], "inspect")
            return "Name: default\nDriver: docker\nName: default\nStatus: running"

        with self.assertRaisesRegex(cache.Skip, "must be dedicated"):
            cache.builder_node(run, "default")

    def test_named_builder_rejects_custom_volume_options(self):
        with tempfile.TemporaryDirectory() as directory:
            builder = "project"
            node = "project0"
            container = "buildx_buildkit_" + node
            volume = container + "_state"
            root = Path(directory)
            source = root / "volumes" / volume / "_data"
            mount = {"Destination": "/var/lib/buildkit", "Type": "volume", "Driver": "local",
                     "Name": volume, "Source": str(source)}
            storage = {"Driver": "local", "Mountpoint": str(source), "Options": None}

            def run(*args):
                if args[1] == "buildx":
                    return (f"Name: {builder}\nDriver: docker-container\nName: {node}\n"
                            "Endpoint: unix:///var/run/docker.sock\nStatus: running")
                if args[3] == "info":
                    return directory
                if args[3] == "inspect":
                    return "a" * 64 + " " + json.dumps([mount])
                if args[3] == "volume":
                    return json.dumps(storage)
                self.fail(f"Unexpected command {args}")

            self.assertEqual(cache.builder_node(run, builder), (node, "a" * 64))
            storage["Options"] = {"device": ":/remote", "type": "nfs"}
            with self.assertRaisesRegex(cache.Skip, "custom storage"):
                cache.builder_node(run, builder)


if __name__ == "__main__":
    unittest.main()
