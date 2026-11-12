"""Adapter for opencode (v2) JSON event streams."""

from __future__ import annotations

import json
import shutil
import subprocess

from harness_base import MESSAGE_END, TOOL, Harness, _tail

# The per-run MCP server that is `browser` for opencode, which has none built in.
BROWSER_MCP = "lmloopbrowser"

# lmloop tool name -> the opencode permission keys that grant it.  Every run
# starts from `"*": "deny"`, so anything not granted here is unavailable.
# `inspect_image` is `read` (it hands images to the model) plus a model that
# accepts them; see `unmet_requirements`.
TOOL_PERMISSIONS = {
    "read": ("read",), "write": ("edit",), "edit": ("edit",), "replace": ("edit",),
    "bash": ("shell",), "grep": ("grep",), "glob": ("glob",), "find": ("glob",), "ls": ("glob",),
    "webfetch": ("webfetch",), "websearch": ("websearch",),
    "browser": (f"{BROWSER_MCP}_*",), "inspect_image": ("read",),
}


def _browser_command() -> list[str]:
    # ponytail: pinned MCP fetched by npx on first use; vendor it if runs must work offline.
    command = ["npx", "-y", "chrome-devtools-mcp@1.10.1", "--headless", "--isolated", "--no-usage-statistics"]
    chromium = shutil.which("chromium") or shutil.which("google-chrome")
    return command + (["--executablePath", chromium] if chromium else [])


class OpencodeHarness(Harness):
    """opencode v2, via `run --format json`.

    Written against captured output rather than documentation.  Its stream is
    structured quite differently from pi's: events carry a `part`, tool calls
    arrive as one `tool_use` with the result already attached rather than as a
    start/end pair, and token usage rides on `step_finish` instead of a message.

    Two consequences worth knowing.  It exposes no compaction event, so the
    summary harvest is unavailable and an overflowing iteration falls back to a
    git-synthesised handoff.  And a step's `reason` is the closest thing it has
    to a stop reason -- `tool-calls` between steps, `stop` at the end -- so only
    the final one is meaningful.

    The tool allowlist is a per-run agent, `lmloop`, delivered in
    `OPENCODE_CONFIG_CONTENT` (see `run_env`).  `--standalone` matters: without
    it `run` attaches to the background server, whose config never sees that
    variable.  Measured on 2.2.7 against a stub model: under `"*": "deny"` only
    granted tools reach the model, plus `skill`, which is offered but refuses
    every call ("Permission denied: skill").  Global MCP servers, plugin tools
    and a project's own opencode.json/plugins do not get through.
    """

    name = "opencode"
    binary = "opencode"
    # Its tool events arrive with the result already attached -- one `tool_use`
    # per call, never a start and an end -- so a call is never observably in
    # flight and `tool_seconds` cannot apply.
    reports_tool_ends = False
    env_passthrough = ("OPENCODE_*",)
    interesting = ('"tool_use"', '"step_finish"')
    activity = (b'"text"', b'"tool_use"', b'"step_')
    known_tools = frozenset(TOOL_PERMISSIONS)
    verifies_required_tools = True

    def argv(self, *, model, tools, thinking, session_dir, session_id, system_prompt="", resume_session=""):
        # opencode keeps its own sessions, so session_dir has nowhere to go.
        # It has no system-prompt flag either; pi_runner prepends
        # `system_prompt` to the prompt for it.
        argv = [self.binary, "run", "--format", "json"]
        if tools.strip():
            argv += ["--standalone", "--archived", "--agent", "lmloop"]
        if model:
            # v2 dropped `--variant`; the variant rides on the selector.
            argv += ["--model", f"{model}#{thinking}" if thinking else model]
        return argv

    def run_env(self, tools):
        names = [name.strip() for name in tools.split(",") if name.strip()]
        if not names:
            return {}
        permission = {"*": "deny"}
        for name in names:
            for key in TOOL_PERMISSIONS.get(name, ()):
                # `edit` also grants opencode's built-in `worktree_remove`, which
                # no permission key switches off.  It refuses worktrees other
                # than its own; denying edit on "." stops it removing the one
                # the run lives in, and files inside still edit.
                permission[key] = {"*": "allow", ".": "deny"} if key == "edit" else "allow"
        config = {"agent": {"lmloop": {
            "mode": "primary", "description": "lmloop iteration", "permission": permission,
        }}}
        if "browser" in names:
            config["mcp"] = {BROWSER_MCP: {"type": "local", "command": _browser_command()}}
        return {"OPENCODE_CONFIG_CONTENT": json.dumps(config), "OPENCODE_DISABLE_PROJECT_CONFIG": "1"}

    def unmet_requirements(self, model, required):
        if "inspect_image" not in required:
            return []
        if not model:
            return ["inspect_image needs an explicit [agent] model to check for image input"]
        selector = model.split("#", 1)[0]
        try:
            models = self._model_list()
        except (OSError, subprocess.SubprocessError, ValueError, KeyError) as error:
            return [f"inspect_image: could not ask opencode about {selector}: {type(error).__name__}"]
        entry = next((m for m in models if f"{m.get('providerID')}/{m.get('id')}" == selector), None)
        if entry is None:
            return [f"inspect_image: opencode does not know model {selector}"]
        # opencode assumes image input for a custom model that declares no
        # modalities, so this trusts provider config, not a live probe.
        if "image" not in (entry.get("capabilities") or {}).get("input", []):
            return [f"inspect_image: {selector} does not accept image input"]
        return []

    def _model_list(self) -> list[dict]:
        result = subprocess.run(
            [self.binary, "api", "--standalone", "model.list"],
            capture_output=True, text=True, timeout=60,
        )
        if result.returncode:
            raise subprocess.CalledProcessError(result.returncode, result.args)
        return json.loads(result.stdout)["data"]

    def list_models_argv(self):
        return [self.binary, "models"]

    def catalogue(self):
        result = subprocess.run(
            [self.binary, "models"], capture_output=True, text=True, timeout=60,
        )
        if result.returncode:
            raise subprocess.CalledProcessError(result.returncode, result.args, result.stdout, result.stderr)
        return [line.strip() for line in result.stdout.splitlines() if line.strip()]

    def classify(self, event):
        kind = event.get("type")
        part = event.get("part") or {}
        if kind == "tool_use":
            state = part.get("state") or {}
            tool_input = state.get("input") or {}
            return {
                "kind": TOOL,
                "name": part.get("tool", ""),
                "target": self._target(tool_input),
                "identity": json.dumps(tool_input, sort_keys=True, separators=(",", ":")),
                # v1 spelled it `filePath`; v2 (2.2.x) sends `path`.
                "path": tool_input.get("filePath") or tool_input.get("path"),
            }
        if kind == "step_finish":
            tokens = part.get("tokens") or {}
            return {
                "kind": MESSAGE_END,
                "stop_reason": part.get("reason") or "",
                "error": "",
                "input": int(tokens.get("input") or 0),
                "output": int(tokens.get("output") or 0),
            }
        return None

    @staticmethod
    def _target(args: dict) -> str:
        for key in ("filePath", "path", "file"):
            value = args.get(key)
            if isinstance(value, str) and value:
                return _tail(value)
        command = args.get("command")
        if isinstance(command, str) and command:
            return " ".join(command.split())[:60]
        pattern = args.get("pattern") or args.get("query")
        return pattern[:40] if isinstance(pattern, str) else ""
