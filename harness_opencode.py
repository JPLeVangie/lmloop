"""Adapter for opencode JSON event streams."""

from __future__ import annotations

import json
import subprocess

from harness_base import MESSAGE_END, TOOL, Harness, _tail

class OpencodeHarness(Harness):
    """opencode, via `run --format json`.

    Written against captured output rather than documentation.  Its stream is
    structured quite differently from pi's: events carry a `part`, tool calls
    arrive as one `tool_use` with the result already attached rather than as a
    start/end pair, and token usage rides on `step_finish` instead of a message.

    Two consequences worth knowing.  It exposes no compaction event, so the
    summary harvest is unavailable and an overflowing iteration falls back to a
    git-synthesised handoff.  And a step's `reason` is the closest thing it has
    to a stop reason -- `tool-calls` between steps, `stop` at the end -- so only
    the final one is meaningful.
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

    def argv(self, *, model, tools, thinking, session_dir, session_id, system_prompt="", resume_session=""):
        # opencode keeps its own sessions and takes no tool allowlist, so
        # session_dir and tools have nowhere to go.  Saying so is better than
        # passing flags it will reject.  It has no system-prompt flag either;
        # pi_runner prepends `system_prompt` to the prompt for it.
        argv = [self.binary, "run", "--format", "json"]
        if model:
            argv += ["--model", model]
        if thinking:
            argv += ["--variant", thinking]
        return argv

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
