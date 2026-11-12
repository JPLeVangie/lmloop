"""What each bundled adapter must make of its agent's real output.

The fixtures in `testdata/` are captured from actual runs -- pi from an
archived run, omp from a live worktree -- and then redacted by a default-deny
pass that replaces every string whose key is not structural, so no source
repository content survives.  What is left is the shape: event names, field
names, roles, stop reasons, and the token counts, which is exactly the part
an adapter has to keep agreeing with.

The point of capturing rather than inventing: an invented event asserts what
the adapter already believes.  These have caught nothing yet because they are
new, but the class of bug they exist for -- an agent renaming a field, or the
two forks drifting apart -- is one this project has already paid for twice
(`auto_compaction_*` vs `compaction_*`, and `--list-models`).
"""

import json
import tempfile
import unittest
import unittest.mock
from pathlib import Path

import harness
import harness_omp
import harness_opencode
import harness_pi

TESTDATA = Path(__file__).parent.parent / "testdata"


class CapturedEvents:
    """Ordered captured events, retaining repeated keys rather than overwriting."""

    def __init__(self):
        self._events = []

    def add(self, key, event):
        self._events.append((key, event))

    def __contains__(self, key):
        return any(name == key for name, _event in self._events)

    def __getitem__(self, key):
        for name, event in self._events:
            if name == key:
                return event
        raise KeyError(key)

    def items(self):
        return iter(self._events)

    def __len__(self):
        return len(self._events)


def load(name, *, directory=TESTDATA):
    """The captured stream in wire order, including duplicate event keys."""
    events = CapturedEvents()
    for line in (directory / name).read_text().splitlines():
        if not line.strip():
            continue
        event = json.loads(line)
        kind = event.get("type")
        if kind == "message_end":
            message = event.get("message") or {}
            role = message.get("role")
            key = (f"message_end:assistant:{message.get('stopReason')}"
                   if role == "assistant" else f"message_end:{role}")
        else:
            key = kind
        events.add(key, event)
    return events


class CapturedEventContractTests(unittest.TestCase):
    def test_loader_preserves_order_and_duplicate_event_keys(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory) / "events.jsonl"
            fixture.write_text("{\"type\": \"turn_start\", \"n\": 1}\n"
                               "{\"type\": \"turn_start\", \"n\": 2}\n")
            events = load(fixture.name, directory=Path(directory))

        self.assertEqual(2, len(events))
        self.assertEqual(["turn_start", "turn_start"],
                         [key for key, _event in events.items()])
        self.assertEqual([1, 2], [event["n"] for _key, event in events.items()])

    CASES = (("pi", "pi-events.jsonl", "compaction_start"),
             ("omp", "omp-events.jsonl", "auto_compaction_start"))

    def test_a_tool_call_is_normalised_to_kind_name_and_target(self):
        for agent, fixture, _ in self.CASES:
            with self.subTest(agent=agent):
                adapter = harness.get(agent)
                event = load(fixture)["tool_execution_start"]
                result = adapter.classify(event)
                self.assertEqual(harness.TOOL, result["kind"])
                self.assertEqual(event["toolName"], result["name"])
                self.assertTrue(result["target"], "a tool call with args must have a target")
                self.assertIn("path", result)

    def test_an_assistant_turn_carries_its_stop_reason_and_token_counts(self):
        for agent, fixture, _ in self.CASES:
            with self.subTest(agent=agent):
                adapter = harness.get(agent)
                event = load(fixture)["message_end:assistant:toolUse"]
                result = adapter.classify(event)
                self.assertEqual(harness.MESSAGE_END, result["kind"])
                self.assertEqual("toolUse", result["stop_reason"])
                usage = event["message"]["usage"]
                self.assertEqual(usage["input"], result["input"])
                self.assertEqual(usage["output"], result["output"])
                self.assertGreater(result["input"], 0, "captured usage should be real")

    def test_every_captured_stop_reason_survives_classification(self):
        """`length` is the one that matters most: it is how an overflow shows
        up, and an adapter that dropped it would make a truncated turn look
        like a clean one."""
        for agent, fixture, _ in self.CASES:
            adapter = harness.get(agent)
            events = load(fixture)
            for key, event in events.items():
                if not key.startswith("message_end:assistant:"):
                    continue
                with self.subTest(agent=agent, variant=key):
                    result = adapter.classify(event)
                    self.assertEqual(key.rsplit(":", 1)[1], result["stop_reason"])

    def test_a_tool_call_finishing_is_reported_as_such(self):
        """Paired with the start so the loop can tell "a tool call is still
        running" from "the model is thinking" -- both are silence from
        outside, and only one of them is a hung subprocess (lm-8l4)."""
        for agent, fixture, _ in self.CASES:
            with self.subTest(agent=agent):
                adapter = harness.get(agent)
                event = load(fixture)["tool_execution_end"]
                self.assertEqual({"kind": harness.TOOL_END}, adapter.classify(event))

    def test_a_tool_result_is_not_mistaken_for_an_assistant_turn(self):
        """Both arrive as `message_end`.  Counting a `toolResult` as a turn
        would multiply the token totals by the number of tools called."""
        for agent, fixture, _ in self.CASES:
            with self.subTest(agent=agent):
                adapter = harness.get(agent)
                events = load(fixture)
                self.assertIsNone(adapter.classify(events["message_end:toolResult"]))
                self.assertIsNone(adapter.classify(events["message_end:user"]))

    def test_each_adapter_recognises_only_its_own_compaction_event(self):
        """The one that has actually bitten: omp names it `auto_compaction_*`
        where pi names it `compaction_*`, and an adapter hardcoded to either
        harvests nothing from the other while saying nothing about it."""
        for agent, fixture, own in self.CASES:
            with self.subTest(agent=agent):
                adapter = harness.get(agent)
                self.assertEqual(own, adapter.compaction_event)
                event = load(fixture)[own]
                self.assertEqual({"kind": harness.COMPACTION}, adapter.classify(event))

        # And crossed over, the other agent's name means nothing.
        self.assertIsNone(
            harness.get("pi").classify(load("omp-events.jsonl")["auto_compaction_start"]),
        )
        self.assertIsNone(
            harness.get("omp").classify(load("pi-events.jsonl")["compaction_start"]),
        )

    def test_bookkeeping_events_classify_to_nothing(self):
        for agent, fixture, _ in self.CASES:
            adapter = harness.get(agent)
            events = load(fixture)
            for key in ("agent_start", "turn_start"):
                if key not in events:
                    continue
                with self.subTest(agent=agent, event=key):
                    self.assertIsNone(adapter.classify(events[key]))

    def test_classify_never_raises_on_any_captured_event(self):
        """It runs on every line of a stream that reaches tens of megabytes."""
        for agent, fixture, _ in self.CASES:
            adapter = harness.get(agent)
            for key, event in load(fixture).items():
                with self.subTest(agent=agent, event=key):
                    adapter.classify(event)


class StreamFilterContractTests(unittest.TestCase):
    """`interesting` and `activity` run against raw bytes before anything is
    parsed, so a name that drifts here silently stops the loop seeing events
    it still handles perfectly well once they arrive."""

    CASES = (("pi", "pi-events.jsonl"), ("omp", "omp-events.jsonl"))

    def test_every_event_the_adapter_classifies_survives_the_interesting_filter(self):
        for agent, fixture in self.CASES:
            adapter = harness.get(agent)
            for key, event in load(fixture).items():
                if adapter.classify(event) is None:
                    continue
                with self.subTest(agent=agent, event=key):
                    line = json.dumps(event)
                    self.assertTrue(
                        any(marker in line for marker in adapter.interesting),
                        f"{key} is classified but would never be parsed",
                    )

    def test_the_activity_markers_appear_in_real_output(self):
        """They are what tells "the model is still loading" from "it hung"."""
        for agent, fixture in self.CASES:
            adapter = harness.get(agent)
            blob = (TESTDATA / fixture).read_bytes()
            with self.subTest(agent=agent):
                self.assertTrue(adapter.activity, "an adapter needs a stall signal")
                for marker in adapter.activity:
                    self.assertIn(marker, blob)

    def test_each_compaction_marker_appears_in_its_own_streams_bytes(self):
        """Scanned against raw bytes before parsing, so it has to match the
        wire form and not the name someone remembered."""
        for agent, fixture in self.CASES:
            with self.subTest(agent=agent):
                marker = harness.get(agent).compaction_marker
                self.assertTrue(marker, "both bundled agents compact")
                self.assertIn(marker, (TESTDATA / fixture).read_bytes())

    def test_neither_compaction_marker_matches_the_other_agents_stream(self):
        """The leading quote in each marker is load-bearing, in both
        directions.  `"auto_compaction_end"` contains `compaction_end`, so
        without its quote pi's marker fires on omp's stream; and without its
        own quote omp's marker (`compaction_end"`) fires on pi's.  Either way
        one agent harvests a summary out of a stream that is not its own.
        """
        streams = {agent: (TESTDATA / fixture).read_bytes()
                   for agent, fixture in self.CASES}
        for agent, _ in self.CASES:
            marker = harness.get(agent).compaction_marker
            for other, blob in streams.items():
                if other == agent:
                    continue
                with self.subTest(marker=agent, stream=other):
                    self.assertNotIn(marker, blob)


class CompactionSummaryContractTests(unittest.TestCase):
    """The summary an agent writes for itself on the way out of an overflow.

    Worth a captured test rather than an invented one: it is buried several
    levels into the event, the two agents put it in differently named events,
    and when the harvest misses the loop silently falls back to a
    git-synthesised handoff -- worse, but never obviously broken.
    """

    def test_each_agent_harvests_its_summary_from_its_own_event(self):
        for agent, fixture, event_name in (
            ("pi", "pi-events.jsonl", "compaction_end"),
            ("omp", "omp-events.jsonl", "auto_compaction_end"),
        ):
            with self.subTest(agent=agent):
                event = load(fixture)[event_name]
                # The fixture's summary is redacted to a placeholder, so a
                # non-empty answer means the adapter found the right field.
                self.assertEqual("<summary>",
                                 harness.get(agent).compaction_summary(event))

    def test_an_event_with_no_summary_harvests_nothing_rather_than_raising(self):
        for agent in ("pi", "omp"):
            with self.subTest(agent=agent):
                self.assertEqual("", harness.get(agent).compaction_summary({}))


class DeclaredWindowContractTests(unittest.TestCase):
    """Where each agent's model catalogue comes from.

    For anything lmloop cannot measure itself, the agent's own catalogue is the
    authority -- and it has to be *that* agent's.  Reading pi's file whatever
    the agent was configured answered for four models where omp knows
    ninety-seven, and every other one came back with no window at all.
    """

    def test_pi_reads_its_own_models_file(self):
        payload = json.dumps({"providers": {"9router": {"models": [
            {"id": "agent-default", "contextWindow": 262144, "maxTokens": 32768},
        ]}}})
        adapter = harness.get("pi")
        with unittest.mock.patch.object(
            type(adapter), "models_file", _FakeFile(payload),
        ):
            self.assertEqual({"9router/agent-default": (262144, 32768)},
                             adapter.declared_windows())

    def test_pi_survives_a_missing_or_broken_models_file(self):
        adapter = harness.get("pi")
        for content in (OSError("gone"), "{not json"):
            with self.subTest(content=str(content)[:20]):
                with unittest.mock.patch.object(
                    type(adapter), "models_file", _FakeFile(content),
                ):
                    self.assertEqual({}, adapter.declared_windows())

    def test_pi_skips_entries_with_no_usable_numbers(self):
        payload = json.dumps({"providers": {"p": {"models": [
            {"id": "good", "contextWindow": 100, "maxTokens": 10},
            {"id": "no-output", "contextWindow": 100},
            {"id": "text-numbers", "contextWindow": "100", "maxTokens": "10"},
        ]}}})
        adapter = harness.get("pi")
        with unittest.mock.patch.object(
            type(adapter), "models_file", _FakeFile(payload),
        ):
            self.assertEqual({"p/good": (100, 10)}, adapter.declared_windows())

    def test_omp_asks_omp_rather_than_reading_pis_file(self):
        """The bug this replaced: `OmpHarness` extends `PiHarness`, so it
        inherited a reader pointed at a config directory omp does not use."""
        payload = json.dumps({"models": [
            {"selector": "9router/xmtp/mimo-v2.5",
             "contextWindow": 1048576, "maxTokens": 131072},
        ]})
        completed = unittest.mock.Mock(stdout=payload)
        with unittest.mock.patch.object(
            harness_omp.subprocess, "run", return_value=completed,
        ) as run:
            windows = harness.get("omp").declared_windows()
        self.assertEqual({"9router/xmtp/mimo-v2.5": (1048576, 131072)}, windows)
        self.assertEqual(["omp", "models", "--json"], run.call_args.args[0])

    def test_omp_survives_not_being_installed_or_answering_rubbish(self):
        """A run with no window metadata still runs, so this can never raise."""
        for failure in (OSError("no such binary"),
                        harness_omp.subprocess.TimeoutExpired("omp", 60)):
            with self.subTest(failure=type(failure).__name__):
                with unittest.mock.patch.object(
                    harness_omp.subprocess, "run", side_effect=failure,
                ):
                    self.assertEqual({}, harness.get("omp").declared_windows())
        with unittest.mock.patch.object(
            harness_omp.subprocess, "run", return_value=unittest.mock.Mock(stdout="not json"),
        ):
            self.assertEqual({}, harness.get("omp").declared_windows())

    def test_an_agent_with_no_catalogue_says_so_rather_than_guessing(self):
        self.assertEqual({}, harness.get("opencode").declared_windows())

    def test_opencode_lists_full_model_selectors_from_lines(self):
        output = "9router/agent-local\nprovider/model/variant [local]\nfoo@bar:baz\n"
        with unittest.mock.patch.object(
            harness_opencode.subprocess, "run",
            return_value=unittest.mock.Mock(returncode=0, args=["opencode", "models"], stdout=output),
        ) as run:
            self.assertEqual(
                ["9router/agent-local", "provider/model/variant [local]", "foo@bar:baz"],
                harness.get("opencode").catalogue(),
            )
        self.assertEqual(["opencode", "models"], run.call_args.args[0])
        self.assertEqual(60, run.call_args.kwargs["timeout"])

    def test_opencode_model_listing_failure_is_not_an_empty_catalogue(self):
        with unittest.mock.patch.object(
            harness_opencode.subprocess, "run",
            return_value=unittest.mock.Mock(returncode=1, args=["opencode", "models"], stdout="", stderr="failed"),
        ):
            with self.assertRaises(harness_opencode.subprocess.CalledProcessError):
                harness.get("opencode").catalogue()

    def test_opencode_exposes_models_command_to_cli(self):
        self.assertEqual(["opencode", "models"], harness.get("opencode").list_models_argv())


class _FakeFile:
    """Stands in for a `Path` that `declared_windows` only ever reads."""

    def __init__(self, content):
        self._content = content

    def read_text(self):
        if isinstance(self._content, Exception):
            raise self._content
        return self._content


def load_opencode():
    """opencode's stream is shaped differently: no `message_end`, tool calls
    arrive as one `tool_use` with the result attached, and a step's `reason` is
    the closest thing it has to a stop reason."""
    events = CapturedEvents()
    for line in (TESTDATA / "opencode-events.jsonl").read_text().splitlines():
        if not line.strip():
            continue
        event = json.loads(line)
        part = event.get("part") or {}
        kind = event.get("type")
        if kind == "tool_use":
            key = f"tool_use:{(part.get('state') or {}).get('status')}"
        elif kind == "step_finish":
            key = f"step_finish:{part.get('reason')}"
        else:
            key = kind
        events.add(key, event)
    return events


class OpencodeCapturedEventTests(unittest.TestCase):
    """Captured from a real opencode run against a local llama-swap.

    Kept apart from the pi/omp cases because opencode shares none of their
    vocabulary -- which is the reason to capture it rather than assume the
    adapter's own view of it is right.
    """

    def test_a_tool_call_is_normalised_like_any_other_agents(self):
        adapter = harness.get("opencode")
        event = load_opencode()["tool_use:completed"]
        result = adapter.classify(event)
        self.assertEqual(harness.TOOL, result["kind"])
        self.assertEqual("write", result["name"])
        self.assertEqual("src/example.py", result["path"])
        self.assertTrue(result["target"])

    def test_v2_spells_the_file_argument_path(self):
        """opencode 2.2.x sends `path` where 1.x sent `filePath` (captured
        from a real 2.2.7 `read` call); missing it loses every write's file."""
        event = {"type": "tool_use", "part": {"tool": "write", "state": {
            "status": "completed", "input": {"path": "/tmp/oc-probe/index.html"}}}}
        result = harness.get("opencode").classify(event)
        self.assertEqual("/tmp/oc-probe/index.html", result["path"])
        self.assertEqual("index.html", result["target"])

    def test_an_allowlist_becomes_a_deny_all_per_run_agent(self):
        adapter = harness.get("opencode")
        common = dict(model="p/m", thinking="high", session_dir=Path("/s"), session_id="x")
        argv = adapter.argv(tools="read,edit,bash", **common)
        self.assertEqual("lmloop", argv[argv.index("--agent") + 1])
        self.assertIn("--standalone", argv)  # the background server never sees the env config
        self.assertEqual("p/m#high", argv[argv.index("--model") + 1])  # v2 has no --variant
        self.assertNotIn("--variant", argv)
        env = adapter.run_env("read,edit,bash")
        self.assertEqual("1", env["OPENCODE_DISABLE_PROJECT_CONFIG"])
        agent = json.loads(env["OPENCODE_CONFIG_CONTENT"])["agent"]["lmloop"]
        self.assertEqual({"*": "deny", "read": "allow", "shell": "allow",
                          "edit": {"*": "allow", ".": "deny"}}, agent["permission"])
        self.assertNotIn("mcp", json.loads(env["OPENCODE_CONFIG_CONTENT"]))
        self.assertEqual({}, adapter.run_env(""))

    def test_browser_is_a_per_run_mcp_server_granted_by_prefix(self):
        config = json.loads(harness.get("opencode").run_env("read,browser")["OPENCODE_CONFIG_CONTENT"])
        server = harness_opencode.BROWSER_MCP
        self.assertEqual("allow", config["agent"]["lmloop"]["permission"][f"{server}_*"])
        self.assertIn("--isolated", config["mcp"][server]["command"])

    def test_a_step_carries_its_reason_and_token_counts(self):
        adapter = harness.get("opencode")
        for variant, reason in (("step_finish:stop", "stop"),
                                ("step_finish:tool-calls", "tool-calls")):
            with self.subTest(variant=variant):
                event = load_opencode()[variant]
                result = adapter.classify(event)
                self.assertEqual(harness.MESSAGE_END, result["kind"])
                self.assertEqual(reason, result["stop_reason"])
                tokens = event["part"]["tokens"]
                self.assertEqual(tokens["input"], result["input"])
                self.assertEqual(tokens["output"], result["output"])

    def test_tokens_come_from_the_step_not_a_message(self):
        """opencode puts usage on `step_finish`; pi and omp put it on an
        assistant `message_end`.  An adapter that looked for a message here
        would report zero tokens for every iteration."""
        result = harness.get("opencode").classify(load_opencode()["step_finish:stop"])
        self.assertGreater(result["input"], 0)
        self.assertGreater(result["output"], 0)

    def test_bookkeeping_events_classify_to_nothing(self):
        adapter = harness.get("opencode")
        for key in ("step_start", "text"):
            with self.subTest(event=key):
                self.assertIsNone(adapter.classify(load_opencode()[key]))

    def test_it_exposes_no_compaction_and_says_so(self):
        """No compaction event means the summary harvest is unavailable and an
        overflowing iteration falls back to a git-synthesised handoff."""
        adapter = harness.get("opencode")
        self.assertEqual("", adapter.compaction_event)
        self.assertEqual(b"", adapter.compaction_marker)
        self.assertEqual("", adapter.compaction_summary(load_opencode()["step_finish:stop"]))

    def test_everything_it_classifies_survives_the_interesting_filter(self):
        adapter = harness.get("opencode")
        for key, event in load_opencode().items():
            if adapter.classify(event) is None:
                continue
            with self.subTest(event=key):
                self.assertTrue(
                    any(m in json.dumps(event) for m in adapter.interesting),
                    f"{key} is classified but would never be parsed",
                )

    def test_its_activity_markers_appear_in_real_output(self):
        blob = (TESTDATA / "opencode-events.jsonl").read_bytes()
        for marker in harness.get("opencode").activity:
            with self.subTest(marker=marker):
                self.assertIn(marker, blob)

    def test_classify_never_raises_on_any_captured_event(self):
        adapter = harness.get("opencode")
        for key, event in load_opencode().items():
            with self.subTest(event=key):
                adapter.classify(event)


class CatalogueTests(unittest.TestCase):
    """What each adapter makes of its agent's real catalogue output.

    Captured, for the same reason as the event streams: the bug these exist
    for is one where the code's belief and the agent's output had diverged and
    nothing said so.  `web/server.py` read every agent's catalogue with pi's
    column parser, so `omp models` -- a provider header and then a box-drawing
    table -- came back as two models named after the provider counts, and the
    API reported them as omp's own catalogue.
    """

    def stdout(self, name):
        return (TESTDATA / name).read_text()

    def run_with(self, adapter, stdout):
        """The adapter's catalogue, with the agent's real output handed back."""
        module = harness_omp if adapter.name == "omp" else harness_pi
        with unittest.mock.patch.object(
            module.subprocess, "run",
            return_value=unittest.mock.Mock(stdout=stdout),
        ):
            return adapter.catalogue()

    def test_pi_reads_its_own_columns(self):
        models = self.run_with(harness.get("pi"), self.stdout("pi-models.txt"))
        self.assertGreater(len(models), 50, models)
        self.assertNotIn("provider/model", models, "the header is not a model")
        for model in models:
            with self.subTest(model=model):
                self.assertIn("/", model)

    def test_omp_reads_the_json_and_not_the_table(self):
        models = self.run_with(harness.get("omp"), self.stdout("omp-models.json"))
        self.assertGreater(len(models), 50, models)
        for model in models:
            with self.subTest(model=model):
                self.assertIn("/", model)

    def test_omp_knows_far_more_models_than_its_table_has_rows_of_header(self):
        """The measured shape of the bug: the printable answer yields two."""
        models = self.run_with(harness.get("omp"), self.stdout("omp-models.json"))
        self.assertGreater(len(models), 90, len(models))

    def test_pis_parser_over_omps_table_is_the_bug_this_replaced(self):
        """Not a claim about today's code -- a record of what the shared parser
        did, so the reason omp has its own is checkable rather than asserted."""
        wrong = harness.get("pi").parse_catalogue(self.stdout("omp-models.txt"))
        self.assertTrue(all("(" in model for model in wrong), wrong)
        self.assertLess(len(wrong), 5, wrong)

    def test_every_model_omp_offers_is_one_it_also_declares_a_window_for(self):
        """Both come from the same `omp models --json`, and a selector in one
        and not the other means the two readers have drifted apart."""
        stdout = self.stdout("omp-models.json")
        adapter = harness.get("omp")
        with unittest.mock.patch.object(
            harness_omp.subprocess, "run",
            return_value=unittest.mock.Mock(stdout=stdout),
        ):
            self.assertEqual(set(adapter.catalogue()), set(adapter.declared_windows()))

    def test_opencode_can_list_its_catalogue(self):
        self.assertEqual(["opencode", "models"], harness.get("opencode").list_models_argv())
        with unittest.mock.patch.object(
            harness_opencode.subprocess, "run",
            return_value=unittest.mock.Mock(returncode=0, args=["opencode", "models"], stdout=""),
        ):
            self.assertEqual([], harness.get("opencode").catalogue())

    def test_a_failure_to_run_reaches_the_caller(self):
        """The dashboard tells "could not be run" from "knows no models" apart,
        and can only do that if the adapter does not swallow the difference."""
        for agent in ("pi", "omp"):
            module = harness_omp if agent == "omp" else harness_pi
            with self.subTest(agent=agent), \
                 unittest.mock.patch.object(module.subprocess, "run", side_effect=OSError):
                with self.assertRaises(OSError):
                    harness.get(agent).catalogue()

    def test_omp_answering_with_something_other_than_json_reaches_the_caller(self):
        with unittest.mock.patch.object(
            harness_omp.subprocess, "run",
            return_value=unittest.mock.Mock(stdout="Error: unknown flag: --json"),
        ):
            with self.assertRaises(ValueError):
                harness.get("omp").catalogue()


if __name__ == "__main__":
    unittest.main()
