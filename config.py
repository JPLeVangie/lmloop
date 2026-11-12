"""Configuration: a global TOML file, overridden per repository.

The predecessor reads one global config and nothing else, which is why its dashboard has to
rewrite ``~/.predecessor/config.yml`` in place just to choose a model for a run.  A
project-local file removes that whole class of hack: per-repo gate commands and
worktree placement live with the repo they describe.

## Why TOML and not YAML

Asked for as `config.yaml`, and answered no, deliberately.  Python's standard
library reads TOML (`tomllib`, 3.11+) and does not read YAML.  The ways out were:
add PyYAML as a dependency, keep TOML, or accept YAML only when PyYAML happens
to be installed.

TOML, because the alternatives cost more than the syntax is worth.  A dependency
would end "standard library only, no build step", which is what lets this be
installed by cloning it and is why a run can start on a machine that has nothing
but Python.  Optional YAML is worse than either: the same file would be read on
one machine and rejected on another, and the failure would arrive as a config
that silently does not exist.  Hand-writing a parser is not on the table -- YAML
has enough edges that a partial one is a liability, and this file would own it
forever.

The gap is small for ten sections of scalars, and TOML's `[section]` maps
exactly onto the shape below.  Worth revisiting if lmloop ever grows a
dependency for another reason; not worth acquiring one for this.

## Compatibility

A config written before a rename keeps working, and the renames so far are
handled in place rather than by a version number:

* `[stop] max_iterations` sets both `initial_turns` and `hard_turn_ceiling`,
  and an explicit new key wins over it.  Translated in `load`, and still a
  valid setting as far as `validate` is concerned -- legacy is not wrong.
* `[models] local_provider` (a string) is read as a one-item `local_providers`;
  see `models.local_providers`.

No `schema_version` here, deliberately, unlike `runrecord.py` -- that one is a
contract between two *programs* that must agree about files on disk, where a
version is what lets a reader refuse politely.  This is a file a person writes,
where a rename is better absorbed than announced.  Add one if a change ever
cannot be: the shim goes in `load`, and `validate` keeps accepting the old
spelling so nobody is told their working config is a mistake.
"""

from __future__ import annotations

import difflib
import os
import shutil
import string
import subprocess
import sys
import tomllib
from pathlib import Path

import harness
from config_defaults import DEFAULTS

GLOBAL_CONFIG = Path.home() / ".config" / "lmloop" / "config.toml"
# Operator instructions appended to every run's system prompt; edited from the
# dashboard.  Absent means none.
SYSTEM_PROMPT = GLOBAL_CONFIG.parent / "system-prompt.md"
PROJECT_CONFIG = ".lmloop.toml"



def _merge(base: dict, override: dict) -> dict:
    """Two-level merge; the config is deliberately only two levels deep."""
    merged = {section: dict(values) for section, values in base.items()}
    for section, values in override.items():
        if isinstance(values, dict):
            merged.setdefault(section, {}).update(values)
        else:
            merged[section] = values
    return merged


def _read(path: Path) -> dict:
    try:
        with path.open("rb") as handle:
            return tomllib.load(handle)
    except FileNotFoundError:
        return {}
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise SystemExit(f"lmloop: cannot read {path}: {error}") from error


def _tool_names(value) -> list[str]:
    """Normalize comma-separated or TOML-list tool names."""
    values = value.split(",") if isinstance(value, str) else value or []
    return [name.strip() for name in values if isinstance(name, str) and name.strip()]


def resolve_tools(harness_name: str, tools: str, strict: bool = True) -> str:
    """The tool allowlist, reconciled with the agent that has to accept it.

    Two agents, two vocabularies.  pi takes whatever it is handed and ignores
    what it does not recognise; omp checks the list against its own built-ins
    and exits 1 -- ``Unknown tools in --tools: replace, ls`` -- before it emits
    a single event.  So the default above, which is pi's, is not a default omp
    can be given.

    An allowlist still identical to the shipped one is nobody's decision, so
    selecting omp swaps in omp's.  Anything else is a decision, and gets
    checked rather than replaced: naming a tool the agent does not have fails
    here, where the operator is still reading config, instead of after a run
    has built a worktree, written a prompt and started an iteration.

    Testing the string rather than remembering whether it was written down
    keeps this true wherever the value came from -- the defaults, either config
    file, or `--agent` on the command line, which arrives long after the files
    have been forgotten.
    """
    agent_name = (harness_name or "pi").strip().lower()
    try:
        adapter = harness.get(agent_name)
    except SystemExit:
        if strict:
            raise
        return tools
    # Only when the operator never chose: an allowlist they typed is theirs,
    # even if this agent will reject half of it -- `unknown_tools` below is
    # where they get told, rather than here where it would be silently
    # replaced.
    if adapter.default_tools and tools == DEFAULTS["agent"]["tools"]:
        return adapter.default_tools
    unknown = adapter.unknown_tools(tools)
    if unknown and strict:
        raise SystemExit(
            f"lmloop: [agent] tools names {', '.join(unknown)}, which {agent_name} "
            f"does not have; it would exit before the first iteration.  Known: "
            f"{', '.join(sorted(adapter.known_tools))}"
        )
    return tools


# What a value of each shape is called when telling somebody they typed the
# wrong one.  `bool` before `int` on purpose: in Python `True` is an `int`, and
# reporting "expects an integer" for a `true` would be nonsense.
_SHAPES = ((bool, "true or false"), (int, "a whole number"), (float, "a number"),
           (str, "a string in quotes"), (list, "a list"))


def _shape(value) -> str:
    for kind, name in _SHAPES:
        if isinstance(value, kind):
            return name
    return type(value).__name__


def _accepts(expected, value) -> bool:
    """Is `value` a usable stand-in for a default of `expected`'s shape?

    One deliberate looseness: a whole number where a float is expected.  TOML
    tells `0` and `0.0` apart and nobody writing `older_than_days = 0` means
    anything different by it.
    """
    if isinstance(expected, bool):
        return isinstance(value, bool)
    if isinstance(expected, float):
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if isinstance(expected, int):
        return isinstance(value, int) and not isinstance(value, bool)
    return isinstance(value, type(expected))


def _suggest(name: str, known) -> str:
    close = difflib.get_close_matches(name, sorted(known), n=1, cutoff=0.6)
    return f"; did you mean `{close[0]}`?" if close else ""


def reference(value: str) -> str:
    """Resolve a setting that may point at its value instead of holding it.

    The same spellings `secret` takes, for settings that are not credentials
    but are still nobody else's business: an ntfy host and topic are together
    enough to push to somebody's phone, and `dashboard_url` names a machine.

    Separate from `secret` only in what it says about the caller's intent --
    the resolution is identical, and a literal is still a literal, so every
    config that predates this keeps working untouched.
    """
    return secret(value)


def secret(value: str) -> str:
    """Resolve a secret a config points at rather than contains.

    A config file gets copied into a repo, pasted into an issue, and read by the
    agent the loop is driving -- an agent whose whole job is reading files and
    which will happily quote one back.  So a credential belongs somewhere else,
    and this is how a config says where:

        token = "env:NTFY_TOKEN"          from the environment
        token = "file:~/.config/ntfy"     from a file
        token = "!pass show ntfy"         from a command's output
        token = "hunter2"                 literally, still allowed

    The `!command` spelling is omp's, already in use in `~/.omp/agent/models.yml`
    for exactly this; matching it means one convention rather than two.

    A reference that cannot be resolved comes back empty rather than raising: an
    unreachable notification token should cost one line at the end of a run,
    which is what `_announce` already does with a failure, not the run itself.
    Never returns the reference as though it were the value -- sending the
    literal `env:NTFY_TOKEN` as a bearer token would look like an auth failure
    from a server nobody can see.
    """
    if not isinstance(value, str) or not value:
        return ""
    if value.startswith("env:"):
        return os.environ.get(value[4:].strip(), "")
    if value.startswith("file:"):
        try:
            return Path(value[5:].strip()).expanduser().read_text().strip()
        except OSError:
            return ""
    if value.startswith("!"):
        try:
            done = subprocess.run(
                value[1:].strip(), shell=True, capture_output=True,
                text=True, timeout=15,
            )
        except (OSError, subprocess.SubprocessError):
            return ""
        return done.stdout.strip() if done.returncode == 0 else ""
    return value


def require_model(config: dict) -> None:
    """Refuse to start a run with no model, before anything is built.

    Separate from `override_agent`, which settles the *allowlist* and is called
    from places that have no business caring what model is set.  Called where a
    run actually begins, so this lands before a worktree, a branch, a prompt and
    a preflight rather than after them.
    """
    if config["agent"].get("model"):
        return
    agent_name = config["agent"].get("harness", "pi")
    raise SystemExit(
        "lmloop: no model set.  Put one in .lmloop.toml:\n"
        "\n"
        "  [agent]\n"
        '  model = "<provider>/<name>"\n'
        "\n"
        f"  `lmloop models` lists what {agent_name} can reach; "
        "--model overrides it for one run."
    )


# Settings that no longer do anything but are not mistakes.  A config that
# still sets one keeps working and is not told off for it -- the same courtesy
# `[stop] max_iterations` gets, and for the same reason: somebody wrote it when
# it meant something.
#
# `[worktree] keep` only ever had one sane value.  Worktrees are never removed
# automatically -- that is invariant 1, not a policy a setting can vary -- so
# the option was a choice between "always" and something the loop would refuse
# to do. Nothing has read it for as long as the audit can see.
RETIRED = {("worktree", "keep")}


def validate(raw: dict, source: Path, allow_command: bool = True) -> list[str]:
    """Everything wrong with one config file, as lines somebody can act on.

    A config file is hand-written, and until this existed every mistake in one
    was silent.  A misspelled key is not a smaller version of a wrong value --
    it is *no* value, so the run quietly uses the default and does none of what
    was asked.  Measured on a file with three ordinary slips: `modle` left the
    model at `llama-swap/local-fast`, a `[stopp]` section was discarded whole,
    and `timeout_seconds = "900"` sailed through as a string to be compared
    against a number much later, somewhere that says nothing about config.

    Returns rather than raises, so a caller can decide whether a typo should
    stop a run from starting (it should) or stop you reading one that is
    already going (it should not).

    `allow_command` is False for the project file: a `.lmloop.toml` gets
    copied into a repo, pasted into an issue, and read by the agent the loop
    is driving, and a value starting `!` runs a shell command as the operator.
    The global config is trusted and keeps the spelling.
    """
    problems = []
    for section, values in raw.items():
        if section not in DEFAULTS:
            problems.append(
                f"{source}: unknown section `[{section}]`{_suggest(section, DEFAULTS)}"
            )
            continue
        if not isinstance(values, dict):
            problems.append(f"{source}: `{section}` should be a `[{section}]` section")
            continue
        for key, value in values.items():
            if (section, key) in RETIRED:
                continue
            if not allow_command and isinstance(value, str) and value.startswith("!"):
                problems.append(
                    f"{source}: `[{section}] {key}` runs a shell command (`!...`), "
                    f"which is not allowed in project config; use `env:` or `file:` instead"
                )
                continue
            if key not in DEFAULTS[section]:
                problems.append(
                    f"{source}: `[{section}] {key}` is not a setting"
                    f"{_suggest(key, DEFAULTS[section])}"
                )
                continue
            expected = DEFAULTS[section][key]
            if section == "agent" and key == "required_tools":
                valid = isinstance(value, str) or isinstance(value, list)
            elif section == "preview" and key == "port":
                valid = True  # "auto" or an int; the preview branch below says which
            else:
                valid = _accepts(expected, value)
            if not valid:
                alternatives = " or a string in quotes" if section == "agent" and key == "required_tools" else ""
                problems.append(
                    f"{source}: `[{section}] {key}` expects {_shape(expected)}{alternatives}, "
                    f"got {_shape(value)} ({value!r})"
                )
            elif section == "context" and key == "max_chars" and isinstance(value, int) and value < 0:
                problems.append(
                    f"{source}: `[context] max_chars` expects a non-negative whole number, got {value!r}"
                )
            elif section == "agent" and key == "required_tools" and isinstance(value, list):
                bad = [item for item in value if not isinstance(item, str)]
                if bad:
                    problems.append(
                        f"{source}: `[agent] required_tools` list items must be strings, got {bad!r}"
                    )
            elif section == "context" and key == "files" and isinstance(value, list):
                bad = [item for item in value if not isinstance(item, str)]
                if bad:
                    problems.append(
                        f"{source}: `[context] files` list items must be strings, got {bad!r}"
                    )
            elif section == "review" and key in ("max_rounds", "every") and value < 0:
                problems.append(f"{source}: `[review] {key}` must be 0 or more, got {value!r}")
            elif section == "review" and key == "personas":
                bad = [item for item in value if not isinstance(item, str)]
                if bad:
                    problems.append(f"{source}: `[review] personas` items must be strings, got {bad!r}")
            elif section == "review" and key == "models":
                bad = {k: v for k, v in value.items() if not isinstance(v, str)}
                if bad:
                    problems.append(f"{source}: `[review.models]` values must be model names, got {bad!r}")
            elif section == "preview" and key == "command" and isinstance(value, list):
                bad = [item for item in value if not isinstance(item, str)]
                if bad:
                    problems.append(
                        f"{source}: `[preview] command` list items must be strings, got {bad!r}"
                    )
            elif section == "preview" and key == "port":
                if value != "auto" and (not isinstance(value, int) or isinstance(value, bool)
                                        or not 1 <= value <= 65535):
                    problems.append(
                        f"{source}: `[preview] port` expects \"auto\" or a whole number from 1 to 65535, got {value!r}"
                    )
            elif section == "preview" and key in ("path", "ready_path"):
                if not isinstance(value, str) or not value.startswith("/"):
                    problems.append(
                        f"{source}: `[preview] {key}` expects a string beginning with `/`, got {value!r}"
                    )
            elif section == "preview" and key == "startup_timeout_seconds":
                if (not isinstance(value, int) or isinstance(value, bool)
                        or not 1 <= value <= 3600):
                    problems.append(
                        f"{source}: `[preview] startup_timeout_seconds` expects a whole number from 1 to 3600, got {value!r}"
                    )
            elif section == "preview" and key == "url" and isinstance(value, str):
                formatter = string.Formatter()
                try:
                    fields = [name for _, name, _, _ in formatter.parse(value) if name]
                except ValueError:
                    fields = ["<invalid>"]
                if any(name not in {"browser-host", "port", "path"} for name in fields):
                    problems.append(
                        f"{source}: `[preview] url` supports only {{browser-host}}, {{port}}, and {{path}} placeholders"
                    )
            elif section == "preview" and key == "url" and not isinstance(value, str):
                problems.append(
                    f"{source}: `[preview] url` expects a string in quotes, got {_shape(value)} ({value!r})"
                )

    return problems


def load(repo_root: Path, strict: bool = True) -> dict:
    """Defaults, then global, then project; translate the legacy turn limit."""
    global_config = _read(GLOBAL_CONFIG)
    project_config = _read(repo_root / PROJECT_CONFIG)

    problems = (validate(global_config, GLOBAL_CONFIG)
                + validate(project_config, repo_root / PROJECT_CONFIG))
    if problems:
        listed = "\n  ".join(problems)
        if strict:
            raise SystemExit(f"lmloop: config problems:\n  {listed}")
        # Read-only commands still have to work on a config that would refuse
        # to start a run: the whole point of `lmloop status` is the run that is
        # already going, and refusing to show it because of a typo in a setting
        # that run never saw helps nobody.  Said out loud rather than swallowed.
        print(f"lmloop: ignoring config problems:\n  {listed}", file=sys.stderr)

    config = _merge(_merge(DEFAULTS, global_config), project_config)
    explicit_stop = {**global_config.get("stop", {}), **project_config.get("stop", {})}
    if "max_iterations" in explicit_stop:
        legacy = explicit_stop["max_iterations"]
        if "initial_turns" not in explicit_stop:
            config["stop"]["initial_turns"] = legacy
        if "hard_turn_ceiling" not in explicit_stop:
            config["stop"]["hard_turn_ceiling"] = legacy
    # Not strict: this is the path `list`, `status` and `prune` take, and a
    # config that would refuse to start a run must still let you read one that
    # is already running.  Starting a run goes through `override_agent`, which
    # is strict, so nothing reaches an agent unchecked.
    config["agent"]["tools"] = resolve_tools(
        config["agent"].get("harness", "pi"), config["agent"].get("tools", ""),
        strict=False,
    )
    return config


# Required-tool validation is intentionally separate from load: read-only commands
# may inspect an invalid config, while a run validates after command-line overrides.


def validate_required_tools(config: dict) -> None:
    """Reject required tools before a worktree or run directory is created."""
    agent = config.get("agent", {})
    required = _tool_names(agent.get("required_tools", []))
    if not required:
        return
    harness_name = agent.get("harness", "pi")
    adapter = harness.get(harness_name)
    configured = set(_tool_names(agent.get("tools", "")))
    missing_config = sorted(set(required) - configured)
    if not adapter.verifies_required_tools:
        raise SystemExit(
            f"lmloop: [agent] required_tools cannot be verified by {harness_name}; "
            "use a harness with an enforceable tool allowlist"
        )
    missing_known = sorted(set(required) - adapter.known_tools)
    if missing_config or missing_known:
        details = []
        if missing_config:
            details.append("not configured in [agent] tools: " + ", ".join(missing_config))
        if missing_known:
            details.append("not known to " + harness_name + ": " + ", ".join(missing_known))
        raise SystemExit("lmloop: [agent] required_tools cannot be satisfied; " + "; ".join(details))
    unmet = adapter.unmet_requirements(agent.get("model", ""), required)
    if unmet:
        raise SystemExit("lmloop: [agent] required_tools cannot be satisfied; " + "; ".join(unmet))


def override_review(config: dict, rounds: int | None, every: int | None,
                    personas: str | None) -> None:
    """Apply `--review-rounds/--review-every/--review-personas`, in place."""
    review = config["review"]
    for key, value in (("max_rounds", rounds), ("every", every)):
        if value is not None:
            if value < 0:
                raise SystemExit(f"lmloop: --review-{key.split('_')[-1]} must be 0 or more")
            review[key] = value
    if personas is not None:
        review["personas"] = [p.strip() for p in personas.split(",") if p.strip()]


def override_decision(config: dict, enabled: bool | None = None, endpoint: str | None = None,
                      model: str | None = None) -> None:
    """Apply decision model overrides in place."""
    decision = config.setdefault("decision", {
        "enabled": False,
        "endpoint": "http://127.0.0.1:9080/upstream/Clef-Flash/v1/systemone",
        "model": "clef-flash",
        "every": 1,
    })
    if enabled is not None:
        decision["enabled"] = bool(enabled)
    if endpoint:
        decision["endpoint"] = endpoint
    if model:
        decision["model"] = model


def override_agent(config: dict, harness_name: str = "", tools: str = "") -> None:
    """Apply `--agent` / `--tools` from the command line, in place.

    The allowlist has to be settled *after* both, not during either: `--agent
    omp` against a config file that says pi arrives with pi's tool names still
    in hand, and reconciling at load time would have already blessed them.
    """
    if harness_name:
        harness.get(harness_name)  # fail here, not eight lines into a run
        config["agent"]["harness"] = harness_name
    # An agent that is not installed fails the same way an unknown one does, and
    # for the same reason it should fail here: `Popen` raises FileNotFoundError
    # from inside the driver loop, which builds the worktree, writes the prompt,
    # runs the gate probe and *then* dies without a `run:complete`.  omp is
    # installed separately from pi, so a missing binary is a routine input now
    # rather than a broken machine.
    binary = harness.get(config["agent"].get("harness", "pi")).binary
    if not shutil.which(binary):
        raise SystemExit(
            f"lmloop: [agent] harness is {config['agent'].get('harness', 'pi')}, "
            f"whose binary `{binary}` is not on PATH"
        )
    if tools:
        config["agent"]["tools"] = tools
    config["agent"]["tools"] = resolve_tools(
        config["agent"].get("harness", "pi"), config["agent"].get("tools", "")
    )


def sample() -> str:
    """A commented starting config, written by ``lmloop init``."""
    return """\
# lmloop configuration.  Copy to ~/.config/lmloop/config.toml for global
# defaults, or to <repo>/.lmloop.toml to override them for one project.

[agent]
# pi | omp | opencode.  "pi" covers the npm oh-my-pi extension too, because pi
# discovers it and the stream is unchanged.  "omp" is github.com/can1357/oh-my-pi:
# a separate binary with its own browser, task and LSP tools, never selected
# implicitly.  See docs/operations.md for installing it beside pi.
harness = "pi"
# Required; there is no default.  `lmloop models` lists what your agent can
# reach.  Point it directly at a local server rather than through a router --
# a router reports what a model advertises, not how the weights were loaded.
model = "llama-swap/<your-model>"
# This list is pi's.  omp has no `ls` and rejects names it does not have rather
# than ignoring them, so under `harness = "omp"` either comment this line out --
# which gets omp's own default, "read,write,edit,bash,grep,glob" -- or replace
# it.  For work with a user interface in it, omp's native browser:
#   tools = "read,edit,grep,glob,bash,browser"
tools = "read,write,edit,bash,grep,find,ls"
# Every listed tool must be present in `tools` and known by the selected harness.
# A comma-separated string is accepted as well as a TOML list.
required_tools = []
# off | minimal | low | medium | high | xhigh | max.  Empty uses pi's
# default.  Lower it when a model deliberates its whole output budget away
# before calling a tool -- both local models here have done exactly that.
thinking = ""
# Writing the plan is a different job from carrying it out: it reads the whole
# repository once per run, so it wants the widest window you have, while editing
# happens every iteration and wants throughput.  Empty uses `model` for both.
planner_model    = ""      # e.g. "llama-swap/local-wide"
planner_thinking = ""
# Only omp has a browser.  An HTTP CDP discovery endpoint -- not a ws:// URL,
# and not one whose credential rides in the query string; omp's attach drops
# both.  Empty leaves the browser tool to omp's own configuration and skips the
# preflight.  A CDP endpoint is credentials, so it may also point at its value
# rather than hold it: "env:NAME", "file:PATH", "!command" -- see [notify] below.
# browser_cdp_url = "http://127.0.0.1:9222"

[context]
# Tracked files to include verbatim in every iteration prompt, relative to the worktree.
files = []
# Total characters for all configured files; entries are processed in this order.
max_chars = 12000

[models]
# Defaults to whatever ~/.config/lmloop/model-budgets.json says, which is also
# what the pi extension reads.  Set these only to point one repo somewhere else.
# Also names a host, so it too may point at its value -- "env:NAME", "file:PATH",
# "!command" -- see [notify] below.
# llama_swap_url = "http://127.0.0.1:8080"
#
# Which model-id prefixes mean "served by that llama-swap", and so get a real
# measured window instead of whatever the agent's catalogue claims.  Point your
# agents DIRECTLY at llama-swap rather than through a router: a router reports
# what a model advertises, not how the weights were loaded, and one reported
# 262144 for a model actually running with --ctx-size 131072.
# local_providers = ["llama-swap"]
#
# An empty list turns the local path off entirely -- no preflight, no measured
# window -- which is the setting for a machine with no local server.
# local_providers = []

[worktree]
root   = "{repo}/.worktrees/{run_id}"
branch = "lmloop/{run_id}"
# Untracked paths symlinked from the repo into the worktree, so the agent has
# the environment and not just the source.  Missing ones are skipped.  Add
# ".env" here if the project needs it to run -- it is not a default, because it
# would hand the model your secrets without you having asked.
link   = [".venv", "venv", "node_modules"]

[iteration]
timeout_seconds = 14400   # 4h backstop
stall_seconds   = 1200    # 20m of silence from the agent
tool_seconds    = 1800    # 30m for a single tool call; 0 disables
max_compactions = 3       # give up after N context overflows with no writes
max_repeats     = 3       # give up after N unchanged calls to one tool/target

[planning]
pre_write_file_limit = 3  # files allowed before the first write
steps_per_iteration  = 1  # plan steps allowed in one turn

[notify]
# The host and topic may point at their values rather than hold them, the same
# way `token` does: "env:LMLOOP_NTFY_URL", "file:~/.config/ntfy-url",
# "!pass show ntfy/url". Together they are enough to push to your phone.
url           = ""        # e.g. "https://ntfy.example.com"; empty disables
topic         = "lmloop"
token         = ""        # bearer, if the server requires one
dashboard_url = ""        # so the notification taps through to the run

[prune]
after_run       = true    # compress streams and drop bytecode when a run ends
older_than_days = 0       # 0 = including the run that just finished

[gate]
command       = ""        # e.g. "python -m compileall -q backend"
blocks_commit = false     # record the result; commit either way

[preview]
# Empty disables the preview. Commands are TOML argv lists, never shell strings.
# The preview runner substitutes {port} in each argument before execve.
# "auto" picks a free port per start, so several worktrees can preview at once.
command = []
port = "auto"
path = "/"
ready_path = "/"
url = "http://{browser-host}:{port}{path}"
startup_timeout_seconds = 30

[env]
# What the agent -- and the gate -- see of your environment.  The default is an
# allowlist: enough to run a process and a build, plus the harness's own
# namespace (PI_*, OMP_*, OPENCODE_*).  Everything else is left behind,
# including every credential in your shell, for the same reason ".env" is not a
# default under [worktree] link.  A name that looks like a credential is
# dropped even where a prefix rule allowed it, so NODE_* does not bring
# NODE_AUTH_TOKEN with it.  See env.py for the full base list.
inherit = "allowlist"     # "all" restores the pre-allowlist behaviour
# Extra names, exact or with a trailing "*".  Also the opt-in for a credential
# the harness needs in the environment rather than in its own config file --
# that opt-in takes the exact name, so "AWS_*" adds variables but never hands
# over AWS_SECRET_ACCESS_KEY.
pass  = []                # e.g. ["ANTHROPIC_API_KEY"]
block = []                # withheld whatever else allowed them; wins over all

[stop]
# The budget follows the plan: one iteration per step plus retry_allowance
# spare, recomputed as the plan changes.  max_iterations is then the ceiling
# the plan cannot argue past, not the number of steps you expect.
budget_follows_plan = true
retry_allowance     = 5
initial_turns        = 20     # minimum budget before plan-derived growth
hard_turn_ceiling    = 20     # absolute stop; resume --iterations extends it
# max_iterations = 20         # legacy alias for both values above
max_wall_hours       = 10
no_diff_iterations  = 3

[review]
# A finished plan is reviewed by model personas, then waits for you to
# approve it; see docs/review.md.  Each run can override these with
# --review-rounds, --review-every and --review-personas.
max_rounds = 2            # reviewer rounds before handing over unresolved; 0 = off
every      = 5            # drift check every N working iterations; 0 = off
personas   = []           # empty = chosen from the diff: correctness, security,
                          # performance, design; or ~/.config/lmloop/personas/<name>.md
# [review.models]         # a persona's own model; default is the worker's
# design = "llama-swap/Qwen3.8-27B"

[decision]
# Decision model trajectory steering via SystemOne schema (e.g. Clef-Flash).
# Optional and toggleable; disabled by default.
enabled  = false
endpoint = "http://127.0.0.1:9080/upstream/Clef-Flash/v1/systemone"
model    = "clef-flash"
every    = 1
"""
