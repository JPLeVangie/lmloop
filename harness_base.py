"""Shared harness contract and normalized event vocabulary."""

from __future__ import annotations

from pathlib import Path

# The normalised vocabulary.  An adapter maps its agent's events onto these and
# returns None for everything else.
TOOL = "tool"                 # {kind, name, target}
COMPACTION = "compaction"     # {kind}
MESSAGE_END = "message_end"   # {kind, stop_reason, error, input, output}
# A tool call finishing.  Paired with TOOL so the loop can tell "a tool call is
# still running" from "the model is thinking", which look identical from
# outside: both are silence.  An agent that reports a tool call and its result
# as one event has no in-flight state to report and never emits this.
TOOL_END = "tool_end"         # {kind}


# A provider name as an agent prints it: a bare token in the first column.
# Header and rule lines from a table do not match, and neither does a model id
# that already carries its provider.

def _tail(path: str) -> str:
    """Just the file name.  The worktree prefix is identical on every call and
    would push the useful part off a phone screen."""
    return path.rsplit("/", 1)[-1]


class Harness:
    """The interface. Subclasses are small and should stay that way."""

    name = ""
    binary = ""
    # Substrings that make a line worth `json.loads`.  The streams reach tens of
    # megabytes, almost all single-token deltas, so this filter runs on every
    # line before anything parses it.
    interesting: tuple[str, ...] = ()
    # Byte markers proving the model is alive, for the stall clock.  Before the
    # first one, silence means a model is still loading, not that it has hung.
    activity: tuple[bytes, ...] = ()
    # The line carrying the summary an agent wrote for itself on the way out of
    # a context overflow.  `rundir` scans a whole iteration stream for this
    # before parsing anything, so it is bytes, and it belongs to the adapter:
    # omp names that event `auto_compaction_end` where pi names it
    # `compaction_end`, and a marker hardcoded to either harvests nothing from
    # the other and says nothing about having failed.  Empty means the agent
    # does not compact.
    compaction_marker: bytes = b""
    # The event announcing that overflow, by the name this agent gives it.
    # Paired with `compaction_marker` above and separate from it because one is
    # matched against raw bytes before parsing and the other against a parsed
    # event; keeping both on the adapter is what stops an agent recognising a
    # name only its sibling ever emits.
    compaction_event: str = ""
    # The `--tools` names this agent will accept, when it is fussy about it.
    # Empty means it takes whatever it is given; see `unknown_tools`.
    known_tools: frozenset[str] = frozenset()
    # True only when `known_tools` is an authoritative, enforceable capability
    # set. Harnesses with dynamic extension tools cannot prove requirements.
    verifies_required_tools: bool = False
    # Where this agent keeps its own configuration, if lmloop knows.  Used to
    # report what is loaded into it -- extensions in particular, which are
    # invisible from here and can change what a run is allowed to do.
    config_dir: Path | None = None
    # The tool allowlist to use when the operator never chose one.  Empty means
    # "the shipped default in `config.DEFAULTS` already suits this agent" --
    # true for pi, whose names that default was written from, and for opencode,
    # which takes no allowlist at all.  omp rejects names it does not have
    # rather than ignoring them, so it needs its own; see `unknown_tools`.
    default_tools: str = ""
    # The name of this agent's browser tool, if it has one.  Empty means it does
    # not, and `Run.probe_browser` then has nothing to preflight.  A name rather
    # than a flag because the preflight also has to find it in the allowlist.
    browser_tool: str = ""
    # Does this agent announce a tool call starting and finishing separately?
    # Only then can the loop tell "a tool call is still running" from "the
    # model is thinking" and cut a hung subprocess short -- see `tool_seconds`.
    # An agent that reports a call and its result as one event has no in-flight
    # state to offer, and the check has to stay off for it rather than treat
    # every completed call as one that never returned.
    reports_tool_ends: bool = True
    # Environment variables this agent needs, on top of `env.BASE_ALLOW`.
    # Trailing `*` is a prefix.  The adapter owns these because nothing else
    # can: `PI_CODING_AGENT_DIR` relocates pi's whole config directory and is
    # meaningless to opencode, and a list kept anywhere else would have to know
    # every agent's private namespace.  Credential-shaped names still need an
    # explicit `[env] pass` entry -- a prefix here is not an opt-in for one.
    env_passthrough: tuple[str, ...] = ()

    def argv(self, *, model, tools, thinking, session_dir, session_id, system_prompt="", resume_session="") -> list[str]:
        raise NotImplementedError

    def classify(self, event: dict) -> dict | None:
        raise NotImplementedError

    def run_env(self, tools: str) -> dict:
        """Environment additions that enforce `tools` for one run, if any."""
        return {}

    def unmet_requirements(self, model: str, required: list[str]) -> list[str]:
        """Required tools this model cannot back (e.g. images on a text model)."""
        return []

    def unknown_tools(self, tools: str) -> list[str]:
        """Names in a `tools` string this agent has never heard of.

        The allowlist is the one setting an operator carries over verbatim when
        they change agents, and it is the one that does not carry: pi's default
        names `replace` and `ls`, and omp rejects both -- `CliUsageError:
        Unknown tools in --tools`, exit 1, before a single event.  Answering
        this here lets the loop say so while it is still reading config, rather
        than after it has built a worktree for a run that cannot start.
        """
        if not self.known_tools:
            return []
        wanted = [name.strip() for name in tools.split(",") if name.strip()]
        return [name for name in wanted if name not in self.known_tools]

    def list_models_argv(self) -> list[str]:
        """How to ask this agent what models it can reach, or `[]` if it cannot.

        `lmloop models` used to shell out to `pi --list-models` whatever agent
        was configured, so an omp or opencode setup was shown pi's catalogue --
        or pi's error, on a machine where pi is not installed at all.

        This is the question asked *for a person*: what it runs prints a table
        meant for eyes, and `lmloop models` passes it through untouched.  For
        the same catalogue as data, see `catalogue`.  Which of the two an agent
        can answer is one capability, so `[]` here means it can answer neither.
        """
        return []

    def catalogue(self) -> list[str]:
        """Every model selector this agent will accept, asked of the agent.

        The same question as `list_models_argv`, in a form that can be parsed.
        Separate because the printable answer is not the parseable one: omp's
        is a box-drawing table, and reading it with the column parser pi's
        output invites yields its two provider headers -- `9router/(97)` and
        `llama-swap/(7)`, both offered by the dashboard as models, neither of
        which exists.  Which is the failure the whole thing exists to prevent:
        offering a model the agent cannot resolve produces a run that dies on
        its first request, minutes later, for a reason nobody can see from
        where they picked it.

        `OSError`, `ValueError` and `SubprocessError` are deliberately not
        caught here.  "The agent could not be run" and "the agent knows no
        models" are different answers, and only the caller knows how to say
        either one.
        """
        return []

    def loaded_extensions(self) -> list[str]:
        """Everything loaded into this agent that could gate what a run does.

        Not one kind of thing.  pi loads files from `<config_dir>/extensions`
        *and* npm packages named in its `settings.json`, and reporting only the
        first is how `@vtstech/pi-security` stayed invisible: it blocks `git`
        outright in a mode it defaults to when nobody has chosen one, and
        `lmloop doctor` named two inert files in its place while a real run
        spent iterations working around a `git` it was never going to be
        allowed to run.

        Reported, not judged.  `model-catalog.js` is an extension too and
        lmloop does not work without it; which of these should be loaded during
        an unattended run is the operator's call, and they can only make it if
        something says what is there.
        """
        if not self.config_dir:
            return []
        folder = Path(self.config_dir) / "extensions"
        files = sorted(
            path.name for path in folder.iterdir()
            if path.suffix in (".js", ".ts", ".mjs") and not path.name.endswith(".bak")
        ) if folder.is_dir() else []
        return files + self.loaded_packages()

    def loaded_packages(self) -> list[str]:
        """Packages this agent loads by name rather than by file.

        Empty for an agent that has no such list -- omp keeps a `config.yml`,
        which is YAML, which this project has no parser for and does not need
        one for: omp's extensions are files like anybody else's.
        """
        return []

    def declared_windows(self) -> dict[str, tuple[int, int]]:
        """`(context, max_output)` per full model selector, from this agent's
        own catalogue.  Empty when the agent cannot be asked.

        For any model lmloop cannot measure itself -- a router, a cloud
        provider -- the agent's catalogue is the authority, because it is what
        the agent will actually build its request against.  It has to come from
        the adapter because each agent keeps its own: pi reads
        `~/.pi/agent/models.json`, omp has a separate config directory
        (`~/.omp/agent`) and a much larger catalogue, and asking pi's file on
        omp's behalf answered for four models out of ninety-seven.

        Callers cache this -- see `models.declared_window`.  An implementation
        is allowed to be slow.
        """
        return {}

    def compaction_summary(self, event: dict) -> str:
        """The summary the agent wrote for itself when its context overflowed.

        Empty for agents that do not expose one; the loop then falls back to
        synthesising a handoff from git, which is worse but never wrong.
        """
        return ""
