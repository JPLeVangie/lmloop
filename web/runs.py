"""Finding and reading lmloop runs, for the web UI.

The predecessor dashboard spent 732 lines here, most of it reconstructing the present by parsing
an append-only log to its end -- `parse_activity`, `parse_outcome`,
`parse_managed_log`, and a run-directory cache to make that affordable at all.
None of that is needed against lmloop, which writes `status.json`: the present,
atomically, in one small file, every couple of seconds.  So this module reads
files rather than replaying history, and is a fifth of the size.

What it must still be careful about is honesty.  A crashed run leaves its last
`status.json` behind saying `working`, and a dashboard that believes it shows a
dead run as a live one forever.  Every run therefore gets its state from the age
of that file and from whether the log records a completion -- never from the
`phase` field alone.
"""

from __future__ import annotations

import json
from pathlib import Path

import config as config_module
import eta
import policy
import runrecord
from preview import Preview

# See `runrecord.STALE_AFTER_SECONDS`; kept as an attribute here too since it
# is part of this module's own public surface.
STALE_AFTER_SECONDS = runrecord.STALE_AFTER_SECONDS

# Enough of a plan or handoff to render without shipping the whole file to a
# phone on every poll.  The detail view fetches the rest.
PREVIEW_CHARS = 2000


def _read_json(path: Path) -> dict:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def _read_text(path: Path, limit: int = 200_000) -> str:
    try:
        with path.open("r", encoding="utf-8", errors="replace") as handle:
            return handle.read(limit)
    except OSError:
        return ""


def _age_seconds(stamp: str | None) -> float | None:
    return runrecord.age_seconds(stamp)


# -- discovery --------------------------------------------------------------


def projects(roots: list[Path]) -> list[dict]:
    """Git repositories one level below each root.

    Grouped by root and de-duplicated by name, earlier roots winning, which is
    the rule the predecessor dashboard's project picker already used and users already expect.
    """
    found: dict[str, dict] = {}
    for root in roots:
        try:
            entries = sorted(path for path in root.iterdir() if path.is_dir())
        except OSError:
            continue
        for path in entries:
            if not (path / ".git").exists():
                continue
            if path.name in found:
                continue
            found[path.name] = {
                "id": path.name,
                "name": path.name,
                "path": str(path),
                "root": str(root),
                "runs": len(run_dirs(path)),
            }
    return list(found.values())


def _worktree_root(project: Path) -> Path:
    """Where this project's run worktrees live, honouring its own config.

    A project may relocate them with `[worktree] root`, and a dashboard that
    assumes the default silently shows no runs for exactly the projects whose
    owner cared enough to configure them. Goes through `config.load` -- the
    same defaults-then-global-then-project layering the CLI resolves this
    with -- rather than reading the project's `.lmloop.toml` alone, so a root
    relocated only in `~/.config/lmloop/config.toml` is not invisible here.
    See `runrecord.worktree_root`.
    """
    return runrecord.worktree_root(project, config_module.load(project, strict=False))


def _runs_under(root: Path) -> list[Path]:
    try:
        return [path for path in root.glob("*/.lmloop/runs/*") if path.is_dir()]
    except OSError:
        return []


def _checkout_tree(base: Path):
    """Yield a checkout and any lmloop worktrees launched from its worktrees.

    Follow-on runs are valid: an operator may continue from an unmerged run
    branch, which makes the child's checkout live under
    ``parent/.worktrees/child``.  Walk only configured worktree roots rather
    than the repository recursively; source trees may contain arbitrary nested
    directories, while every directory under a worktree root is a checkout.
    """
    pending, seen = [base], set()
    while pending:
        checkout = pending.pop(0)
        try:
            key = checkout.resolve()
        except OSError:
            key = checkout
        if key in seen:
            continue
        seen.add(key)
        yield checkout
        root = _worktree_root(checkout)
        try:
            pending.extend(sorted(path for path in root.iterdir() if path.is_dir()))
        except OSError:
            pass


def _checkout_candidates(project: Path):
    """Yield every checkout from the project and its pinned pilot bases."""
    bases = [project, *sorted((project / ".pilot-bases").glob("*"))]
    return (checkout for base in bases if base.is_dir() for checkout in _checkout_tree(base))


def run_dirs(project: Path) -> list[Path]:
    """Every run directory belonging to a project, newest run id first.

    Runs normally hang off the repository, and that is one glob.  A pilot checks
    the repository out again under `.pilot-bases/<name>` and runs lmloop there,
    so its worktrees hang off *that* copy instead -- two path segments deeper
    than the glob reaches, which made a real omp run invisible to the dashboard
    while every other reader handled it correctly.  `projects()` cannot find
    those bases either, since it looks exactly one level below a root, so
    neither half of the discovery saw them.

    Each base is asked for its own worktree root, because it carries its own
    `.lmloop.toml` -- which is the whole point of pinning a base commit.
    """
    checkouts = _checkout_candidates(project)
    found = [run for checkout in checkouts for run in _runs_under(_worktree_root(checkout))]
    return sorted(found, key=lambda path: path.name, reverse=True)


def owner(project: Path, run_dir: Path) -> Path:
    """Repository checkout whose worktree root contains ``run_dir``.

    A run that recorded its own `repoPath` (see `runrecord.resolved_owner`)
    settles this outright: it is the run's own account of where it was
    launched from, not a guess reconstructed from directory layout. Only a run
    from before that field existed falls through to the walk over
    `.pilot-bases` and every candidate's own `[worktree] root` template.
    """
    resolved = runrecord.resolved_owner(runrecord.latest_run_start(_events(run_dir)))
    if resolved is not None:
        return resolved
    candidates = _checkout_candidates(project)
    # The deepest matching checkout owns a chained run; the repository root is
    # also an ancestor, but dashboard actions must run from the immediate parent.
    for checkout in sorted(candidates, key=lambda path: len(path.parts), reverse=True):
        if _worktree_root(checkout) in run_dir.parents:
            return checkout
    return project


def route_id(project: Path, run_dir: Path) -> str:
    """Stable route component; disambiguates equal ids in separate pilot bases."""
    owning = owner(project, run_dir)
    return run_dir.name if owning == project else f"{owning.name}::{run_dir.name}"


# -- the archive ------------------------------------------------------------
#
# A finished run's worktree is the expensive part of it -- a whole checkout,
# tens of megabytes -- while the part worth keeping is the few megabytes of
# `.lmloop/runs/<id>` inside it: the plan, the handoff, the event stream, every
# prompt.  Archiving copies that out and removes only the checkout, so the run
# stays readable in the dashboard after the tree it ran in is gone.
#
# This is what lets the UI offer removal at all without breaking the first
# invariant in CLAUDE.md.  Nothing is discarded by archiving; a run that
# produced nothing is still diagnosable afterwards, which is the whole point of
# the rule.  Permanent deletion exists, but only as a second, separate step on a
# run that has already been archived.

import os

# Tests and callers may override this; the deployment setting itself is read at
# call time because web.server imports this module before it loads web.env.
ARCHIVE_ROOT: Path | None = None


def archive_root() -> Path:
    if ARCHIVE_ROOT is not None:
        return ARCHIVE_ROOT
    configured = os.environ.get(
        "LMLOOP_WEB_ARCHIVE", str(Path.home() / "lmloop-archive" / "runs")
    )
    return Path(os.path.expandvars(configured)).expanduser()


def archived_dirs(project_id: str) -> list[Path]:
    """Archived run directories for one project, newest run id first."""
    try:
        found = [path for path in (archive_root() / project_id).iterdir() if path.is_dir()]
    except OSError:
        return []
    return sorted(found, key=lambda path: path.name, reverse=True)


def archive_target(project_id: str, run_id: str) -> Path:
    return archive_root() / project_id / run_id


def is_archived(run_dir: Path) -> bool:
    try:
        return archive_root() in run_dir.parents
    except (OSError, ValueError):
        return False


# -- reading ----------------------------------------------------------------


def _events(run_dir: Path) -> list[dict]:
    """The run's own event log, parsed -- see `runrecord.read_events`."""
    return runrecord.read_events(run_dir)


def _holder(run_dir: Path) -> int:
    """The pid of a live lmloop loop on this run, or 0.  See `runrecord.holder`."""
    return runrecord.holder(run_dir)


def _state(run_dir: Path, status: dict, events: list[dict]) -> tuple[str, float | None]:
    """What is actually happening, from evidence rather than self-report.

    Order matters.  A finished run keeps its last status file forever, so
    completion is checked before liveness; and a live run that someone has asked
    to stop is still running, so the sentinels are checked after.
    """
    age = _age_seconds(status.get("updated_at"))
    # The last thing that happened, not the last *lifecycle* thing that
    # happened.  A `run:complete` followed by an `iteration:start` is a run that
    # is plainly still going, and reading only lifecycle events made the
    # dashboard report "finished" over the top of a working loop: a second loop
    # had been started beside the first and wrote its own `run:complete` into
    # the shared log when it died.  That second loop can no longer happen, but
    # the log it poisoned is still on disk, and anything that can be settled by
    # looking at what came last should be.
    latest = ""
    for event in events:
        name = event.get("event")
        if name in ("run:start", "run:complete", "iteration:start", "iteration:end"):
            latest = name
    if latest == "run:complete" and not _holder(run_dir):
        phase = status.get("phase")
        return (phase if phase in ("completed", "stopped") else "stopped"), age
    if age is None:
        return "unknown", None
    if age > STALE_AFTER_SECONDS and not _holder(run_dir):
        # The loop stopped writing without recording a completion: killed,
        # OOMed, rebooted.  Its status file still says "working".
        #
        # Only when no loop is actually there, though.  "Stale" is a claim about
        # the process, not about the clock, and a run holding on PAUSE went
        # quiet for forty minutes and got called stale -- which pushed the
        # dashboard into offering "continue", which started a second loop beside
        # the first.  A live pid is the difference between quiet and gone.
        return "stale", age
    if runrecord.stop_requested(run_dir):
        return "stopping", age
    if runrecord.paused(run_dir):
        return "paused", age
    return "running", age


def _defects(events: list[dict]) -> list[str]:
    """Structural problems the last iteration left behind, if any.

    Worth surfacing on a phone: a run can be committing happily while every file
    it touches has stopped parsing, and that is precisely the state where
    stopping it early is worth something.
    """
    latest: list[str] = []
    for event in events:
        if event.get("event") == "checks:failed":
            latest = event.get("problems", [])
        elif event.get("event") == "iteration:start":
            latest = []
    return latest


def _current_step(text: str) -> str:
    """The first unchecked step: what the run is working on right now.

    This is the single most useful sentence about a live run, and it was
    previously only visible by opening the plan.  Markdown emphasis is stripped
    -- the agent writes `**Fix dark-mode.css conflict**` for itself, and a status
    line should read as a sentence rather than as source.  See
    `runrecord.first_unchecked_step` for why this strips more than the
    runner's own reading of the same plan does.
    """
    import re as _re

    step = runrecord.first_unchecked_step(text)
    step = _re.sub(r"\*\*([^*]+)\*\*", r"\1", step)
    step = step.replace("`", "")
    return step[:160]


def _plan_name(text: str) -> str:
    """The name the agent gave this run, from the plan's own heading.

    Better than the first line of the objective, which is what this replaced: an
    objective is a paragraph written to be acted on, so its first hundred
    characters are a sentence cut in half, and every run of a similar objective
    looks identical in a list.  The heading is the agent's own short label for
    the work, written once it has read the repository and knows what the work
    actually is.

    Tolerant of what earlier runs wrote before the prompt asked for this, which
    was usually `# Plan -- <name>`.
    """
    import re as _re

    for line in text.splitlines()[:20]:
        stripped = line.strip()
        if not stripped.startswith("#"):
            continue
        name = stripped.lstrip("#").strip()
        # `Plan`, `Plan:`, `Plan -- `, `Plan — ` and the em-dash forms of each,
        # at either end: agents put it in front as often as they append it, and
        # "Test Suite Implementation Plan" is the same non-label as "Plan: test
        # suite".  Only stripped when something is left over.
        for pattern in (r"^plan\b[\s:\u2013\u2014-]*", r"[\s:\u2013\u2014-]*\bplan\.?$"):
            trimmed = _re.sub(pattern, "", name, flags=_re.I).strip()
            if trimmed:
                name = trimmed
        name = _re.sub(r"\*\*([^*]+)\*\*", r"\1", name).replace("`", "")
        if name:
            return name[:80]
    return ""


def _plan_progress(text: str) -> tuple[int, int]:
    return runrecord.plan_progress(text)


def summarise(project: dict, run_dir: Path) -> dict:
    """One run, as much as a list view needs and no more."""
    status = _read_json(run_dir / "status.json")
    events = _events(run_dir)
    state, age = _state(run_dir, status, events)
    plan = _read_text(run_dir / "plan.md")
    done, total = _plan_progress(plan)

    commits = 0
    outcomes: list[str] = []
    for event in events:
        if event.get("event") == "iteration:end":
            outcomes.append(event.get("outcome", ""))
        if event.get("event") == "run:complete":
            commits = event.get("commitCount", commits)

    # Which agent did the typing.  It lives in `run:start` rather than in
    # status.json because it cannot change within a run, and `_events` has
    # already read the log for the outcomes above -- so surfacing it costs no
    # extra I/O and works on runs that finished before anyone thought to show
    # it.  A run older than the field says nothing rather than guessing "pi".
    agent = next(
        (event.get("agent", "") for event in reversed(events)
         if event.get("event") == "run:start"),
        "",
    )

    objective = _read_text(run_dir / "prompt.md", 4000).strip()
    # The agent's name for the run when it has written one; the objective's
    # first line only as a fallback, for runs that predate the plan heading.
    name = _plan_name(plan)
    # An archived run has no worktree and no loop, so none of the liveness
    # reasoning above applies to it -- and every control that acts on a run acts
    # on its worktree.  Naming the state outright is what keeps the UI from
    # offering to pause something that is not there.
    archived = is_archived(run_dir)
    if archived:
        state = "archived"
    try:
        worktree = run_dir.parents[2] if len(run_dir.parents) >= 3 else Path(project["path"])
        preview_config = config_module.load(worktree, strict=False)
        preview = Preview(run_dir, preview_config).status()
    except (OSError, SystemExit, ValueError, IndexError):
        preview = {"enabled": False, "state": "disabled", "url": "", "url_template": ""}
    estimate = eta.estimate(
        events, elapsed_seconds=status.get("elapsed_seconds") or 0,
        iteration=status.get("iteration") or 0,
        max_iterations=status.get("max_iterations") or 0,
        plan_done=done, plan_total=total,
    ) if state == "running" else {}
    completed_seconds = sum(
        float(event.get("elapsedMs") or 0) / 1000
        for event in events if event.get("event") == "iteration:end"
    )
    run_elapsed_seconds = completed_seconds + (
        float(status.get("elapsed_seconds") or 0) if state == "running" else 0
    )
    return {
        "run_id": run_dir.name,
        "route_id": route_id(Path(project["path"]), run_dir),
        "project": project["id"],
        "project_path": project["path"],
        "archived": archived,
        "state": state,
        "age_seconds": round(age) if age is not None else None,
        "objective": objective,
        "title": name or (objective.splitlines()[0][:120] if objective else run_dir.name),
        "named": bool(name),
        "model": status.get("model", ""),
        "agent": agent,
        "thinking": status.get("thinking", ""),
        "role": status.get("role", ""),
        "context_window": status.get("context_window"),
        "max_output_tokens": status.get("max_output_tokens"),
        "input_tokens": status.get("input_tokens"),
        "tokens_per_second": status.get("tokens_per_second"),
        "iteration": status.get("iteration"),
        "max_iterations": status.get("max_iterations"),
        "phase": status.get("phase", ""),
        "last_tool": status.get("last_tool", ""),
        "last_target": status.get("last_target", ""),
        "current_step": _current_step(plan),
        "defects": _defects(events),
        "quiet_seconds": status.get("quiet_seconds"),
        "output_tokens": status.get("output_tokens"),
        "peak_output": status.get("peak_output"),
        "truncations": status.get("truncations"),
        "elapsed_seconds": status.get("elapsed_seconds"),
        "run_elapsed_seconds": round(run_elapsed_seconds),
        "tool_calls": status.get("tool_calls"),
        "writes": status.get("writes"),
        "compactions": status.get("compactions"),
        "plan_done": done,
        "plan_total": total,
        "paused": runrecord.paused(run_dir),
        "stopping": runrecord.stop_requested(run_dir),
        "iterations_done": len(outcomes),
        "outcomes": outcomes,
        "commits": commits,
        "updated_at": status.get("updated_at"),
        "preview": preview,
        **estimate,
    }


def detail(project: dict, run_dir: Path) -> dict:
    """Everything the run has written, for the detail view."""
    record = summarise(project, run_dir)
    record.update({
        "plan": _read_text(run_dir / "plan.md"),
        "handoff": _read_text(run_dir / "handoff.md"),
        "notes": _read_text(run_dir / "notes.md"),
        # `parents[2]` walks <worktree>/.lmloop/runs/<id> back to the worktree.
        # An archived run is not nested that way and has no worktree at all, so
        # the same arithmetic would name some unrelated directory.
        "worktree": "" if record["archived"] else str(run_dir.parents[2]),
        "run_dir": str(run_dir),
        "iterations": _iterations(run_dir),
    })
    return record


def _iterations(run_dir: Path) -> list[dict]:
    """Per-iteration outcomes, joined from the event log.

    This is the run's history, and it is the one place worth being precise about
    failure: `thrashing`, `truncated`, `no-action` and `stalled` each mean
    something different about what to change, and collapsing them to "failed"
    throws away the diagnosis the loop worked to produce.

    `pressure` is the same care applied to the iteration *before* the failure.
    A run that overflows looks, in a table of outcomes, like ordinary work
    followed by an inexplicable compaction -- but the loop already knew, and
    said so in a `context:pressure` event: three iterations at 80-84% of the
    window is what an overflow looks like on the way in.  Carried here rather
    than recomputed, so the threshold stays defined in `policy` alone, and per
    iteration rather than per run because the window is not constant across
    one: planning and a thrash retry each escalate to a different model.
    """
    rows: dict[int, dict] = {}
    for event in _events(run_dir):
        number = event.get("iteration")
        if not isinstance(number, int):
            continue
        if event.get("event") == "iteration:start":
            # The model is recorded per iteration, not per run: planning uses one
            # and a thrash retry escalates to another, so a history that names
            # only the configured model misattributes exactly the iterations
            # worth looking at.
            rows.setdefault(number, {"iteration": number}).update({
                "started_at": event.get("timestamp"),
                "model": event.get("model", ""),
                "role": event.get("role", ""),
            })
        elif event.get("event") == "iteration:end":
            rows.setdefault(number, {"iteration": number}).update({
                "outcome": event.get("outcome"),
                "seconds": round(event.get("elapsedMs", 0) / 1000),
                "tool_calls": event.get("toolCalls"),
                "writes": event.get("writes"),
                "compactions": event.get("compactions"),
                "commit": event.get("commit"),
                "summary": event.get("summary", ""),
                "gate": event.get("gate", ""),
                "plan_done": event.get("planDone"),
                "plan_total": event.get("planTotal"),
                "input_tokens": event.get("totalInputTokens"),
                "output_tokens": event.get("totalOutputTokens"),
                "peak_output": event.get("peakOutputTokens"),
                "truncations": event.get("truncations"),
            })
        elif event.get("event") == "context:pressure":
            share = policy.context_pressure(
                event.get("inputTokens") or 0, event.get("window") or 0)
            if share:
                rows.setdefault(number, {"iteration": number}).update({
                    "pressure": share,
                    "context_window": event.get("window"),
                })
    return [rows[key] for key in sorted(rows)]


def all_runs(roots: list[Path]) -> list[dict]:
    """Every run under every project, most recently updated first."""
    collected = []
    live_names = set()
    for project in projects(roots):
        for run_dir in run_dirs(Path(project["path"])):
            live_names.add((project["id"], run_dir.name))
            collected.append(summarise(project, run_dir))
    # Then the archive.  A run id can legitimately exist in both places -- the
    # worktree was removed and the name later re-used -- and the live one is the
    # one anybody means, so the archived copy is skipped rather than listed
    # twice under the same id.
    for project in projects(roots):
        for run_dir in archived_dirs(project["id"]):
            if (project["id"], run_dir.name) in live_names:
                continue
            collected.append(summarise(project, run_dir))
    collected.sort(key=lambda run: (run["updated_at"] or ""), reverse=True)
    return collected
