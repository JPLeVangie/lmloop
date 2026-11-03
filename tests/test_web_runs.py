import hashlib
import json
import re
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import config as config_module
from web import runs


class PwaResumeRecoveryTests(unittest.TestCase):
    def test_gets_retry_and_resume_events_poll_immediately(self):
        app = (Path(__file__).parent.parent / "web" / "static" / "app.js").read_text()
        self.assertIn('method === "GET" ? [0, 250, 750] : [0]', app)
        self.assertIn('window.addEventListener("pageshow"', app)
        self.assertIn('window.addEventListener("online", resumePolling)', app)
        self.assertIn('void poll().finally(schedule)', app)

    # The shell as it stands, and the version it is cached under.  `sw.js`
    # serves the shell from cache and refreshes it in the background, so an
    # installed PWA shows the *previous* build for one more launch unless
    # `SHELL` changes -- and `sw.js` says so in its own header.
    #
    # Pinning the version alone, which is what this test used to do, cannot
    # notice: it fails when somebody bumps the number and passes when they
    # change the shell and forget.  A digest of the shell itself fails on
    # exactly the case that matters.  It caught its first one immediately --
    # three shell files changed in this session's own work with no bump.
    SHELL_VERSION = "lmloop-shell-v21"
    SHELL_DIGEST = "8dfba3e87980d6ed4cef07168ec20b2f40f6d9b41bcdd91c96898d471c3bb041"

    def test_the_shell_version_covers_the_shell_as_it_stands(self):
        static = Path(__file__).parent.parent / "web" / "static"
        worker = (static / "sw.js").read_text()
        self.assertIn(f'const SHELL = "{self.SHELL_VERSION}"', worker)
        digest = hashlib.sha256()
        for name in ("index.html", "app.js", "style.css"):
            digest.update((static / name).read_bytes())
        self.assertEqual(
            self.SHELL_DIGEST, digest.hexdigest(),
            "the cached shell changed: bump SHELL in web/static/sw.js and record "
            "the new digest here, or an installed PWA serves the old build",
        )

    def test_every_asset_the_worker_precaches_is_one_this_test_covers(self):
        """A guard on the guard: a fourth shell file would be uncovered."""
        worker = (Path(__file__).parent.parent / "web" / "static" / "sw.js").read_text()
        listed = set(re.findall(r'"(/[^"]*)"', worker.split("const ASSETS = [")[1].split("]")[0]))
        self.assertEqual({"/", "/static/app.js", "/static/style.css",
                          "/static/icon-192.png", "/manifest.json",
                          "/static/offline.html"}, listed)
from web.server import Handler


class ContextPressureTests(unittest.TestCase):
    """The loop knows an iteration is running out of room and says so; the run
    view is where that has to arrive.

    Without it a run that overflows reads as ordinary work followed by an
    inexplicable compaction -- which is exactly how the run that motivated
    `policy.CONTEXT_PRESSURE` looked: three iterations at 80-84%, then a
    compaction that cost the agent everything it had read.
    """

    def make_run(self, events):
        base = Path(tempfile.mkdtemp())
        run_dir = base / ".worktrees" / "r" / ".lmloop" / "runs" / "r"
        run_dir.mkdir(parents=True)
        (run_dir / "status.json").write_text(json.dumps({
            "phase": "stopped", "model": "local/model", "updated_at": "",
        }))
        (run_dir / "lmloop.log").write_text(
            "".join(json.dumps(event) + "\n" for event in events)
        )
        return {"id": "p", "path": str(base)}, run_dir

    def iterations(self, events):
        project, run_dir = self.make_run(events)
        return runs.detail(project, run_dir)["iterations"]

    def test_an_iteration_the_loop_flagged_carries_its_share_of_the_window(self):
        rows = self.iterations([
            {"event": "iteration:start", "iteration": 1},
            {"event": "iteration:end", "iteration": 1, "outcome": "ok",
             "totalInputTokens": 20599},
            {"event": "context:pressure", "iteration": 1,
             "inputTokens": 20599, "window": 24576},
        ])
        self.assertAlmostEqual(20599 / 24576, rows[0]["pressure"])
        self.assertEqual(24576, rows[0]["context_window"])

    def test_an_iteration_the_loop_did_not_flag_carries_nothing(self):
        """The threshold stays `policy`'s to decide.  A row with no `pressure`
        is not a row at 0% -- it is one the loop had nothing to say about."""
        rows = self.iterations([
            {"event": "iteration:start", "iteration": 1},
            {"event": "iteration:end", "iteration": 1, "outcome": "ok",
             "totalInputTokens": 4000},
        ])
        self.assertNotIn("pressure", rows[0])

    def test_the_window_is_read_per_iteration_and_not_per_run(self):
        """Planning and a thrash retry escalate to different models, so one
        run's iterations do not share a window."""
        rows = self.iterations([
            {"event": "iteration:start", "iteration": 1},
            {"event": "iteration:end", "iteration": 1, "outcome": "thrashing",
             "totalInputTokens": 20000},
            {"event": "context:pressure", "iteration": 1,
             "inputTokens": 20000, "window": 24576},
            {"event": "iteration:start", "iteration": 2},
            {"event": "iteration:end", "iteration": 2, "outcome": "ok",
             "totalInputTokens": 90000},
            {"event": "context:pressure", "iteration": 2,
             "inputTokens": 90000, "window": 106496},
        ])
        self.assertEqual([24576, 106496], [row["context_window"] for row in rows])

    def test_a_pressure_event_with_no_window_is_not_reported_as_zero(self):
        """`Run.window` is 0 for a model nobody measured, and a ratio against
        zero is an invented number rather than a missing one."""
        rows = self.iterations([
            {"event": "iteration:start", "iteration": 1},
            {"event": "iteration:end", "iteration": 1, "outcome": "ok",
             "totalInputTokens": 20000},
            {"event": "context:pressure", "iteration": 1,
             "inputTokens": 20000, "window": 0},
        ])
        self.assertNotIn("pressure", rows[0])


# A process that really is an lmloop program, for the holder case that asserts a
# live loop is reported.  A real one is `python3 /path/to/lmloop.py run ...`, so
# a file of that name is what makes this a positive rather than a lookalike --
# see `runrecord.is_lmloop_cmdline`, which matches the program and not the
# substring.
def spawn_fake_loop():
    directory = Path(tempfile.mkdtemp())
    script = directory / "lmloop.py"
    script.write_text("import time\ntime.sleep(300)\n")
    return subprocess.Popen([sys.executable, str(script)], start_new_session=True)


# The other half of lm-j44: a process that merely *mentions* lmloop, which used
# to be reported as the run's holder.  A `tail -f` on a run's log, an editor on
# a source file, a monitoring shell -- all of them said yes.
def spawn_mere_mention():
    return subprocess.Popen(
        [sys.executable, "-c", "import time  # lmloop\ntime.sleep(300)"],
        start_new_session=True,
    )


class NestedPilotDiscoveryTests(unittest.TestCase):
    def setUp(self):
        self.project = Path(tempfile.mkdtemp()) / "project"
        self.project.mkdir()

    @staticmethod
    def make_run(base: Path, run_id: str, agent: str = "omp") -> Path:
        run_dir = base / ".worktrees" / run_id / ".lmloop" / "runs" / run_id
        run_dir.mkdir(parents=True)
        (run_dir / "status.json").write_text(json.dumps({
            "phase": "stopped", "model": "local/model", "updated_at": "",
        }))
        (run_dir / "lmloop.log").write_text(json.dumps({
            "event": "run:start", "agent": agent,
        }) + "\n")
        return run_dir

    def test_nested_pilot_base_is_discovered(self):
        normal = self.make_run(self.project, "normal")
        pilot = self.make_run(self.project / ".pilot-bases" / "pinned", "pilot")
        self.assertEqual({normal, pilot}, set(runs.run_dirs(self.project)))

    def test_summary_surfaces_the_agent_from_run_start(self):
        run_dir = self.make_run(self.project, "pilot", agent="omp")
        summary = runs.summarise(
            {"id": "project", "path": str(self.project)}, run_dir
        )
        self.assertEqual("omp", summary["agent"])

    def test_nested_run_has_owning_base_and_distinct_route(self):
        base = self.project / ".pilot-bases" / "pinned"
        run_dir = self.make_run(base, "same")
        self.make_run(self.project, "same", agent="pi")
        self.assertEqual(base, runs.owner(self.project, run_dir))
        self.assertEqual("pinned::same", runs.route_id(self.project, run_dir))

    def test_follow_on_run_under_a_worktree_is_discovered_and_owned(self):
        parent = self.project / ".worktrees" / "first-run"
        child = self.make_run(parent, "follow-on")
        self.assertIn(child, runs.run_dirs(self.project))
        self.assertEqual(parent, runs.owner(self.project, child))
        self.assertEqual("first-run::follow-on", runs.route_id(self.project, child))

    def test_latest_start_event_controls_agent_label(self):
        run_dir = self.make_run(self.project, "resumed", agent="pi")
        with (run_dir / "lmloop.log").open("a") as log:
            log.write(json.dumps({"event": "run:start", "agent": "omp"}) + "\n")
        summary = runs.summarise({"id": "project", "path": str(self.project)}, run_dir)
        self.assertEqual("omp", summary["agent"])

    def test_summary_keeps_the_full_iteration_history(self):
        run_dir = self.make_run(self.project, "history")
        expected = ["ok"] * 13 + ["agent-error"] * 17
        with (run_dir / "lmloop.log").open("a") as log:
            for iteration, outcome in enumerate(expected, 1):
                log.write(json.dumps({
                    "event": "iteration:end", "iteration": iteration,
                    "outcome": outcome,
                }) + "\n")
        summary = runs.summarise({"id": "project", "path": str(self.project)}, run_dir)
        self.assertEqual(len(expected), summary["iterations_done"])
        self.assertEqual(expected, summary["outcomes"])

    def test_running_summary_calculates_eta_from_durable_events(self):
        run_dir = self.make_run(self.project, "eta")
        (run_dir / "plan.md").write_text("- [x] one\n- [x] two\n- [ ] three\n")
        (run_dir / "status.json").write_text(json.dumps({
            "phase": "working", "iteration": 3, "max_iterations": 6,
            "elapsed_seconds": 60, "updated_at": datetime.now(timezone.utc).isoformat(),
        }))
        with (run_dir / "lmloop.log").open("a") as log:
            log.write(json.dumps({"event": "iteration:end", "elapsedMs": 600000,
                                  "planDone": 1, "planTotal": 3, "outcome": "ok"}) + "\n")
            log.write(json.dumps({"event": "iteration:end", "elapsedMs": 1200000,
                                  "planDone": 2, "planTotal": 3, "outcome": "ok"}) + "\n")
        summary = runs.summarise({"id": "project", "path": str(self.project)}, run_dir)
        self.assertEqual("running", summary["state"])
        self.assertEqual(840, summary["eta_seconds"])
        self.assertEqual("plan steps", summary["eta_basis"])
        self.assertEqual(1860, summary["run_elapsed_seconds"])


class ArchiveTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.project = self.root / "project"
        self.project.mkdir()
        self.run_id = "finished-run"
        self.run_dir = (
            self.project / ".worktrees" / self.run_id
            / ".lmloop" / "runs" / self.run_id
        )
        self.run_dir.mkdir(parents=True)
        (self.run_dir / "prompt.md").write_text("Keep this history\n")
        (self.run_dir / "status.json").write_text(json.dumps({
            "phase": "completed", "updated_at": datetime.now(timezone.utc).isoformat(),
        }))
        (self.run_dir / "lmloop.log").write_text(json.dumps({
            "event": "run:complete", "commitCount": 2,
        }) + "\n")
        self.archive = self.root / "archive"

    def test_archived_run_remains_discoverable_and_read_only(self):
        target = self.archive / "project" / self.run_id
        target.mkdir(parents=True)
        for source in self.run_dir.iterdir():
            (target / source.name).write_bytes(source.read_bytes())

        with mock.patch.object(runs, "ARCHIVE_ROOT", self.archive):
            summary = runs.summarise(
                {"id": "project", "path": str(self.project)}, target
            )
            self.assertEqual([target], runs.archived_dirs("project"))
            self.assertEqual("archived", summary["state"])
            self.assertTrue(summary["archived"])
            self.assertEqual("Keep this history", summary["objective"])
            self.assertEqual(2, summary["commits"])
            self.assertEqual("", runs.detail(
                {"id": "project", "path": str(self.project)}, target
            )["worktree"])

    def test_archive_never_forces_worktree_removal(self):
        handler = Handler.__new__(Handler)
        completed = SimpleNamespace(returncode=0, stdout="", stderr="")
        (self.run_dir.parents[2] / ".pi").mkdir()
        (self.run_dir.parents[2] / ".pi" / "settings.json").write_text("generated")
        (self.project / ".venv").mkdir()
        (self.run_dir.parents[2] / ".venv").symlink_to(self.project / ".venv")

        with mock.patch.object(runs, "ARCHIVE_ROOT", self.archive), \
             mock.patch("web.server.subprocess.run", return_value=completed) as run, \
             mock.patch.object(Handler, "json") as reply:
            handler.archive_run(
                {"id": "project", "path": str(self.project)}, self.run_dir
            )

        payload = reply.call_args.args[0]
        self.assertTrue(payload["archived"])
        argv = run.call_args.args[0]
        self.assertEqual(["git", "worktree", "remove", str(self.run_dir.parents[2])], argv)
        self.assertNotIn("--force", argv)
        self.assertFalse((self.run_dir.parents[2] / ".pi" / "settings.json").exists())
        self.assertFalse((self.run_dir.parents[2] / ".venv").exists())

    def test_existing_archive_never_merges_or_removes_source(self):
        target = self.archive / "project" / self.run_id
        target.mkdir(parents=True)
        (target / "stale").write_text("older record")
        handler = Handler.__new__(Handler)

        with mock.patch.object(runs, "ARCHIVE_ROOT", self.archive), \
             mock.patch.object(Handler, "json") as reply, \
             mock.patch("web.server.subprocess.run") as run:
            handler.archive_run(
                {"id": "project", "path": str(self.project)}, self.run_dir
            )

        self.assertEqual(409, reply.call_args.args[1])
        self.assertTrue(self.run_dir.is_dir())
        self.assertEqual("older record", (target / "stale").read_text())
        run.assert_not_called()

    def test_failed_copy_leaves_no_archive_and_retry_succeeds(self):
        import shutil

        handler = Handler.__new__(Handler)
        real_copytree = shutil.copytree
        completed = SimpleNamespace(returncode=0, stdout="", stderr="")

        with mock.patch.object(runs, "ARCHIVE_ROOT", self.archive), \
             mock.patch.object(Handler, "json") as reply, \
             mock.patch("shutil.copytree", side_effect=OSError("disk full")):
            handler.archive_run(
                {"id": "project", "path": str(self.project)}, self.run_dir
            )

        target = self.archive / "project" / self.run_id
        self.assertEqual(500, reply.call_args.args[1])
        self.assertFalse(target.exists())
        self.assertEqual([], list(target.parent.glob(f".{self.run_id}.*")))

        with mock.patch.object(runs, "ARCHIVE_ROOT", self.archive), \
             mock.patch.object(Handler, "json") as reply, \
             mock.patch("shutil.copytree", side_effect=real_copytree), \
             mock.patch("web.server.subprocess.run", return_value=completed):
            handler.archive_run(
                {"id": "project", "path": str(self.project)}, self.run_dir
            )

        self.assertTrue(reply.call_args.args[0]["archived"])
        self.assertTrue((target / "prompt.md").is_file())

    def test_failed_non_forced_removal_keeps_verified_archive(self):
        handler = Handler.__new__(Handler)
        refused = SimpleNamespace(returncode=1, stdout="", stderr="contains modified files")

        with mock.patch.object(runs, "ARCHIVE_ROOT", self.archive), \
             mock.patch("web.server.subprocess.run", return_value=refused) as run, \
             mock.patch.object(Handler, "json") as reply:
            handler.archive_run(
                {"id": "project", "path": str(self.project)}, self.run_dir
            )

        self.assertEqual(500, reply.call_args.args[1])
        self.assertIn("source record was restored", reply.call_args.args[0]["error"])
        self.assertTrue((self.archive / "project" / self.run_id / "prompt.md").is_file())
        self.assertTrue((self.run_dir / "prompt.md").is_file())
        self.assertNotIn("--force", run.call_args.args[0])


class ResolvedMetadataTests(unittest.TestCase):
    """Regression coverage for the audit finding that delete/PR reconstruct
    `lmloop/<run-id>` instead of reading the branch `run:start` actually
    recorded -- wrong for any project with a configured `[worktree] branch`
    template. (`archive_run`'s `run_dir.parents[2]` is a different case: that
    walk is always correct, since `.lmloop/runs/<id>` is a fixed relative
    layout under whatever path the worktree happens to be at -- so it needed
    no fix, only the branch guess did.) See lm-ka5.4 notes.
    """

    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.project = self.root / "project"
        self.project.mkdir()
        self.run_id = "custom-run"
        self.worktree = self.project / ".worktrees" / self.run_id
        self.run_dir = self.worktree / ".lmloop" / "runs" / self.run_id
        self.run_dir.mkdir(parents=True)
        self.branch = "custom/branch-name"
        self.run_dir.joinpath("prompt.md").write_text("Keep this history\n")
        self.run_dir.joinpath("status.json").write_text(json.dumps({
            "phase": "completed", "updated_at": datetime.now(timezone.utc).isoformat(),
        }))
        self.run_dir.joinpath("lmloop.log").write_text(
            json.dumps({
                "event": "run:start", "worktreePath": str(self.worktree),
                "branch": self.branch, "repoPath": str(self.project),
            }) + "\n" + json.dumps({"event": "run:complete", "commitCount": 1}) + "\n"
        )
        self.archive = self.root / "archive"
        # Sanity: the naive branch guess this replaces must actually be wrong
        # here, or the test would pass whether or not the fix exists.
        assert f"lmloop/{self.run_id}" != self.branch

    def test_delete_drops_the_persisted_branch_not_a_guessed_name(self):
        target = self.archive / "project" / self.run_id
        target.mkdir(parents=True)
        for source in self.run_dir.iterdir():
            (target / source.name).write_bytes(source.read_bytes())
        handler = Handler.__new__(Handler)
        with mock.patch.object(runs, "ARCHIVE_ROOT", self.archive), \
             mock.patch("web.server.subprocess.run") as run, \
             mock.patch.object(Handler, "json"):
            handler.delete_run(
                {"id": "project", "path": str(self.project)}, target, {"branch": True}
            )
        argv = run.call_args.args[0]
        self.assertEqual(["git", "branch", "-D", self.branch], argv)

    def test_open_pr_targets_the_persisted_branch_not_a_guessed_name(self):
        handler = Handler.__new__(Handler)
        verify = SimpleNamespace(returncode=0, stdout="", stderr="")
        with mock.patch("web.server.subprocess.run", return_value=verify) as run, \
             mock.patch.object(Handler, "json"):
            handler.open_pr(
                {"id": "project", "path": str(self.project)}, self.run_dir, {}
            )
        first_argv = run.call_args_list[0].args[0]
        self.assertIn(self.branch, first_argv)

    def test_owner_prefers_the_persisted_repo_path_over_the_heuristic(self):
        """A run:start with `repoPath` is ground truth; the pilot-base/nested-
        worktree walk in `owner()` only has to guess for runs that predate it.
        """
        # A layout the walk-based heuristic would misattribute if it ran: no
        # `.pilot-bases` and no nested `.worktrees` chain lead back to
        # `self.project` from this run_dir, so only the persisted repoPath
        # can produce the right answer.
        unrelated = self.root / "unrelated"
        unrelated.mkdir()
        self.assertEqual(self.project, runs.owner(unrelated, self.run_dir))


class PlanReaderConsolidationTests(unittest.TestCase):
    """`web.runs._plan_progress`/`_current_step` must keep reading a plan
    exactly as before once they delegate their checkbox scan to
    `runrecord.plan_progress`/`runrecord.first_unchecked_step`."""

    def test_plan_progress_matches_rundir(self):
        text = "- [x] one\n- [X] two\n- [ ] three\nnot a step\n"
        self.assertEqual((2, 3), runs._plan_progress(text))

    def test_current_step_strips_markdown_and_all_backticks_and_truncates(self):
        text = "- [x] done\n- [ ] **fix `foo.py`** bug\n"
        self.assertEqual("fix foo.py bug", runs._current_step(text))
        long_step = "- [ ] " + "x" * 200
        self.assertEqual(160, len(runs._current_step(long_step)))

    def test_current_step_empty_plan_is_empty_string(self):
        self.assertEqual("", runs._current_step(""))


class WorktreeRootConsolidationTests(unittest.TestCase):
    """`web.runs._worktree_root` used to read a project's own `.lmloop.toml`
    directly with `tomllib`, bypassing `config.load`'s defaults-then-global-
    then-project layering entirely.  A `[worktree] root` set only in
    `~/.config/lmloop/config.toml` -- which is exactly how the CLI's own
    `lmloop._discover_runs` resolves the same setting -- was invisible to the
    dashboard, which would then report the project as having no runs at all.
    """

    def test_worktree_root_respects_a_global_config_override(self):
        project = Path(tempfile.mkdtemp())
        global_config = Path(tempfile.mkdtemp()) / "config.toml"
        global_config.write_text('[worktree]\nroot = "{repo}/custom-trees/{run_id}"\n')
        with mock.patch.object(config_module, "GLOBAL_CONFIG", global_config):
            self.assertEqual(
                project / "custom-trees", runs._worktree_root(project),
            )

    def test_worktree_root_still_defaults_with_no_config_at_all(self):
        project = Path(tempfile.mkdtemp())
        with mock.patch.object(config_module, "GLOBAL_CONFIG", project / "no-such-file"):
            self.assertEqual(project / ".worktrees", runs._worktree_root(project))


class EventLogCapTests(unittest.TestCase):
    """`web.runs._events` used to read `lmloop.log` through the same
    200_000-character cap used for previewing `plan.md`/`handoff.md` -- fine
    for those, wrong for an append-only lifecycle log a long, many-iteration
    run can grow past that size, since the cap was anchored to the *start* of
    the file and so hid the most recent events, not the least useful ones.
    `RunDir.read_events` (the runner's own reader) never had this cap.
    """

    def test_events_sees_a_run_complete_written_past_the_old_two_hundred_kb_mark(self):
        run_dir = Path(tempfile.mkdtemp())
        padding_line = json.dumps({"event": "iteration:end", "outcome": "ok", "pad": "x" * 500})
        lines = [padding_line] * 500 + [json.dumps({"event": "run:complete", "commitCount": 3})]
        log_text = "\n".join(lines) + "\n"
        (run_dir / "lmloop.log").write_text(log_text)
        self.assertGreater(len(log_text), 200_000)
        self.assertIn("run:complete", [event["event"] for event in runs._events(run_dir)])


class ControlSentinelConsolidationTests(unittest.TestCase):
    """`_state`/`summarise` read STOP/STOP-NOW/PAUSE by checking file
    existence directly, duplicating `RunDir.stop_requested`/`.paused`. Pins
    that reading through `runrecord` still reflects exactly the files on disk.
    """

    def setUp(self):
        self.run_dir = Path(tempfile.mkdtemp())
        (self.run_dir / "status.json").write_text(json.dumps({
            "phase": "working", "updated_at": datetime.now(timezone.utc).isoformat(),
        }))
        (self.run_dir / "lmloop.log").write_text("")

    def summary(self):
        return runs.summarise({"id": "p", "path": str(self.run_dir)}, self.run_dir)

    def test_no_sentinels_present(self):
        summary = self.summary()
        self.assertFalse(summary["paused"])
        self.assertFalse(summary["stopping"])
        self.assertEqual("running", summary["state"])

    def test_pause_sentinel_is_reflected(self):
        (self.run_dir / "PAUSE").write_text("")
        summary = self.summary()
        self.assertTrue(summary["paused"])
        self.assertEqual("paused", summary["state"])

    def test_either_stop_sentinel_sets_stopping(self):
        (self.run_dir / "STOP-NOW").write_text("")
        summary = self.summary()
        self.assertTrue(summary["stopping"])
        self.assertEqual("stopping", summary["state"])


class HolderCharacterizationTests(unittest.TestCase):
    """Pins `runs._holder` before it delegates to the canonical reader.

    lm-ka5.4 consolidates this with `rundir.RunDir.holder` (see
    `RunDirCharacterizationTests` in test_lmloop.py) behind `runrecord.holder`.
    Unlike RunDir's version, this one is read by a process that is never the
    loop itself, so it has no self-pid exclusion -- that asymmetry is
    intentional and must survive the consolidation.
    """

    def setUp(self):
        self.run_dir = Path(tempfile.mkdtemp())

    def test_no_pid_file_is_zero(self):
        self.assertEqual(0, runs._holder(self.run_dir))

    def test_garbage_content_is_zero(self):
        (self.run_dir / "loop.pid").write_text("not-a-pid\n")
        self.assertEqual(0, runs._holder(self.run_dir))

    def test_dead_pid_is_zero(self):
        proc = subprocess.Popen(["true"])
        proc.wait()
        (self.run_dir / "loop.pid").write_text(f"{proc.pid}\n")
        self.assertEqual(0, runs._holder(self.run_dir))

    def test_live_pid_without_lmloop_in_cmdline_is_zero(self):
        proc = subprocess.Popen(["sleep", "300"], start_new_session=True)
        try:
            (self.run_dir / "loop.pid").write_text(f"{proc.pid}\n")
            self.assertEqual(0, runs._holder(self.run_dir))
        finally:
            proc.kill()
            proc.wait()

    def test_a_live_pid_running_lmloop_is_reported(self):
        proc = spawn_fake_loop()
        try:
            (self.run_dir / "loop.pid").write_text(f"{proc.pid}\n")
            self.assertEqual(proc.pid, runs._holder(self.run_dir))
        finally:
            proc.kill()
            proc.wait()

    def test_a_process_that_merely_mentions_lmloop_is_not_the_holder(self):
        """lm-j44.  Errs safe for archiving -- a false holder refuses to remove
        a worktree -- but not for `_state`, where it keeps a dead run showing
        as running or paused instead of stale."""
        proc = spawn_mere_mention()
        try:
            (self.run_dir / "loop.pid").write_text(f"{proc.pid}\n")
            self.assertEqual(0, runs._holder(self.run_dir))
        finally:
            proc.kill()
            proc.wait()


if __name__ == "__main__":
    unittest.main()
