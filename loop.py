"""The run lifecycle: create a worktree, iterate, commit, stop.

The whole design fits in one sentence: **the loop never discards work.**  The predecessor
calls ``git reset --hard HEAD && git clean -fd`` on any failed iteration, which
is a sound trade when an iteration costs ninety seconds and an unreliable one is
cheaper to redo than to reason about.  At ninety minutes it is not: one run
produced 87 minutes of correct, coherent edits and lost every line because the
agent's final message was not parseable JSON.  The code was fine.  The envelope
was not.

So every iteration that leaves a diff produces a commit, labelled with what
actually happened, and the next iteration continues from it.  `git log` becomes
a truthful record of the run, and anything unwanted is one `git revert` away at
a time of the operator's choosing rather than the loop's.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import time
from pathlib import Path

import browser
import checks
import config
import display
import env as envpolicy
import eta
import gitops
import harness
import models
import policy
import prompts
import pi_runner
import review
import runrecord
from rundir import RunDir, make_run_id, previous_runs


GATE_TERM_GRACE_SECONDS = 5

# Shipped with lmloop, appended to the agent's system prompt in any repository
# that has a `.beads/` directory.
BEADS_SKILL = Path(__file__).resolve().parent / "web" / "skills" / "beads.md"


def _terminate_process_group(process: subprocess.Popen) -> None:
    try:
        group = os.getpgid(process.pid)
    except OSError:
        return
    for sig, wait in ((signal.SIGTERM, GATE_TERM_GRACE_SECONDS), (signal.SIGKILL, 5)):
        try:
            os.killpg(group, sig)
        except OSError:
            return
        try:
            process.wait(timeout=wait)
        except subprocess.TimeoutExpired:
            pass
        if sig == signal.SIGKILL:
            return


class Run:
    def __init__(
        self,
        repo: Path,
        config: dict,
        objective: str,
        max_iterations: int | None = None,
        run_id: str | None = None,
        issue: str = "",
    ):
        self.repo = repo
        self.config = config
        self.objective = objective
        # The Beads issue this run works on, if any: claimed at the start,
        # closed when the operator approves (web/service.py `approval`).
        self.issue = issue
        self.review_round = 0
        # Per-persona review coverage: each persona's most recent verdict.
        # A persona that never returned a verdict in any round is named in
        # the stop reason when the rounds run out, so awaiting approval
        # never hides an unreviewed part of the change.
        self.review_coverage = {}
        # Two numbers, not one.  The floor is what the operator asked for and is
        # always honoured; the ceiling is what no plan can argue past.  An
        # explicit `--max-iterations` raises the floor rather than pinning the
        # run, because the request behind it is "at least this much", and a run
        # that stops one step short of a finished plan is the thing being fixed.
        stop = config["stop"]
        legacy = stop.get("max_iterations", 20)
        self.iteration_floor = max_iterations or stop.get("initial_turns", legacy)
        self.iteration_ceiling = max(
            self.iteration_floor, stop.get("hard_turn_ceiling", legacy)
        )
        self.max_iterations = self.iteration_floor
        self.model = config["agent"]["model"]
        self.thinking = config["agent"].get("thinking", "")
        # Which agent does the typing, resolved once.  Read from the config in
        # four places before this existed, and one of them -- the `run:start`
        # event -- said "pi" whatever it was, so a run's own record could not be
        # trusted to name the thing that produced it.
        self.harness_name = config["agent"].get("harness", "pi")
        # Model policy is read all over `models.py` by functions that never see
        # a config, so the layered answer is handed over once, here.  Here and
        # not in `config.load`, which the WebUI calls once per project while
        # walking them all: a process-wide setting written by whichever config
        # was loaded last is exactly wrong there.  A `Run` is the one place a
        # single config really does govern the whole process.
        models.use(config["models"])
        self.role = "editing"
        self.window, self.max_output = models.declared_window(
            self.model, self.harness_name) or (0, 0)
        self.interrupted = False

        # An explicit run id is one somebody else already resolved -- `lmloop
        # resume` naming the run it means, or `--detach`'s parent naming the lane
        # it picked -- so it is taken literally.  A derived one is a new attempt,
        # and gets a free lane if today already used that name.
        derived = make_run_id(objective) if objective.strip() else ""
        self.run_id = run_id if run_id else self._free_run_id(derived)
        # Computed by comparison rather than remembered, so it is true however
        # the id arrived: a detached run is told its id by its parent, and would
        # otherwise never mention the run it stepped around.
        self.collided_with = derived if derived and self.run_id != derived else ""

        self.branch = self._branch_for(self.run_id)
        self.worktree = self._worktree_for(self.run_id)
        self.rundir = RunDir(self.worktree, self.run_id)

        self.gate_result = ""
        self.gate_output = ""
        self.gate_baseline = ""
        self.last_outcome = ""
        self.last_detail = ""
        self.last_commit: str | None = None
        # How many iterations each plan step has thrashed on, keyed by the step
        # text.  A step that has defeated the window twice is a step to split,
        # and the agent is the one who can split it.
        self.thrashed_steps: dict[str, int] = {}
        self.no_diff_streak = 0
        # Wall clock from earlier segments only.  This segment's own time is
        # measured from `_segment_started` and added when it is read, never
        # accumulated here -- adding both is how a 10h budget stopped a run
        # at six real hours while reporting 11.4h.
        self.elapsed_before = 0.0
        self._segment_started: float | None = None
        self._inactive_seconds = 0.0
        self.pending_iteration = 0
        self.linked: list[str] = []
        self.defects: list[str] = []
        self.screen = display.Screen()

    def restore_policy(self, state: dict) -> None:
        """Apply a run's resolved policy, leaving old state backward compatible."""
        if not isinstance(state, dict):
            return
        repo = state.get("repo_path")
        worktree = state.get("worktree_path")
        if isinstance(repo, str) and repo:
            self.repo = Path(repo)
        if isinstance(worktree, str) and worktree:
            self.worktree = Path(worktree)
            self.rundir = RunDir(self.worktree, self.run_id)
        branch = state.get("branch")
        if isinstance(branch, str) and branch:
            self.branch = branch
        agent = self.config.setdefault("agent", {})
        for key in ("model", "thinking", "tools", "required_tools"):
            if key in state and state[key] is not None:
                agent[key] = state[key]
        for section in ("gate", "stop", "iteration", "review"):
            saved = state.get(section)
            if isinstance(saved, dict):
                self.config.setdefault(section, {}).update(saved)
        self.model = agent.get("model", "")
        self.thinking = agent.get("thinking", "")
        self.harness_name = agent.get("harness", self.harness_name)
        models.use(self.config["models"])
        self.window, self.max_output = models.declared_window(
            self.model, self.harness_name) or (0, 0)

    def _branch_for(self, run_id: str) -> str:
        return self.config["worktree"]["branch"].format(repo=self.repo.name, run_id=run_id)

    def _worktree_for(self, run_id: str) -> Path:
        return Path(
            self.config["worktree"]["root"].format(repo=str(self.repo), run_id=run_id)
        )

    def _free_run_id(self, base: str) -> str:
        """``base``, or ``base-2``, ``base-3`` ... if those names are taken.

        The run id is date + slug + hash of the prompt, so the same objective on
        the same day derives the same id -- and that is exactly the day it
        happens, because the reason to re-run an objective is that the first
        attempt went nowhere and the prompt or the config has been fixed since.
        Failing there sent the operator to `git worktree remove` to get on with
        the thing they had just decided to do again.

        Nothing is discarded to make room: the earlier run keeps its worktree,
        its branch and its handoff chain, and the new attempt simply takes the
        next name.  The caller is told which run it stepped around, so that an
        operator who meant `lmloop resume` finds out before the second worktree
        is a surprise.
        """
        if not self._taken(base):
            return base
        for suffix in range(2, 100):
            candidate = f"{base}-{suffix}"
            if not self._taken(candidate):
                return candidate
        return base  # 99 attempts today; let prepare() say so plainly.

    def _taken(self, run_id: str) -> bool:
        """A name is taken if either half of it is: worktree or branch.

        Checking only the directory would hand back a name whose branch still
        exists from a worktree someone removed by hand, and `git worktree add`
        would then fail on the branch instead -- the same dead end, one step
        later and with a worse message.
        """
        return (
            self._worktree_for(run_id).exists()
            or gitops.branch_exists(self.repo, self._branch_for(run_id))
        )

    # -- setup ------------------------------------------------------------

    def _exclusions(self) -> list[str]:
        """What git must never see, whichever worktree it is standing in.

        `.lmloop.toml` is the operator's config at the repo root; the rest are
        run artifacts and the linked environment.  A symlinked `.venv` is still
        a file as far as `git add -A` is concerned, and committing it would put
        an absolute path to somebody's home directory in the history.
        """
        linkable = list(self.config["worktree"].get("link") or [])
        return [".worktrees/", ".lmloop/", ".lmloop.toml", ".pi/"] + linkable

    def prepare(self) -> None:
        if self.worktree.exists():
            # Only reachable when `_free_run_id` ran out of lanes, or when a run
            # id was named explicitly.  Either way the two real options are
            # continuing that run or clearing it out, so say both, with the
            # commands: this used to be a dead end that the operator had to
            # reverse-engineer from a path.
            raise SystemExit(
                f"lmloop: {self.worktree} already exists\n"
                f"  continue it:  lmloop resume {self.run_id}\n"
                f"  or clear it:  git worktree remove {self.worktree}"
                f" && git branch -D {self.branch}"
            )

        # `.lmloop.toml` is excluded too: it is the operator's config, it lives
        # at the repo root, and `git add -A` inside a worktree would otherwise
        # sweep it into the run's first commit.
        gitops.exclude(self.repo, self._exclusions())
        gitops.add_worktree(self.repo, self.worktree, self.branch)
        base = gitops.head_commit(self.worktree)
        self.rundir.create(self.objective, base)
        self.rundir.claim()
        try:
            self.publish_sessions()
            self.linked = self.link_environment()
            self.probe_env()
            self.probe_gate(base)
            self.probe_browser()
            if self.issue:
                self._bd("update", self.issue, "--claim")
            self.rundir.event(
                "run:start",
                runId=self.run_id,
                runDir=str(self.rundir.path),
                agent=self.harness_name,
                tools=self.config["agent"].get("tools", ""),
                model=self.model,
                worktree=True,
                repoPath=str(self.repo),
                worktreePath=str(self.worktree),
                branch=self.branch,
                baseCommit=base,
                maxIterations=self.max_iterations,
                promptLength=len(self.objective),
                issue=self.issue,
            )
        except BaseException:
            self.rundir.release()
            raise

    def attach(self, extra_iterations: int) -> int:
        """Re-enter an existing run instead of starting a new one.

        A run that dies -- a reboot, a closed ssh session, an OOM -- leaves its
        commits behind, but starting fresh would build a second worktree and
        abandon the handoff chain that makes the next iteration cheap.  This
        picks the run back up where it stopped: same worktree, same branch, same
        run directory, same handoff.
        """
        state = self.rundir.read_run_state()
        self.restore_policy(state)
        if not self.rundir.path.is_dir():
            raise SystemExit(f"lmloop: no run directory at {self.rundir.path}")
        holder = self.rundir.holder()
        if holder:
            # Two loops in one worktree commit over each other and write the
            # same status file.  A paused run still has a loop; resuming beside
            # it is the mistake this catches.
            raise SystemExit(
                f"lmloop: run {self.run_id} already has a loop (pid {holder})\n"
                f"  it may just be paused:  rm {self.rundir.pause_path}\n"
                f"  or stop it first:       touch {self.rundir.stop_path}"
            )
        self.rundir.claim()
        try:
            self.probe_env()
            done = runrecord.completed_iterations(self.rundir.read_events())
            self.objective = (self.rundir.path / "prompt.md").read_text().strip()
        except BaseException:
            self.rundir.release()
            raise
        try:
            state = self.rundir.read_run_state()
            self.issue = self.issue or str(state.get("issue", ""))
            self.review_round = int(state.get("review_round", 0))
            saved = state.get("review_coverage")
            self.review_coverage = (
                {str(k): v for k, v in saved.items() if isinstance(v, str)}
                if isinstance(saved, dict) else {})
            self.pending_iteration = int(state.get("pending_iteration", 0))
            if self.pending_iteration:
                # A provider outage writes the prompt before the request fails.  It
                # is evidence of an attempt, not proof the iteration completed.
                done = min(done, self.pending_iteration - 1)
            prior_ceiling = int(state.get("hard_turn_ceiling", done))
            self.iteration_ceiling = max(done, prior_ceiling) + extra_iterations
            self.iteration_floor = min(self.iteration_ceiling, done + extra_iterations)
            self.max_iterations = self.iteration_floor
            # Carried across resumes so a bare `resume` cannot launder the one guard
            # that never lies -- but capped one below the limit, because a run that
            # stopped ON this guard would otherwise reload a tripped streak and exit
            # before iteration 1, having run nothing.  A resume is the operator
            # saying "I changed something, try again"; it buys exactly one iteration
            # to prove it, and if that one also moves nothing the guard fires again.
            limit = self.config["stop"]["no_diff_iterations"]
            carried = int(state.get("no_diff_streak", 0))
            self.no_diff_streak = min(carried, max(limit - 1, 0)) if limit else carried
            self.elapsed_before = float(state.get("active_elapsed_seconds", 0))
            self.thrashed_steps = {
                str(step): int(count)
                for step, count in (state.get("thrashed_steps") or {}).items()
            }
        except BaseException:
            self.rundir.release()
            raise
        # Runs that predate this, and runs resumed after the exclude list grew.
        gitops.exclude(self.repo, self._exclusions())
        self.publish_sessions()
        self.linked = self.link_environment()
        self.rundir.event(
            "run:start",
            runId=self.run_id,
            runDir=str(self.rundir.path),
            agent=self.harness_name,
            tools=self.config["agent"].get("tools", ""),
            model=self.model,
            resumed=True,
            completedIterations=done,
            repoPath=str(self.repo),
            worktreePath=str(self.worktree),
            branch=self.branch,
            maxIterations=self.max_iterations,
        )
        return done

    # -- stop conditions --------------------------------------------------

    def _budget(self, iteration: int) -> int:
        """How many iterations this run may use, recomputed from the plan.

        A fixed count is the wrong shape for a plan whose length is not known
        when the run starts.  This run planned twelve steps, grew to thirteen,
        then to fifteen, and spent five of its fourteen iterations on transport
        failures and a server that was switched off -- so it stopped at 10/15
        having never once been short of *work*, only of budget.

        One iteration per step plus `retry_allowance` spare, so a step that
        needs a second attempt does not cost the run its last step.

        Two things keep this from being a number the agent writes for itself.
        It never shrinks below the explicit figure the operator asked for, and
        it is capped by `max_iterations`, which no amount of plan growth can
        argue past.  Between them, growing the plan buys attempts up to a
        ceiling, and never buys an unbounded run.
        """
        if not self.config["stop"].get("budget_follows_plan", False):
            return self.iteration_floor
        done, total = self.rundir.plan_progress()
        return policy.budget(
            iteration, done, total,
            iteration_floor=self.iteration_floor,
            iteration_ceiling=self.iteration_ceiling,
            retry_allowance=self.config["stop"].get("retry_allowance", 5),
        )

    def _abort_reason(self, iteration: int, started: float) -> str | None:
        """Why this run should stop now, or None -- see `policy.abort_reason`."""
        done, total = self.rundir.plan_progress()
        return policy.abort_reason(
            iteration, started + self._inactive_seconds,
            interrupted=self.interrupted,
            stop_now_requested=self.rundir.stop_now_requested(),
            stop_requested=self.rundir.stop_requested(),
            plan_done=done,
            plan_total=total,
            max_iterations=self.max_iterations,
            iteration_ceiling=self.iteration_ceiling,
            elapsed_before=self.elapsed_before,
            max_wall_hours=self.config["stop"]["max_wall_hours"],
            no_diff_streak=self.no_diff_streak,
            no_diff_iterations_limit=self.config["stop"]["no_diff_iterations"],
            plan_at_start=getattr(self, "_plan_at_start", None),
        )

    # -- the environment the worktree does not inherit ---------------------

    def link_environment(self) -> list[str]:
        """Symlink the repo's untracked environment into the worktree.

        See `config.DEFAULTS["worktree"]["link"]` for why this exists and why it
        links rather than copies.  Returns the names actually linked, which the
        prompt then names for the agent -- knowing the interpreter is there is
        worth as much as the interpreter being there, since an agent that cannot
        find one goes looking instead of working.
        """
        linked: list[str] = []
        for name in self.config["worktree"].get("link") or []:
            source = self.repo / name
            target = self.worktree / name
            if not source.exists():
                continue
            if target.is_symlink() or target.exists():
                # Already there, usually from an earlier iteration of this run.
                # Still report it: what the prompt and the header describe is
                # what the worktree HAS, not what this call happened to create.
                linked.append(name)
                continue
            try:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.symlink_to(source, target_is_directory=source.is_dir())
            except OSError as error:
                # Never fail a run over this: the agent may not need it, and a
                # missing link is visible in the log either way.
                self.rundir.event("worktree:link:failed", name=name, detail=str(error))
                continue
            linked.append(name)
        if linked:
            self.rundir.event("worktree:link", names=linked)
        return linked

    def interpreter(self) -> str:
        """The project's own Python, if one was linked in.  Repo-relative."""
        for name in self.config["worktree"].get("link") or []:
            candidate = self.worktree / name / "bin" / "python"
            if candidate.exists():
                return f"{name}/bin/python"
        return ""

    # -- discoverability --------------------------------------------------

    def publish_sessions(self) -> None:
        """Point pi-aware tools at this run's transcripts.

        lmloop keeps pi's session files inside the run directory so a run is one
        self-contained artifact.  The cost is that anything looking for pi
        sessions in the usual place finds nothing, and an lmloop run was
        therefore invisible in paseo's import picker -- not because of how the
        run was launched, but because paseo resolves one session directory per
        cwd and walks only that.  Symlinking into the default location does not
        help: its walker tests `isFile()`, which a symlink fails.

        `<cwd>/.pi/settings.json` is the hook that ecosystem already reads for
        exactly this.  One small file, transcripts stay where they are.

        The matching is on the *worktree* path, since that is the cwd pi records
        -- so the worktree is what has to be registered as a paseo workspace,
        not the repo above it.
        """
        settings = self.worktree / ".pi" / "settings.json"
        settings.parent.mkdir(parents=True, exist_ok=True)
        settings.write_text(
            json.dumps({"sessionDir": str(self.rundir.sessions)}, indent=2) + "\n"
        )

    # -- environment ------------------------------------------------------

    def env(self) -> dict:
        """The environment for anything that runs inside the worktree.

        An allowlist by default rather than a copy of the operator's shell --
        see `envpolicy` for what survives and why.  This is the environment for
        the agent *and* for the gate, deliberately: a gate is a shell string
        from config, run in the same worktree, and there is no reading of it
        under which it needs the credentials the agent is not trusted with.

        Nothing that merely *ran* during an iteration should end up in the
        commit.  Python writes bytecode next to the source by default, so both
        the gate and any `python` the agent invokes through its bash tool leave
        `__pycache__` behind, and `git add -A` sweeps it up.  Redirecting the
        cache into the run directory -- already excluded from git -- keeps the
        commit to what the agent actually wrote.
        """
        settings = self.config.get("env", {})
        adapter = harness.get(self.harness_name)
        overrides = {"PYTHONPYCACHEPREFIX": str(self.rundir.path / "pycache")}
        if self.harness_name == "pi":
            overrides["PI_CODING_AGENT_DIR"] = str(adapter.config_dir)
        return envpolicy.build(
            os.environ,
            inherit=settings.get("inherit", "allowlist"),
            harness_names=adapter.env_passthrough,
            allow=tuple(settings.get("pass", ())),
            block=tuple(settings.get("block", ())),
            overrides=overrides,
        )

    def probe_env(self) -> None:
        """Say once what the agent will *not* be able to see.

        A harness that needed a credential fails a long way from the cause -- an
        auth error mid-iteration, a provider that simply returns nothing -- and
        an operator has no reason to connect that to an allowlist they never
        set.  Two lines at the start turn a lost hour into a five-second fix.

        Names only, never values: this goes into the event log, and a run's own
        record gets read by people who were not at the machine it ran on.
        """
        settings = self.config.get("env", {})
        if settings.get("inherit", "allowlist") == "all":
            return
        missing = envpolicy.withheld(os.environ, self.env())
        if not missing:
            return
        # Capped: a shell carrying forty of these would otherwise push the run
        # header off the screen, and the count is the part that matters.
        self.rundir.event("env:withheld", names=missing[:40], count=len(missing))
        shown = ", ".join(missing[:3])
        more = f", and {len(missing) - 3} more" if len(missing) > 3 else ""
        self.screen.log(f"  env     withholding {len(missing)} credential-shaped"
                        f" variable(s): {shown}{more}")
        self.screen.log("          add names to [env] pass if the agent needs them")

    def probe_browser(self) -> None:
        """Say once, at the start, whether the agent's browser can attach.

        Only an agent that declares a `browser_tool` has one, and only when the
        allowlist lets it through, so this is silent for every other
        configuration.  It is never fatal: an
        iteration whose browser is unreachable still reads, edits, gates and
        commits.  What it saves is the shape of failure where the agent spends
        an hour discovering, one tool call at a time, that the tab it was asked
        to look at was never reachable -- and where the run log blames the
        model for it.
        """
        agent = self.config["agent"]
        tool = harness.get(self.harness_name).browser_tool
        if not tool:
            return
        # An empty allowlist is not "no tools" -- an agent with a browser reads
        # it as all of them, browser included -- so it is the one case where
        # finding no browser tool in the list still means the browser is on.
        enabled = [name.strip() for name in agent.get("tools", "").split(",") if name.strip()]
        if enabled and tool not in enabled:
            return
        cdp_url = config.reference(agent.get("browser_cdp_url", ""))
        if not cdp_url:
            # Not a failure, and saying it was one was wrong in the way that
            # matters: a pilot run logged `ok=false, "no browser CDP endpoint
            # configured"` and then opened a tab anyway, because with nothing
            # configured omp launches its own headless Chromium.  There is
            # nothing to preflight and nothing missing.
            self.rundir.event("browser:preflight", ok=True, detail="omp's own browser")
            self.screen.log("  browser: omp's own (no CDP endpoint configured)")
            return
        try:
            ok, detail = browser.preflight(cdp_url)
        except Exception as error:  # noqa: BLE001 - never fails a run
            # This runs inside `prepare()`, after the worktree and the run
            # directory exist and before `run:start` is emitted.  Anything that
            # escapes here loses the run's own record of itself over a typo in
            # an optional setting.
            ok, detail = False, f"preflight failed: {type(error).__name__}"
        # `detail` is redacted at the source; see browser.redact.
        self.rundir.event("browser:preflight", ok=ok, detail=detail)
        self.screen.log(f"  browser: {detail}" if ok else
                        f"  browser: {detail} (omp will fall back to its own)")

    # -- the gate ---------------------------------------------------------

    def probe_gate(self, base: str) -> None:
        """Run the gate once, on the untouched worktree, before any iteration.

        A gate whose command cannot be found fails identically every iteration,
        and reads as a broken project rather than a broken path -- the observed
        run recorded `fail (rc=127)` twelve times for a `.venv` that was simply
        not in the worktree, and nothing surfaced it but the event log.  One run
        against the base commit separates the two questions for good, and it is
        cheap next to the hour that follows it.

        It also answers a second question worth having: whether the gate was
        already failing before the agent touched anything.  That failure belongs
        to the repository, not to the run, and an iteration that inherits it
        should not read as the iteration that caused it.

        Only an unrunnable gate stops the run.  A gate that runs and fails is a
        fact about the repository, and refusing to start on it would make lmloop
        useless for the case it is most wanted in: a project that is broken.
        """
        command = self.config["gate"]["command"]
        if not command:
            return
        self.run_gate(0)
        self.gate_baseline = self.gate_result
        self.rundir.event(
            "gate:probe", command=command, result=self.gate_result, baseCommit=base
        )
        if self.gate_result.startswith("misconfigured") or self.gate_result == "fail (could not run)":
            raise SystemExit(
                f"lmloop: the gate cannot be run, so every iteration would record the"
                f" same failure\n"
                f"  gate:  {command}\n"
                f"  cwd:   {self.worktree}\n"
                f"  said:  {self.gate_output.strip()[-300:] or self.gate_result}\n"
                f"\n"
                f"  The gate runs inside the worktree, not the repo root, so a path"
                f" that only exists\n"
                f"  in the main checkout has to be absolute or listed under"
                f" [worktree] link.\n"
                f"\n"
                f"  This worktree holds no work -- no iteration ran -- so fixing the"
                f" gate and\n"
                f"  re-running is enough; the next attempt takes its own name. To"
                f" clear this one:\n"
                f"    git worktree remove {self.worktree} && git branch -D {self.branch}"
            )
        if self.gate_result.startswith("fail"):
            self.screen.log(
                f"  gate already fails on the base commit ({self.gate_result});"
                " that failure is the repository's, not this run's"
            )

    def run_gate(self, number: int) -> None:
        command = self.config["gate"]["command"]
        if not command:
            self.gate_result, self.gate_output = "", ""
            return
        try:
            process = subprocess.Popen(
                command,
                shell=True,
                cwd=str(self.worktree),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
                env=self.env(),
            )
            try:
                stdout, stderr = process.communicate(timeout=600)
            except subprocess.TimeoutExpired as error:
                _terminate_process_group(process)
                stdout = error.output or ""
                stderr = error.stderr or ""
                try:
                    stdout, stderr = process.communicate()
                except (OSError, ValueError):
                    pass
                stdout = stdout.decode(errors="replace") if isinstance(stdout, bytes) else stdout
                stderr = stderr.decode(errors="replace") if isinstance(stderr, bytes) else stderr
                output, self.gate_result = (
                    (stdout + stderr).strip(), "fail (timeout)"
                )
            else:
                output = (stdout + stderr).strip()
                if process.returncode == 0:
                    self.gate_result = "pass"
                elif process.returncode == 127:
                    # 127 is the shell saying it could not find the command, which is
                    # a broken gate, not broken code.  Kept out of the "fail" family
                    # deliberately: `blocks_commit` keys off that prefix, and a gate
                    # that cannot run must never be the reason an hour of work sits
                    # uncommitted.
                    self.gate_result = "misconfigured (rc=127: command not found)"
                else:
                    self.gate_result = f"fail (rc={process.returncode})"
        except OSError as error:
            output, self.gate_result = str(error), "fail (could not run)"
        self.gate_output = output
        self.rundir.gate_log(number).write_text(f"$ {command}\n\n{output}\n")

    # -- one iteration ----------------------------------------------------

    def roles(self) -> tuple[str, str, str]:
        """Which model runs this iteration, and why.

        The first iteration of a run has no plan, so its job is to read the
        repository and decide the steps -- a whole-repository question that
        happens once and wants the widest window available.  Every iteration
        after it carries out one step, which is a two-file question that happens
        constantly and wants throughput.  Those are different models on local
        hardware, and `planner_model` is how a project says so.
        """
        agent = self.config["agent"]
        planning = not self.rundir.read_plan().strip()
        if planning and agent.get("planner_model"):
            return (
                agent["planner_model"],
                agent.get("planner_thinking") or agent.get("thinking", ""),
                "planning",
            )
        wider = self._wider_model() if self.last_outcome == "thrashing" else ""
        if wider:
            return (
                wider,
                agent.get("planner_thinking") or agent.get("thinking", ""),
                "retry",
            )
        return agent["model"], agent.get("thinking", ""), "editing"

    def _retry_step(self) -> str:
        """The step in play, if it is one that has already thrashed.

        Checked against the plan as it stands now rather than remembered: the
        agent may have split the step itself, or checked it off, and in either
        case the warning has done its job and should stop being shown.
        """
        step = self.rundir.current_step()
        return step if step and step in self.thrashed_steps else ""

    def _wider_model(self) -> str:
        """The configured model with the most room, when the last one ran out.

        Thrashing is the window losing to the codebase: the agent reads until it
        overflows, compacts, distrusts the summary and reads again.  Retrying the
        same step on the same model is retrying the thing that just failed for a
        reason that has not changed, so the retry goes to whichever model this
        project already names that measures widest -- on one repository that is
        90112 tokens of prompt budget against 49152, which is the difference
        between a file fitting and not.

        Only models the project configured are considered.  Picking a model the
        operator never named would be lmloop deciding what their hardware should
        load, and a wider window it has to swap in for is not obviously a better
        trade than a narrower one already resident.
        """
        agent = self.config["agent"]
        current = agent["model"]
        candidates = {name for name in (current, agent.get("planner_model")) if name}
        best, best_room = "", models.declared_window(current, self.harness_name)
        if best_room is None:
            return ""  # unmeasured: no basis to call anything wider
        for name in candidates:
            room = models.declared_window(name, self.harness_name)
            if room and room[0] > best_room[0]:
                best, best_room = name, room
        return best

    def iterate(self, number: int) -> None:
        base = self.rundir.base_commit
        self.model, thinking, role = self.roles()
        # Kept on the run so the status file can say which model is working and
        # under what settings.  `roles` can hand back a different model than the
        # one configured -- planning uses one, a thrash retry another -- and a
        # dashboard that shows the configured model is showing the wrong one.
        self.thinking, self.role = thinking, role
        self.window, self.max_output = models.declared_window(
            self.model, self.harness_name) or (0, 0)
        ok, detail = models.preflight(
            self.model, config.reference(self.config["models"]["llama_swap_url"]))
        self.rundir.event("preflight", iteration=number, ok=ok, detail=detail)
        if not ok:
            raise PreflightError(detail)

        prompt = prompts.build(
            objective=self.objective,
            number=number,
            max_iterations=self.max_iterations,
            branch=self.branch,
            base=base,
            log=gitops.log_oneline(self.worktree, base),
            diff=gitops.diff_stat(self.worktree, base),
            handoff=self.rundir.read_handoff(),
            handoff_path=str(self.rundir.handoff_path),
            tree=gitops.tracked_files(self.worktree),
            context=gitops.context_files(
                self.worktree,
                self.config.get("context", {}).get("files", []),
                self.config.get("context", {}).get("max_chars", 0),
            ),
            history=previous_runs(self.worktree.parent, self.run_id),
            plan=self.rundir.read_plan(),
            plan_path=str(self.rundir.plan_path),
            plan_progress=self.rundir.plan_progress(),
            linked=getattr(self, "linked", []),
            interpreter=self.interpreter(),
            gate_command=self.config["gate"]["command"],
            gate_result=self.gate_result,
            gate_output=self.gate_output,
            gate_baseline=self.gate_baseline,
            defects=self.defects,
            thrashed_step=self._retry_step(),
            thrashed_times=self.thrashed_steps.get(self._retry_step(), 0),
            planning=self.config.get("planning", {}),
        )
        self.rundir.iteration_prompt(number).write_text(prompt)
        self.rundir.event(
            "iteration:start",
            iteration=number,
            promptLength=len(prompt),
            preflight=detail,
            model=self.model,
            role=role,
            git={"head": gitops.head_commit(self.worktree), "commitCount": gitops.commit_count(self.worktree, base)},
        )
        self.screen.log(f"  iteration {number}: {detail}")
        if role == "planning":
            self.screen.log(f"    planning with {self.model}")

        self._loading = True
        handoff_before = self.rundir.handoff_mtime()
        self._plan_before = self.rundir.plan_progress()
        self._plan_steps_before = self._plan_steps()
        self._step_before = self.rundir.current_step()
        result = pi_runner.run(
            model=self.model,
            agent_name=self.harness_name,
            tools=self.config["agent"]["tools"],
            thinking=thinking,
            prompt=prompt,
            cwd=self.worktree,
            session_dir=self.rundir.sessions,
            session_id=f"iter-{number}",
            raw_path=self.rundir.iteration_jsonl(number),
            timeout_seconds=self.config["iteration"]["timeout_seconds"],
            stall_seconds=self.config["iteration"]["stall_seconds"],
            tool_seconds=self.config["iteration"].get("tool_seconds", 0),
            max_compactions=self.config["iteration"]["max_compactions"],
            max_repeats=self.config["iteration"].get("max_repeats", 3),
            env=self.env(),
            system_prompt=self.system_prompt(),
            # Only the *hard* stop reaches in here.  A plain STOP means "end the
            # run", and the boundary -- where the gate runs, the handoff is
            # written and the tree is committed -- is where ending it is worth
            # something; killing pi at minute 55 to save five minutes throws
            # away the handoff that made the hour reusable.  STOP-NOW, and a
            # SIGINT, say the iteration itself is the thing to end.
            should_stop=lambda: self.interrupted or self.rundir.stop_now_requested(),
            on_progress=lambda snap: self._show(number, snap),
        )

        self._classify_provider_loss(result)

        # Which step defeated the window, and how often.  Recorded from the step
        # that was in play when the iteration started, not the one in play now:
        # a thrashing iteration writes nothing, so the plan has not moved, but
        # reading it after the fact would still be reading a file the agent was
        # free to edit mid-iteration.
        self.last_outcome = result.outcome
        if result.outcome == "thrashing" and self._step_before:
            self.thrashed_steps[self._step_before] = (
                self.thrashed_steps.get(self._step_before, 0) + 1
            )
            self.rundir.event(
                "step:thrashed",
                iteration=number,
                step=self._step_before[:160],
                times=self.thrashed_steps[self._step_before],
            )

        self.run_gate(number)
        # Structural checks run whatever the project configured, because the
        # damage an edit does is not project-specific -- see checks.py.
        # The plan is checked too, separately: it lives under `.lmloop/`, which
        # git excludes, so `checks.run` -- which works from the git diff -- can
        # never see the file that decides what every future iteration does.
        self.defects = checks.run(self.worktree, base) + self.rundir.plan_problems()
        if self.defects:
            self.rundir.event("checks:failed", iteration=number, problems=self.defects[:20])

        handoff_written = self.rundir.handoff_mtime() > handoff_before
        if not handoff_written:
            # An iteration that overflowed its context did write a handoff -- into
            # pi's event stream rather than to disk.  Prefer it over a git diff
            # that, for the iterations this happens to, is empty.
            self.rundir.write_synthetic_handoff(
                number,
                gitops.diff_shortstat(self.worktree, base),
                carried=self.rundir.last_compaction_summary(number, self.harness_name),
            )
        summary = self._subject(number, result, handoff_written)

        commit = self.commit(number, summary, result, handoff_written, base)
        self.record(number, summary, result, handoff_written, commit, base)
        self.last_detail = result.detail or ""
        self.last_commit = commit

    def _subject(self, number: int, result, handoff_written: bool) -> str:
        """One line describing what this iteration did, for the commit subject.

        Line 1 of the handoff is the right answer when the agent wrote one.  It
        is the wrong answer when it did not: the loop then synthesises a handoff
        whose first line says "iteration N ended without writing a handoff", and
        that became the subject of four of nine commits on one repository --
        iterations that wrote real, working test files and described none of it
        in `git log`.  Overflowing the context is exactly when the agent fails to
        write a handoff, so the commits most in need of a subject were the ones
        guaranteed not to get one.

        So a synthesised handoff is not trusted for the subject.  Fall back, in
        order, to the plan step that got checked off this iteration, then to the
        files that changed, then to the outcome.  All three are observations, not
        self-reports.
        """
        if handoff_written:
            first = self.rundir.read_handoff().splitlines()
            if first and first[0].strip():
                return first[0].strip()

        step = self._completed_step()
        if step:
            return step

        changed = self._relative_files(result.files_touched)
        if changed:
            listed = ", ".join(changed[:3])
            more = f" (+{len(changed) - 3} more)" if len(changed) > 3 else ""
            return f"{listed}{more}"

        return f"iteration {number} ({result.outcome})"

    def _completed_step(self) -> str:
        """The plan step checked off during this iteration, if exactly one was.

        Ambiguity is not worth guessing at: if the agent checked off two steps,
        neither is "the" subject, and the file list describes the commit better.
        """
        before = getattr(self, "_plan_steps_before", set())
        after = self._plan_steps()
        gained = [text for text in after - before]
        return gained[0][:68] if len(gained) == 1 else ""

    def _plan_steps(self) -> set[str]:
        """The text of every checked-off plan step, right now."""
        done = set()
        for line in self.rundir.read_plan().splitlines():
            stripped = line.strip()
            if stripped.startswith(("- [", "* [")) and stripped[3:4].lower() == "x":
                done.add(stripped[5:].strip().strip("`"))
        return done

    def _show(self, number: int, snap: dict) -> None:
        """One line that keeps moving, so an attached terminal never looks dead."""
        # A model load is a one-off event with a duration, not a state worth
        # repeating every five seconds.  Report it once, when it ends.
        if not snap["loading"] and self._loading:
            self._loading = False
            self.screen.log(f"    model ready in {display.elapsed(snap['elapsed'])}")

        paint = self.screen.paint
        # The spinner advances only while pi is emitting.  A model that has gone
        # quiet freezes it, so "moving" means the agent is alive rather than
        # merely that the loop redrew.
        alive = snap["loading"] or snap["quiet"] <= 30
        spinner = self.screen.spin(advance=alive)

        if snap["loading"]:
            detail = paint.yellow("loading model")
        elif snap["quiet"] > 60:
            detail = paint.yellow(f"quiet {display.elapsed(snap['quiet'])}")
        elif snap["last_tool"]:
            target = snap.get("last_target", "")
            detail = paint.cyan(snap["last_tool"] + (f" {target}" if target else ""))
        else:
            detail = paint.dim("thinking")

        plan_done, plan_total = self.rundir.plan_progress()
        estimate = eta.estimate(
            self.rundir.read_events(), elapsed_seconds=snap["elapsed"],
            iteration=number, max_iterations=self.max_iterations,
            plan_done=plan_done, plan_total=plan_total,
        )
        flags = "".join(
            flag
            for flag, on in (("PAUSE", self.rundir.paused()), ("STOP", self.rundir.stop_requested()))
            if on
        )
        # List order is layout; the number is what survives a narrow terminal.
        # On a phone the useful answer is "which iteration, doing what, and is it
        # stopping" -- counters and even elapsed time go before those do.
        self.screen.status([
            # The spinner rides with the iteration counter rather than as its own
            # segment: it must never be the thing `compose` drops, because on the
            # narrowest terminal it is the only proof the run is alive.
            (6, f"{paint.cyan(spinner)} {paint.bold(f'{number}/{self.max_iterations}')}"),
            (4, display.elapsed(snap["elapsed"])),
            (5, detail),
            # Only shown once it has happened, and then it outranks the counters:
            # a climbing overflow count against zero writes is the difference
            # between an iteration that is working and one that is going in
            # circles, and it is not visible anywhere else while the run is live.
            (3, paint.red(f"{snap['compactions']} overflow") if snap["compactions"] else ""),
            # What a long run actually looks like from outside: not tool calls,
            # but how much of the plan is behind it.
            (4, paint.green(f"{plan_done}/{plan_total} steps") if plan_total else ""),
            (2, paint.dim(f"{snap['tool_calls']} tools")),
            (2, paint.dim(f"{snap['tokens_per_second']:.1f} tok/s") if snap["tokens_per_second"] else ""),
            (1, paint.dim(f"{snap['output_tokens']} out")),
            (7, paint.red(f"[{flags}]") if flags else ""),
        ])
        self.rundir.write_status({
            "run_id": self.run_id,
            "iteration": number,
            "max_iterations": self.max_iterations,
            "model": self.model,
            "thinking": self.thinking,
            "role": self.role,
            # Zero for a model nobody has measured; the display says "unmeasured"
            # rather than inventing a denominator.
            "context_window": self.window,
            "max_output_tokens": self.max_output,
            "phase": "loading" if snap["loading"] else "working",
            "elapsed_seconds": round(snap["elapsed"]),
            "last_tool": snap["last_tool"],
            "last_target": snap.get("last_target", ""),
            "tool_calls": snap["tool_calls"],
            "writes": snap["writes"],
            "compactions": snap["compactions"],
            "plan_done": plan_done,
            "plan_total": plan_total,
            "output_tokens": snap["output_tokens"],
            # The largest single reply so far. This is the number the cap
            # applies to; `output_tokens` is the iteration's running total.
            "peak_output": snap.get("peak_output"),
            "truncations": snap.get("truncations"),
            "input_tokens": snap["input_tokens"],
            "tokens_per_second": round(snap["tokens_per_second"], 2),
            "quiet_seconds": round(snap["quiet"]),
            "paused": self.rundir.paused(),
            "stopping": self.rundir.stop_requested(),
            **estimate,
        })

    # -- commit -----------------------------------------------------------

    def commit(self, number: int, summary: str, result, handoff_written: bool, base: str) -> str | None:
        # Line 1 of the handoff is written by a model and is not always one
        # line's worth of text, so the subject is trimmed to something `git log
        # --oneline` can show.  The full text survives in notes.md.
        subject = " ".join(summary.split())
        if len(subject) > 68:
            subject = subject[:65].rstrip(" ,.;:") + "..."

        lines = [
            f"lmloop {self.run_id} iter {number}: {subject}",
            "",
            f"outcome: {result.outcome}" + (f" ({result.detail})" if result.detail else ""),
            f"model:   {self.model}",
        ]
        if self.config["gate"]["command"]:
            lines.append(f"gate:    {self.config['gate']['command']} -> {self.gate_result}")
        if not handoff_written:
            lines.append("handoff: missing (synthesised from git)")
        shortstat = gitops.diff_shortstat(self.worktree, base).strip()
        if shortstat:
            lines.append(f"files:  {shortstat}")

        blocked = self.config["gate"]["blocks_commit"] and self.gate_result.startswith("fail")
        if blocked:
            self.rundir.event("git:commit:blocked", iteration=number, gate=self.gate_result)
            self.screen.log(f"    gate failed and blocks_commit is set; leaving {number} uncommitted")
            return None

        sha = gitops.commit_all(self.worktree, "\n".join(lines) + "\n")
        if sha:
            self.rundir.event("git:commit", iteration=number, sha=sha, outcome=result.outcome)
        return sha

    # -- bookkeeping ------------------------------------------------------

    def _relative_files(self, paths: list[str]) -> list[str]:
        """Repo-relative source paths only.

        The agent's write to handoff.md is a tool call like any other, so it
        turns up in the event stream; it is bookkeeping, not a code change, and
        listing it under "Changes" makes every iteration look like it touched an
        extra file.
        """
        relative = []
        for path in paths:
            candidate = Path(path)
            try:
                candidate = candidate.relative_to(self.worktree)
            except ValueError:
                pass
            if str(candidate).startswith(".lmloop/"):
                continue
            if str(candidate) not in relative:
                relative.append(str(candidate))
        return relative[:20]

    def record(self, number: int, summary: str, result, handoff_written: bool, commit: str | None, base: str) -> None:
        prefix = "" if result.outcome == "ok" else f"[{result.outcome.upper()}] "
        changes = self._relative_files(result.files_touched)
        learnings = []
        if self.gate_result:
            learnings.append(f"gate `{self.config['gate']['command']}` -> {self.gate_result}")
        done, total = self.rundir.plan_progress()
        if total:
            gained = done - getattr(self, '_plan_before', (0, 0))[0]
            learnings.append(
                f"plan: {done}/{total} steps done"
                + (f" (+{gained} this iteration)" if gained > 0 else " (no step completed)")
            )
        elif self.rundir.read_plan():
            learnings.append("plan exists but has no checkboxes")
        else:
            learnings.append("no plan written; the objective was never broken down")
        for defect in self.defects[:6]:
            learnings.append(f"structural check: {defect}")
        pressure = policy.context_warning(result.input_tokens, self.window)
        if pressure:
            learnings.append(pressure)
        # Separate from `pressure` above and separate from the `truncated`
        # outcome: that outcome fires only when the *last* reply was cut off
        # and nothing was written, so an iteration that was cut off, recovered
        # and committed reports `ok` with no sign this happened at all.
        reply_pressure = policy.reply_warning(
            result.truncations, result.peak_output, self.max_output)
        if reply_pressure:
            learnings.append(reply_pressure)
        if result.compactions:
            learnings.append(
                f"context overflowed {result.compactions}x; every overflow costs the"
                " agent everything it had read"
            )
        if not handoff_written and result.compactions:
            learnings.append("agent wrote no handoff; the loop carried its last context summary forward")
        elif not handoff_written:
            learnings.append("agent wrote no handoff; the loop synthesised one from git")
        if result.detail and result.outcome != "ok":
            learnings.append(result.detail)
        self.rundir.append_notes(number, prefix + summary, changes, learnings)

        uncommitted = gitops.has_uncommitted(self.worktree)
        if self._counts_as_no_progress(commit, uncommitted):
            self.no_diff_streak += 1
        elif commit is not None or uncommitted:
            # Any git-visible change clears it, whatever killed the iteration.
            self.no_diff_streak = 0

        if pressure:
            self.screen.log(f"    {pressure}")
            self.rundir.event(
                "context:pressure", iteration=number,
                inputTokens=result.input_tokens, window=self.window,
            )
        self._save_run_state()
        self.rundir.event(
            "iteration:end",
            iteration=number,
            elapsedMs=int(result.elapsed_seconds * 1000),
            success=result.outcome == "ok",
            outcome=result.outcome,
            summary=summary,
            toolCalls=result.tool_calls,
            writes=result.writes,
            compactions=result.compactions,
            handoffWritten=handoff_written,
            planDone=done,
            planTotal=total,
            commit=commit,
            gate=self.gate_result,
            totalInputTokens=result.input_tokens,
            totalOutputTokens=result.output_tokens,
            # The one comparable to the cap, and the count that says whether it
            # was reached.  `totalOutputTokens` is the iteration's sum and the
            # cap applies per reply, so the two never belonged side by side.
            peakOutputTokens=result.peak_output,
            truncations=result.truncations,
            commitCount=gitops.commit_count(self.worktree, base),
        )
        # The tool-call count is context, not evidence.  An agent can edit a
        # file perfectly well through its bash tool -- one interrupted run here
        # showed "0 writes" against 8 committed insertions -- so what gets
        # reported is what git saw, not what the event stream counted.
        if commit:
            changed = gitops.commit_shortstat(self.worktree, commit) or "no file changes"
            verdict = f"committed {commit[:8]}: {changed}"
        else:
            verdict = "nothing to commit"
        # The overflow count earns its place on this line whenever it is not
        # zero: it is the difference between an iteration that ran out of time
        # and one that ran out of room, and those want opposite responses.
        paint = self.screen.paint
        overflows = paint.red(f" | {result.compactions} overflows") if result.compactions else ""
        if self.defects:
            self.screen.log(paint.red(f"    {len(self.defects)} structural problem(s) in changed files:"))
            for defect in self.defects[:4]:
                self.screen.log(f"      {defect}")
        outcome = (paint.green if result.outcome == "ok" else paint.yellow)(result.outcome)
        self.screen.log(
            f"    {outcome} in {display.elapsed(result.elapsed_seconds)}"
            f" | {result.tool_calls} tool calls{overflows} | {verdict}"
        )

    def _counts_as_no_progress(self, commit: str | None, uncommitted: bool) -> bool:
        """Whether this outcome should advance the no-diff streak -- see
        `policy.counts_as_no_progress` and `policy.NO_PROGRESS_OUTCOMES`."""
        return policy.counts_as_no_progress(self.last_outcome, commit, uncommitted)

    def _save_run_state(self) -> None:
        elapsed = self.elapsed_before
        if self._segment_started is not None:
            elapsed += time.monotonic() - self._segment_started - self._inactive_seconds
        self.rundir.write_run_state({
            "active_elapsed_seconds": elapsed,
            "no_diff_streak": self.no_diff_streak,
            "thrashed_steps": self.thrashed_steps,
            "hard_turn_ceiling": self.iteration_ceiling,
            "pending_iteration": self.pending_iteration,
            "issue": self.issue,
            "review_round": self.review_round,
            "review_coverage": self.review_coverage,
            "review": self.config["review"],
            # Policy state that needs to be restored on resume for semantic continuity
            "repo_path": str(self.repo),
            "worktree_path": str(self.worktree),
            "branch": self.branch,
            "model": self.model,
            "thinking": self.thinking,
            "harness": self.harness_name,
            "tools": self.config["agent"].get("tools", ""),
            "required_tools": self.config["agent"].get("required_tools", []),
            "gate": self.config["gate"].copy() if self.config.get("gate") else {},
            "stop": self.config["stop"].copy() if self.config.get("stop") else {},
            "iteration": self.config["iteration"].copy() if self.config.get("iteration") else {},
        })

    # -- driver -----------------------------------------------------------

    def start(self, from_iteration: int = 0) -> int:
        started = self._segment_started = time.monotonic()
        signal.signal(signal.SIGINT, self._on_interrupt)
        signal.signal(signal.SIGTERM, self._on_interrupt)

        keys = display.Keys(self.rundir, self.screen)
        keys.start()
        if self.screen.tty:
            self.screen.log(f"  {display.Keys.HELP}")

        iteration = from_iteration
        reason = None
        self._plan_at_start = self.rundir.plan_progress()[0]
        try:
            while True:
                iteration += 1
                display.wait_while_paused(
                    self.rundir, self.screen,
                    lambda: self.interrupted or self.rundir.stop_requested())
                # Recomputed here rather than fixed at the start: the plan is the
                # agent's, it changes while the run is going, and the budget is
                # supposed to follow it.  Everything downstream -- the status line,
                # status.json, the prompt -- reads `max_iterations`, so setting it
                # here is enough to make all of them agree.
                self.max_iterations = self._budget(iteration)
                reason = self._abort_reason(iteration, started)
                if reason and reason.startswith("plan complete"):
                    try:
                        verdict = self._review_gate()
                    except PreflightError as error:
                        if not self._backoff(iteration, str(error)):
                            reason = f"review could not reach the model: {error}"
                            break
                        iteration -= 1
                        continue
                    if verdict == "changes":
                        # Findings are plan items now; the plan is no longer
                        # complete, and the next pass works them.  A review is
                        # not an iteration, so the number is not spent.
                        iteration -= 1
                        continue
                    reason += f"; awaiting approval{verdict}"
                if reason:
                    break
                try:
                    self.iterate(iteration)
                except PreflightError as error:
                    if not self._backoff(iteration, str(error)):
                        reason = f"preflight failed: {error}"
                        break
                    iteration -= 1
                    continue
                if self.last_outcome == "provider-unavailable":
                    self.pending_iteration = iteration if not self.last_commit else 0
                    self._save_run_state()
                    if not self._wait_for_server(
                        iteration, self.last_detail or "local model provider unavailable",
                        retry_same=not bool(self.last_commit),
                    ):
                        reason = "local model provider unavailable; paused run was stopped"
                        break
                    # Useful partial work was committed and should not be
                    # repeated.  A barren provider failure retries this number.
                    if not self.last_commit:
                        iteration -= 1
                    continue
                if self.pending_iteration:
                    self.pending_iteration = 0
                    self._save_run_state()
                self._drift_check(iteration)
                transport = self._transport_failure()
                if transport:
                    # The server, not the work.  Same backoff as a failed preflight,
                    # and the iteration number is reused, so a restart of llama-swap
                    # does not quietly cost one of twelve iterations.
                    if not self._backoff(iteration, transport):
                        reason = f"model server unreachable: {transport}"
                        break
                    iteration -= 1
        except BaseException as error:  # noqa: BLE001 - re-raised below
            # An iteration can fail in ways this loop never enumerated -- a bug
            # here, a harness that raises something new, a second Ctrl-C landing
            # mid-iterate.  Whatever it was, the run still owes the disk its
            # evidence and the operator their terminal back, so the reason is
            # recorded and `finally` finalises exactly as a clean stop would.
            # The exception is re-raised, never swallowed: the caller's exit
            # code and traceback are the operator's copy of the same news.
            reason = policy.failure_reason(error)
            raise
        finally:
            self._finalise(reason, iteration, started)
        return 0

    def _finalise(self, reason: str | None, iteration: int, started: float) -> None:
        """Everything the run owes the disk and the operator on the way out.

        Called from a `finally`, so it runs whether the loop stopped on its own
        terms, raised, or took a second Ctrl-C.  Only the first of those used to
        finalise at all: an unexpected exception anywhere in `iterate` skipped
        the `run:complete` event, the terminal status, the run-state save, the
        claim release, the screen cleanup and the notification in one go,
        leaving a finished run reading as "working" for as long as anyone cared
        to look and its `loop.pid` on disk.

        Every step is guarded on its own, because the steps a later reader needs
        most are not the ones most likely to survive.  `self.rundir.base_commit`
        is one missing file away from raising and it sits inside the *first*
        step, which before this was enough on its own to cost the release and
        the terminal status.  A step that fails is evidence in its own right --
        see `_safely` -- rather than a reason to skip the rest.

        The order is the one a clean run always used, and nothing here touches
        the worktree: finalising a crashed run must never be the thing that
        discards what it managed to produce.
        """
        stopped = reason or "complete"
        progress: list[int] = [0, 0]

        self._safely("run:complete event", lambda: self.rundir.event(
            "run:complete",
            status=stopped,
            iterations=iteration - 1,
            commitCount=gitops.commit_count(self.worktree, self.rundir.base_commit),
            worktreePath=str(self.worktree),
        ))
        self._safely("plan progress", lambda: progress.__setitem__(
            slice(0, 2), self.rundir.plan_progress()))
        self._safely("terminal status", lambda: self.rundir.write_terminal_status(
            stopped, iteration - 1, progress[0], progress[1]))
        self._safely("run-state save", self._save_run_state)
        self._safely("claim release", self.rundir.release)
        self._safely("screen cleanup", self.screen.close)
        self._safely("summary", lambda: self._summarise(reason, started))
        self._safely("sweep", self._sweep)
        self._safely("notification", lambda: self._announce(reason, started, iteration - 1))
        self._safely("web push", lambda: self._announce_push(reason, started, iteration - 1))

    def system_prompt(self) -> str:
        """Path to this iteration's appended system prompt, or "" for none.

        Three parts, each optional: the operator's own instructions from
        `SYSTEM_PROMPT` (editable from the dashboard), the bundled Beads skill
        when the repository tracks work with `bd`, and the issue this run is
        for.  Rebuilt every iteration so an edit reaches a run already going.
        Written into the run directory, which git never sees.
        """
        parts = []
        try:
            parts.append(config.SYSTEM_PROMPT.read_text().strip())
        except OSError:
            pass
        if (self.repo / ".beads").is_dir():
            parts.append(BEADS_SKILL.read_text().strip())
        if self.issue:
            parts.append(
                f"This run is working on Beads issue `{self.issue}`. "
                f"Run `bd show {self.issue}` once for its acceptance criteria."
            )
        parts = [p for p in parts if p]
        if not parts:
            return ""
        path = self.rundir.path / "system-prompt.md"
        path.write_text("\n\n".join(parts) + "\n")
        return str(path)

    def _bd(self, *args: str) -> None:
        # Best effort: a missing or failing `bd` must never cost the run.
        try:
            subprocess.run(["bd", *args], cwd=str(self.repo),
                           capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.SubprocessError):
            pass

    # ── Review (docs/review.md) ─────────────────────────────────────────────

    def _review_gate(self) -> str:
        """Review a completed plan.  "changes" to keep working, else a suffix
        for the stop reason: "" when approved, " (review unresolved)" when the
        rounds ran out, " (review off)" when disabled.

        Coverage is tracked per persona across rounds; when the rounds run
        out, a persona that never returned a verdict is named in the stop
        reason, so an awaiting-approval run shows what was never reviewed."""
        settings = self.config["review"]
        if settings["max_rounds"] <= 0:
            return " (review off)"
        base = self.rundir.base_commit
        diff = gitops.git(["diff", f"{base}...HEAD"], self.worktree, check=False)
        changed = gitops.git(["diff", "--name-only", f"{base}...HEAD"],
                             self.worktree, check=False).splitlines()
        chosen = review.select(changed, self.objective, self.rundir.read_plan(),
                               diff, settings["personas"])
        if self.review_round >= settings["max_rounds"]:
            never = [name for name, _ in chosen
                     if self.review_coverage.get(name) in (None, "no verdict")]
            suffix = " (review unresolved)"
            if never:
                suffix += f"; {' and '.join(never)} never got a verdict"
            return suffix
        self.review_round += 1
        self._save_run_state()
        # Every persona reviews every round.  Stopping at the first objection
        # starved design: correctness always found something, rounds ran out.
        changes = ""
        for name, why in chosen:
            verdict, findings = self._review_once(f"r{self.review_round}", name, why, diff)
            self.review_coverage[name] = verdict or "no verdict"
            if verdict != "APPROVED":
                self._add_findings(f"review r{self.review_round}/{name}", findings or [
                    f"{name} reviewer produced no verdict; re-check the work against the objective"])
                changes = "changes"
        self._save_run_state()
        return changes

    def _drift_check(self, iteration: int) -> None:
        """Every `every` working iterations: is the run still aimed right?
        Never blocks; findings join the plan and do not count as a round."""
        every = self.config["review"]["every"]
        if every <= 0 or iteration % every or self.interrupted or self.rundir.stop_requested():
            return
        base = self.rundir.base_commit
        diff = gitops.git(["diff", f"{base}...HEAD"], self.worktree, check=False)
        try:
            verdict, findings = self._review_once(f"drift{iteration}", "correctness",
                                                  "drift check", diff, drift=True)
        except PreflightError:
            return  # the next working iteration meets the same server and waits properly
        if verdict == "CHANGES_REQUESTED" and findings:
            self._add_findings(f"drift i{iteration}", findings)

    def _review_once(self, label: str, name: str, why: str, diff: str,
                     drift: bool = False) -> tuple[str, list[str]]:
        verdict_path = self.rundir.path / "review" / f"{label}-{name}.md"
        verdict_path.parent.mkdir(exist_ok=True)
        verdict_path.unlink(missing_ok=True)
        model = self.config["review"]["models"].get(name) or self.config["agent"]["model"]
        ok, detail = models.preflight(
            model, config.reference(self.config["models"]["llama_swap_url"]))
        if not ok:
            raise PreflightError(detail)
        brief = review.brief(name)
        if drift:
            brief += ("\n\nThis is a mid-run drift check, not a final review. Only ask "
                      "for changes if the work is heading away from the objective or "
                      "the plan is wrong. Unfinished work is expected.")
        system = self.rundir.path / "review" / f"{label}-{name}.system.md"
        system.write_text(review.brief("rules") + "\n\n" + brief + "\n")
        prompt = (
            f"Review this lmloop run as the `{name}` reviewer ({why}).\n\n"
            f"## Objective\n\n{self.objective}\n\n"
            f"## Plan\n\n{self.rundir.read_plan()}\n\n"
            f"## Gate\n\n{self.gate_result or 'no gate'}\n{self.gate_output[-4000:]}\n\n"
            f"## Diff against the base\n\n```diff\n{diff[:60000]}\n```\n\n"
            f"Write your verdict to `{verdict_path}`.\n"
        )
        (self.rundir.path / "review" / f"{label}-{name}.prompt.md").write_text(prompt)
        head = gitops.head_commit(self.worktree)
        self.rundir.event("review:start", label=label, persona=name, why=why, model=model)
        self.screen.log(f"  review {label}: {name} ({why})")
        pi_runner.run(
            model=model, agent_name=self.harness_name,
            tools=self.config["agent"]["tools"], thinking=self.config["agent"].get("thinking", ""),
            prompt=prompt, cwd=self.worktree, session_dir=self.rundir.sessions,
            session_id=f"review-{label}-{name}",
            raw_path=self.rundir.path / "review" / f"{label}-{name}.jsonl",
            timeout_seconds=self.config["iteration"]["timeout_seconds"],
            stall_seconds=self.config["iteration"]["stall_seconds"],
            tool_seconds=self.config["iteration"].get("tool_seconds", 0),
            max_compactions=self.config["iteration"]["max_compactions"],
            max_repeats=self.config["iteration"].get("max_repeats", 3),
            env=self.env(), system_prompt=str(system),
            should_stop=lambda: self.interrupted or self.rundir.stop_now_requested(),
        )
        # Read-only is enforced, not requested.  Edits are kept -- as a patch
        # and a stash, never reset -- and the verdict that came with them is not.
        if gitops.has_uncommitted(self.worktree) or gitops.head_commit(self.worktree) != head:
            patch = gitops.git(["diff", head], self.worktree, check=False)
            (self.rundir.path / "review" / f"{label}-{name}.patch").write_text(patch + "\n")
            gitops.git(["stash", "push", "-u", "-m", f"lmloop review edits {label}-{name}"],
                       self.worktree, check=False)
            self.rundir.event("review:edited", label=label, persona=name)
            return "", [f"{name} reviewer edited files instead of reviewing; its edits "
                        f"are in review/{label}-{name}.patch and `git stash list`"]
        try:
            text = verdict_path.read_text()
        except OSError:
            text = ""
        verdict, findings = review.parse(text)
        self.rundir.event("review:verdict", label=label, persona=name,
                          verdict=verdict or "none", findings=findings[:20])
        return verdict, findings

    def _add_findings(self, tag: str, findings: list[str]) -> None:
        lines = "".join(f"- [ ] {f} ({tag})\n" for f in findings)
        with self.rundir.plan_path.open("a") as handle:
            handle.write("\n" + lines)

    def _safely(self, step: str, action) -> None:
        """Run one finalisation step; never let it take the rest down with it.

        The failure is recorded twice on purpose, in the two places the two
        readers look: an event for whoever reads the run back later, and a line
        on the terminal for whoever is watching now.  Both are themselves
        guarded, because the event log is one of the things that can be broken
        at this point -- a full disk takes the run and the record of why with it
        otherwise.
        """
        try:
            action()
        except Exception as error:  # noqa: BLE001 - continuing is the whole point
            detail = f"{type(error).__name__}: {error}"
            try:
                self.rundir.event("finalise:failed", step=step, detail=detail)
            except Exception:  # noqa: BLE001 - the log may be what broke
                pass
            try:
                self.screen.log(f"  {step} failed on the way out: {detail}")
            except Exception:  # noqa: BLE001 - so may the terminal
                pass

    def _finalisation_run_summary(self, reason: str | None, started: float, iterations: int) -> dict:
        """The run dict `notify.send` and `webpush.send` both summarise from.

        Failures are read back from the event log rather than tracked as the
        run went, so a step that failed before either notifier existed still
        shows up correctly the day one does.
        """
        failures: dict[str, int] = {}
        for event in self.rundir.read_events():
            if event.get("event") == "iteration:end":
                outcome = event.get("outcome", "")
                if outcome and outcome != "ok":
                    failures[outcome] = failures.get(outcome, 0) + 1
        return {
            "repo": self.repo.name,
            "project": self.repo.name,
            "run_id": self.run_id,
            "objective": self.objective,
            "iterations": iterations,
            "commits": gitops.commit_count(self.worktree, self.rundir.base_commit),
            "hours": (time.monotonic() - started) / 3600,
            "plan": self.rundir.plan_progress(),
            "reason": reason or "complete",
            "failures": failures,
            "defects": self.defects,
            "dashboard_url": self.config.get("notify", {}).get("dashboard_url", ""),
        }

    def _announce(self, reason: str | None, started: float, iterations: int) -> None:
        """Push one notification saying the run has stopped.

        Last, after the sweep, so the figures quoted are the ones that survive.
        Like the sweep it can never fail a run: a dead server, a typo in a URL
        and a network outage all end the same way, with an event in the log.
        """
        settings = self.config.get("notify", {})
        if not settings.get("url"):
            return

        try:
            import notify

            problem = notify.send(settings, self._finalisation_run_summary(reason, started, iterations))
        except Exception as error:  # noqa: BLE001 - never fails a run
            problem = str(error)

        if problem:
            self.rundir.event("notify:failed", detail=problem)
            self.screen.log(f"  could not notify: {problem}")
        else:
            self.rundir.event("notify", topic=settings.get("topic", ""))

    def _announce_push(self, reason: str | None, started: float, iterations: int) -> None:
        """Web Push counterpart to `_announce`: push to every browser
        subscribed through the dashboard instead of (or alongside) ntfy.

        Independent of `[notify]` -- `webpush.py` is itself a no-op when the
        VAPID key is unset or nobody has subscribed, so this needs no gate of
        its own beyond `_safely`'s try/except.
        """
        try:
            import webpush

            problem = webpush.send(self._finalisation_run_summary(reason, started, iterations))
        except Exception as error:  # noqa: BLE001 - never fails a run
            problem = str(error)

        if problem:
            self.rundir.event("webpush:failed", detail=problem)
            self.screen.log(f"  could not web-push: {problem}")

    def _sweep(self) -> None:
        """Reclaim the disk this run cost, now that it has stopped.

        At the end rather than on a timer: this is the moment the space appears
        and the moment somebody is watching, and one honest line about what was
        freed is easier to trust than a cron job quietly rewriting run
        directories overnight.  Nothing is lost -- streams are compressed and
        stay readable; only regenerable bytecode is removed.
        """
        settings = self.config.get("prune", {})
        if not settings.get("after_run", True):
            return
        try:
            import prune

            result = prune.prune(
                [self.repo],
                older_than_days=settings.get("older_than_days", 0.0),
                finished={self.run_id},
            )
        except Exception as error:  # noqa: BLE001 - housekeeping never fails a run
            self.rundir.event("prune:failed", detail=str(error))
            return
        freed = result["saved"] + result["bytecode"]
        if freed:
            self.rundir.event(
                "prune", files=len(result["files"]), saved=result["saved"],
                bytecode=result["bytecode"],
            )
            self.screen.log(f"  reclaimed {freed / 1e6:.0f} MB "
                            f"({len(result['files'])} streams compressed, bytecode dropped)")

    def _on_interrupt(self, *_args) -> None:
        if self.interrupted:
            raise KeyboardInterrupt
        self.interrupted = True
        # A signal is the operator asking for the terminal back, not for another
        # forty minutes of generation, so it cuts the iteration short rather than
        # waiting it out -- and says so, because the previous wording promised the
        # opposite of what the code did.  Nothing is lost either way: whatever the
        # iteration wrote is gated, checked and committed on the way out.
        self.screen.log("  stop requested; ending this iteration now and committing what it has")

    def _transport_failure(self) -> str:
        """The detail, if this iteration died of the server rather than itself
        -- see `policy.transport_failure`.  Observed here: fifty minutes of
        generation ended by a llama-server being swapped for a faster build
        mid-stream.
        """
        return policy.transport_failure(self.last_outcome, self.last_commit, self.last_detail)

    def _classify_provider_loss(self, result) -> None:
        """Turn an opaque local-agent failure into an actionable run state.

        Agent error wording is not a stable interface.  The provider's own cheap
        health endpoint is: if a local request failed and that endpoint is gone,
        the provider disappeared underneath the iteration.  Cloud models never
        touch the local endpoint.
        """
        if (
            result.outcome in {"agent-error", "stalled", "timeout"}
            and models.is_local(self.model)
            and not self._server_is_up()
        ):
            result.outcome = "provider-unavailable"

    def _server_is_up(self) -> bool:
        """Does llama-swap answer at all?  Free, and never triggers a swap."""
        try:
            models.running(config.reference(self.config["models"]["llama_swap_url"]), timeout=5.0)
            return True
        except Exception:  # noqa: BLE001 - any failure to answer means "not there"
            return False

    def _sleep_interruptibly(self, seconds: int) -> bool:
        """Sleep, but stay responsive to Ctrl-C, STOP and PAUSE.  False if asked to quit."""
        for _ in range(seconds):
            if self.interrupted or self.rundir.stop_requested():
                return False
            time.sleep(1)
        return True

    def _wait_for_server(self, iteration: int, detail: str, *, retry_same: bool = True) -> bool:
        """Pause for an absent local provider until the operator resumes the run.

        Only reached for a run whose model is served locally -- see `_backoff`.
        This is the case where the machine's owner needs the GPU: the server is
        stopped deliberately, for as long as a game lasts.  Nothing is wrong and
        nothing needs escalating -- the iteration simply cannot run yet, so the
        loop creates the same PAUSE sentinel used by the dashboard and keyboard
        controls.  Recovery is explicit: start the provider, then resume.  The
        iteration number and active wall-clock budget are not consumed.
        """
        if int(self.config["models"].get("server_wait_seconds", 0)) <= 0:
            return False

        since = time.monotonic()
        self.rundir.pause_path.touch()
        self.rundir.write_status({
            "run_id": self.run_id,
            "iteration": iteration,
            "max_iterations": self.max_iterations,
            "model": self.model,
            "phase": "provider-unavailable",
            "provider_detail": detail,
            "paused": True,
            "stopping": self.rundir.stop_requested(),
        })
        self.rundir.event("provider:pause", iteration=iteration, detail=detail)
        self.screen.log(f"    local model provider unavailable; paused ({detail})")

        display.wait_while_paused(
            self.rundir, self.screen,
            lambda: self.interrupted or self.rundir.stop_requested(),
        )
        waited = time.monotonic() - since
        self._inactive_seconds += waited
        if self.interrupted or self.rundir.stop_requested():
            return False
        if not self._server_is_up():
            # The operator may resume before starting the provider.  Retry the
            # same iteration; its preflight will immediately restore the pause.
            self.rundir.event("provider:still-unavailable", iteration=iteration)
            return True

        next_iteration = iteration if retry_same else iteration + 1
        self.rundir.event(
            "provider:resume", iteration=iteration, nextIteration=next_iteration,
            waited=round(waited),
        )
        action = "retrying" if retry_same else "continuing with"
        self.screen.log(f"    local model provider is back; {action} iteration {next_iteration}")
        self._errors = 0
        return True

    def _backoff(self, iteration: int, detail: str) -> bool:
        """Hold after a server-side failure.  True to retry, False to end the run.

        Two different failures wear the same coat here, and they need opposite
        treatment:

          * The server is NOT THERE -- stopped by hand, rebooting, GPU wanted for
            something else.  Waiting is exactly right, and the wait may be hours.
            Handled by `_wait_for_server`.
          * The server IS there and failing -- a bad build, a model that will not
            load, an OOM on every request.  Waiting six hours on that just wastes
            six hours, so it keeps the original 1m/2m/4m and then gives up.

        `GET /running` tells them apart, and it is the same call preflight makes.

        Both of those are about a *local* server, and only a run whose model
        comes from one has anything to wait for.  Asking regardless cost a
        cloud-model run six hours: with no llama-swap listening, the very first
        agent-error found the server "not there", parked on the long wait for a
        service the run never used, and only then failed -- the same failure the
        short backoff reaches in seven minutes.
        """
        if models.is_local(self.model) and not self._server_is_up():
            return self._wait_for_server(iteration, detail)

        if not hasattr(self, "_errors"):
            self._errors = 0
        self._errors += 1
        delay = policy.backoff_delay(self._errors)
        if delay is None:
            return False
        self.rundir.event("backoff:start", iteration=iteration, seconds=delay, detail=detail)
        self.screen.log(f"    {detail}; retrying in {delay // 60}m")
        return self._sleep_interruptibly(delay)

    def _summarise(self, reason: str | None, started: float) -> None:
        base = self.rundir.base_commit
        # Paths are shown relative to the repo, and the commands assume you are
        # standing in it.  Absolute paths here ran to 217 characters, which wraps
        # into noise on a phone -- and the worktree is inside the repo anyway.
        def relative(path: Path) -> str:
            try:
                return str(path.relative_to(self.repo))
            except ValueError:
                return str(path)

        self.screen.log()
        self.screen.log(f"  run stopped: {reason or 'complete'}")
        self.screen.log(f"  {gitops.commit_count(self.worktree, base)} commits on {self.branch}"
                        f" in {(time.monotonic() - started) / 3600:.1f}h")
        self.screen.log()
        # The notes path relative to the *worktree*, not the repo: relative to the
        # repo it repeats the run id twice and reaches 130 characters, which is
        # four wrapped lines on a phone for one file name.  The worktree it hangs
        # off is printed directly above it.
        try:
            notes = self.rundir.notes_path.relative_to(self.worktree)
        except ValueError:
            notes = self.rundir.notes_path
        self.screen.log(f"  worktree  {relative(self.worktree)}")
        self.screen.log(f"  notes     {notes}  (inside it)")
        self.screen.log()
        self.screen.log(f"  from {self.repo}:")
        self.screen.log(f"    git log --oneline {base[:8]}..{self.branch}")
        self.screen.log(f"    git merge {self.branch}")


class PreflightError(RuntimeError):
    pass
