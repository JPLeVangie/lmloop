"""Staged review and operator approval -- docs/review.md."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT), str(ROOT / "web")]

import config  # noqa: E402
import loop  # noqa: E402
import review  # noqa: E402
import service  # noqa: E402


class SelectionTests(unittest.TestCase):
    def names(self, changed, objective="", diff="", pinned=()):
        return [n for n, _ in review.select(changed, objective, "", diff, list(pinned))]

    def test_correctness_always_and_others_by_evidence(self):
        self.assertEqual(self.names(["README.md"]), ["correctness"])
        self.assertEqual(self.names(["web/server.py"]), ["correctness", "security"])
        self.assertEqual(self.names(["a.py"], diff="+subprocess.run(x, shell=True)"),
                         ["correctness", "security"])
        self.assertEqual(self.names(["a.py"], objective="make startup faster"),
                         ["correctness", "performance"])
        self.assertEqual(self.names(["web/static/app.css"]), ["correctness", "design"])

    def test_pinned_replaces_selection(self):
        self.assertEqual(self.names(["web/server.py"], pinned=["design"]), ["design"])

    def test_every_builtin_has_a_brief(self):
        for name in (*review.ORDER, "rules"):
            self.assertTrue(review.brief(name).strip(), name)
        with self.assertRaises(SystemExit):
            review.brief("nobody")


class VerdictTests(unittest.TestCase):
    def test_parse(self):
        self.assertEqual(review.parse("verdict: APPROVED\n"), ("APPROVED", []))
        self.assertEqual(review.parse("Verdict: **changes_requested**\n- [a.py:1] fix x\n* do y"),
                         ("CHANGES_REQUESTED", ["[a.py:1] fix x", "do y"]))
        self.assertEqual(review.parse("looks fine to me"), ("", []))


class ConfigTests(unittest.TestCase):
    def test_review_section_validates(self):
        ok = {"review": {"max_rounds": 1, "every": 0, "personas": ["design"],
                         "models": {"design": "llama-swap/X"}}}
        self.assertEqual(config.validate(ok, Path("g.toml")), [])
        for bad in ({"max_rounds": -1}, {"every": "5"}, {"personas": [1]},
                    {"models": {"design": 3}}, {"rounds": 2}):
            self.assertTrue(config.validate({"review": bad}, Path("g.toml")), bad)

    def test_layers_global_then_project_then_run(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "global.toml").write_text("[review]\nmax_rounds = 4\nevery = 7\n")
            (root / ".lmloop.toml").write_text("[review]\nevery = 3\n[review.models]\ndesign = \"m\"\n")
            with mock.patch.object(config, "GLOBAL_CONFIG", root / "global.toml"):
                cfg = config.load(root)
        self.assertEqual((cfg["review"]["max_rounds"], cfg["review"]["every"]), (4, 3))
        self.assertEqual(cfg["review"]["models"], {"design": "m"})
        config.override_review(cfg, 0, None, "security, design")
        self.assertEqual(cfg["review"]["max_rounds"], 0)
        self.assertEqual(cfg["review"]["every"], 3)
        self.assertEqual(cfg["review"]["personas"], ["security", "design"])
        with self.assertRaises(SystemExit):
            config.override_review(cfg, -1, None, None)

    def test_dashboard_turns_fields_into_flags(self):
        project = {"id": "p", "path": "/tmp"}
        cfg = {"roots": [], "python": "py", "default_max_iterations": 3,
               "default_model": "", "default_thinking": ""}
        done = mock.Mock(returncode=0, stdout="ok", stderr="")
        with mock.patch.object(service.runs_module, "projects", return_value=[project]), \
             mock.patch.object(service.subprocess, "run", return_value=done) as run:
            for bad in ({"review_rounds": "-1"}, {"review_every": "x"},
                        {"review_personas": "a;rm -rf"}):
                code, _ = service.start_run({"project": "p", "objective": "o", **bad}, cfg, "l")
                self.assertEqual(code, 400, bad)
            code, _ = service.start_run({"project": "p", "objective": "o", "review_rounds": "0",
                                         "review_every": "", "review_personas": "design"}, cfg, "l")
        self.assertEqual(code, 200)
        argv = run.call_args.args[0]
        self.assertEqual(argv[argv.index("--review-rounds") + 1], "0")
        self.assertNotIn("--review-every", argv)
        self.assertEqual(argv[argv.index("--review-personas") + 1], "design")


def git(cwd, *args):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True,
                          text=True).stdout.strip()


class GateTests(unittest.TestCase):
    """`_review_gate` against a real worktree, with the reviewer stubbed."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        git(root, "init", "-q", "-b", "main")
        git(root, "config", "user.email", "t@t")
        git(root, "config", "user.name", "t")
        (root / "a.py").write_text("x = 1\n")
        git(root, "add", "-A")
        git(root, "commit", "-qm", "seed")
        base = git(root, "rev-parse", "HEAD")
        (root / "a.py").write_text("x = 2\n")
        git(root, "commit", "-qam", "work")
        rundir = root / "run"
        rundir.mkdir()
        (rundir / "plan.md").write_text("- [x] step\n")
        self.run = SimpleNamespace(
            worktree=root, objective="o", review_round=0, review_coverage={},
            interrupted=False,
            config={"review": {"max_rounds": 2, "every": 0, "personas": [], "models": {}}},
            rundir=SimpleNamespace(path=rundir, base_commit=base, plan_path=rundir / "plan.md",
                                   read_plan=lambda: (rundir / "plan.md").read_text()),
            _save_run_state=lambda: None,
        )
        self.run._add_findings = lambda tag, f: loop.Run._add_findings(self.run, tag, f)

    def tearDown(self):
        self.tmp.cleanup()

    def gate(self, verdicts):
        self.run._review_once = mock.Mock(side_effect=verdicts)
        return loop.Run._review_gate(self.run)

    def test_approved(self):
        self.assertEqual(self.gate([("APPROVED", [])]), "")
        self.assertEqual(self.run.review_coverage, {"correctness": "APPROVED"})

    def test_changes_become_plan_items_then_rounds_run_out(self):
        self.assertEqual(self.gate([("CHANGES_REQUESTED", ["[a.py:1] use 3"])]), "changes")
        self.assertIn("- [ ] [a.py:1] use 3 (review r1/correctness)", self.run.rundir.read_plan())
        self.assertEqual(self.gate([("", [])]), "changes")           # no verdict
        self.assertIn("produced no verdict", self.run.rundir.read_plan())
        self.assertEqual(self.gate([]),
                         " (review unresolved; correctness never got a verdict")

    def test_later_personas_still_review_after_changes(self):
        (self.run.worktree / "a.css").write_text("a{}\n")
        git(self.run.worktree, "add", "-A")
        git(self.run.worktree, "commit", "-qm", "css")
        self.assertEqual(self.gate([("CHANGES_REQUESTED", ["fix x"]),
                                    ("CHANGES_REQUESTED", ["space buttons"])]), "changes")
        self.assertEqual([c.args[1] for c in self.run._review_once.call_args_list],
                         ["correctness", "design"])
        self.assertIn("space buttons (review r1/design)", self.run.rundir.read_plan())

    def test_off(self):
        self.run.config["review"]["max_rounds"] = 0
        self.assertEqual(self.gate([]), " (review off)")

    def test_exhaustion_names_personas_without_verdicts(self):
        # The slots failure: the diff touches CSS, so design is selected,
        # but the reviewer never returns a verdict.  Exhaustion must name
        # every persona that was never reviewed, not just "review unresolved".
        (self.run.worktree / "a.css").write_text("a{}\n")
        git(self.run.worktree, "add", "-A")
        git(self.run.worktree, "commit", "-qm", "css")
        self.assertEqual(self.gate([("", []), ("", [])]), "changes")
        self.assertEqual(self.gate([("", []), ("", [])]), "changes")
        self.assertEqual(self.gate([]),
                         " (review unresolved; correctness and design never got a verdict")
        self.assertEqual(self.run.review_coverage,
                         {"correctness": "no verdict", "design": "no verdict"})

    def test_exhaustion_after_verdicts_is_bare_unresolved(self):
        (self.run.worktree / "a.css").write_text("a{}\n")
        git(self.run.worktree, "add", "-A")
        git(self.run.worktree, "commit", "-qm", "css")
        self.assertEqual(self.gate([("CHANGES_REQUESTED", ["fix x"]), ("APPROVED", [])]),
                         "changes")
        self.assertEqual(self.gate([("CHANGES_REQUESTED", ["fix x"]), ("APPROVED", [])]),
                         "changes")
        # Both personas did get verdicts; the operator sees the bare flag and
        # the findings, not a false "never got a verdict".
        self.assertEqual(self.gate([]), " (review unresolved)")
        self.assertEqual(self.run.review_coverage,
                         {"correctness": "CHANGES_REQUESTED", "design": "APPROVED"})


class ApprovalTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self.tmp.name)
        git(self.repo, "init", "-q", "-b", "main")
        git(self.repo, "config", "user.email", "t@t")
        git(self.repo, "config", "user.name", "t")
        (self.repo / "a").write_text("1\n")
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-qm", "seed")
        git(self.repo, "branch", "lmloop/r1")
        git(self.repo, "checkout", "-q", "lmloop/r1")
        (self.repo / "a").write_text("2\n")
        git(self.repo, "commit", "-qam", "work")
        git(self.repo, "checkout", "-q", "main")
        self.run_dir = self.repo / "runs" / "r1"
        self.run_dir.mkdir(parents=True)
        (self.run_dir / "plan.md").write_text("- [x] a\n")
        (self.run_dir / "run-state.json").write_text(json.dumps({"issue": "x-1"}))
        (self.run_dir / "lmloop.log").write_text(json.dumps(
            {"event": "run:start", "branch": "lmloop/r1"}) + "\n")
        self.wait(True)
        self.project = {"id": "p", "path": str(self.repo)}

    def tearDown(self):
        self.tmp.cleanup()

    def wait(self, flag):
        (self.run_dir / "status.json").write_text(json.dumps({"awaiting_approval": flag}))

    def decide(self, action, note=""):
        with mock.patch.object(service.runs_module, "_holder", return_value=0), \
             mock.patch.object(service, "control", return_value=(200, {"continued": True})), \
             mock.patch.object(service.subprocess, "run", wraps=subprocess.run) as run:
            code, reply = service.approval(self.project, self.run_dir,
                                           {"action": action, "note": note}, {}, "l")
        bd = [c.args[0] for c in run.call_args_list if c.args[0][0] == "bd"]
        return code, reply, bd

    def test_approve_fast_forwards_and_closes_the_issue_once(self):
        code, reply, bd = self.decide("approve")
        self.assertEqual(code, 200, reply)
        self.assertEqual(git(self.repo, "rev-parse", "main"), git(self.repo, "rev-parse", "lmloop/r1"))
        self.assertEqual(bd[0][:3], ["bd", "close", "x-1"])
        self.assertEqual(self.decide("approve")[0], 409)          # already decided

    def test_approve_refuses_when_base_moved(self):
        (self.repo / "b").write_text("x\n")
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-qm", "moved")
        code, reply, bd = self.decide("approve")
        self.assertEqual(code, 409)
        self.assertIn("fast-forward", reply["error"])
        self.assertEqual(bd, [])

    def test_request_changes_appends_operator_items_and_continues(self):
        self.assertEqual(self.decide("request_changes")[0], 400)  # note required
        code, reply, _ = self.decide("request_changes", "- rename x\nadd a test")
        self.assertEqual((code, reply), (200, {"continued": True}))
        plan = (self.run_dir / "plan.md").read_text()
        self.assertIn("- [ ] rename x (operator)", plan)
        self.assertIn("- [ ] add a test (operator)", plan)

    def test_reject_releases_the_issue_and_keeps_the_branch(self):
        code, _, bd = self.decide("reject", "wrong approach")
        self.assertEqual(code, 200)
        self.assertIn("--assignee", bd[0])
        git(self.repo, "rev-parse", "lmloop/r1")                   # still there

    def test_only_when_awaiting(self):
        self.wait(False)
        self.assertEqual(self.decide("approve")[0], 409)
        self.assertEqual(self.decide("maybe")[0], 400)


if __name__ == "__main__":
    unittest.main()
