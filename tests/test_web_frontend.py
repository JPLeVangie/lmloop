"""The dashboard's vocabulary, kept in step with the runner's.

The frontend is served as static files, so nothing imports it and nothing type
checks it against the API it renders. That makes one kind of drift invisible:
the runner grows an outcome, the dashboard does not know the word, and a
failure renders as neutral -- which is worse than not rendering at all,
because it reads as "fine".

That is not hypothetical. `tool-timeout` was added to `pi_runner` in this
project's own history and appeared in the dashboard as an unstyled cell and a
grey pip until this test was written.
"""

import json
import re
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
APP = (ROOT / "web" / "static" / "app.js").read_text()
CSS = (ROOT / "web" / "static" / "style.css").read_text()
RUNNER = (ROOT / "pi_runner.py").read_text()
SERVER = (ROOT / "web" / "server.py").read_text()
HTML = (ROOT / "web" / "static" / "index.html").read_text()


def code_only(source: str) -> str:
    """`source` with its comments removed.

    The same move `test_lmloop.py` makes on every `git` argv, for the same
    reason: a check on the text of a file must not be answerable by prose.
    Here it runs the other way -- a comment recording what a number *used* to
    be would otherwise fail a test asserting the number is gone.
    """
    source = re.sub(r"/\*.*?\*/", "", source, flags=re.S)
    return "\n".join(line for line in source.splitlines()
                     if not line.lstrip().startswith("//"))


def runner_outcomes():
    """Every outcome `pi_runner` can hand back, read from the source.

    Read rather than listed, so a new one is picked up by the test that is
    supposed to notice new ones.
    """
    found = set(re.findall(r'outcome, detail = "([a-z-]+)"', RUNNER))
    found |= set(re.findall(r'outcome = "([a-z-]+)"', RUNNER))
    return found


class OutcomeVocabularyTests(unittest.TestCase):
    def test_the_runner_produces_the_outcomes_this_test_thinks_it_does(self):
        """A guard on the guard: if the source stops matching these shapes,
        everything below would pass by finding nothing."""
        outcomes = runner_outcomes()
        self.assertGreaterEqual(len(outcomes), 8, outcomes)
        for expected in ("ok", "timeout", "tool-timeout", "agent-error"):
            self.assertIn(expected, outcomes)

    def test_every_outcome_has_a_class_in_the_dashboard(self):
        block = re.search(r"const OUTCOME_CLASS = \{(.*?)\};", APP, re.S)
        self.assertIsNotNone(block, "OUTCOME_CLASS moved or was renamed")
        known = set(re.findall(r'"?([a-z-]+)"?\s*:', block.group(1)))
        missing = runner_outcomes() - known
        self.assertEqual(set(), missing,
                         f"outcomes the dashboard would render unstyled: {sorted(missing)}")

    def test_every_failing_outcome_is_visibly_a_failure(self):
        """Neutral is worse than missing: it reads as fine."""
        block = re.search(r"const OUTCOME_CLASS = \{(.*?)\};", APP, re.S).group(1)
        classes = dict(re.findall(r'"?([a-z-]+)"?\s*:\s*"([a-z]*)"', block))
        for outcome in ("stalled", "timeout", "tool-timeout", "agent-error"):
            with self.subTest(outcome=outcome):
                self.assertEqual("bad", classes.get(outcome))
        for outcome in ("thrashing", "looping", "truncated", "no-action"):
            with self.subTest(outcome=outcome):
                self.assertEqual("warn", classes.get(outcome))

    def test_every_failing_outcome_also_colours_its_pip(self):
        """The pips are the only view of a run's history on a phone, and they
        are styled by CSS rather than by the class map above."""
        rules = [line for line in CSS.splitlines() if line.startswith(".pip")]
        bad = " ".join(line for line in rules if "--bad" in line)
        warn = " ".join(line for line in rules if "--warn" in line)
        for outcome, expected in (("stalled", bad), ("timeout", bad),
                                  ("tool-timeout", bad), ("agent-error", bad),
                                  ("thrashing", warn), ("looping", warn), ("truncated", warn),
                                  ("no-action", warn)):
            with self.subTest(outcome=outcome):
                # Either an exact class, or a substring selector naming the
                # whole outcome or one of its words -- the stylesheet uses all
                # three shapes, and which one is a formatting choice.
                candidates = [outcome, *outcome.split("-")]
                matched = (
                    f".pip.{outcome}" in expected
                    or any(f'[class*="{fragment}"]' in expected
                           for fragment in candidates)
                )
                self.assertTrue(matched, f"{outcome} has no colour among: {expected}")


def model_sources():
    """Every answer `available_models` gives for where its list came from.

    Read from the source, like the outcomes above.  The agent's own name is not
    among these -- it is every value that is not one of these, which is what
    makes the set worth keeping in step.
    """
    found = set()
    # To the end of the line rather than to the first `}`: one of these values
    # is an f-string, and its own `{agent}` closes before the string does.
    for value in re.findall(r'"model_source":\s*(.*)', SERVER):
        found |= {literal for literal in re.findall(r'f?"([^"]*)"', value) if literal}
    return found


class ModelSourceVocabularyTests(unittest.TestCase):
    """A list the agent was never asked for renders exactly like its
    catalogue, and the difference is minutes of a run that was doomed at the
    first request.  `model_source` is the API saying which one it handed over;
    the sheet has to know every word it can say."""

    def test_the_api_reports_the_sources_this_test_thinks_it_does(self):
        """A guard on the guard: finding nothing would pass everything."""
        sources = model_sources()
        self.assertGreaterEqual(len(sources), 4, sources)
        self.assertIn("fallback", sources)
        self.assertIn("{agent} cannot list", sources)

    def test_the_sheet_can_explain_every_list_that_is_not_a_catalogue(self):
        block = re.search(r"const MODEL_SOURCE_REASON = \{(.*?)\n\};", APP, re.S)
        self.assertIsNotNone(block, "MODEL_SOURCE_REASON moved or was renamed")
        known = set(re.findall(r'"([^"]+)":', block.group(1)))
        for source in model_sources():
            with self.subTest(source=source):
                if source.startswith("{agent} "):
                    # This one carries the agent's own name, so the sheet
                    # matches it by suffix rather than by lookup.
                    self.assertIn(source[len("{agent} "):], APP)
                else:
                    self.assertIn(source, known)

    def test_the_reason_reaches_a_paragraph_that_exists(self):
        """The whole feature is one element; a renamed id makes it silent."""
        self.assertIn('id="model-source"', HTML)
        self.assertIn('$("model-source").textContent = source', APP)
        self.assertIn('$("model-source").hidden = !source', APP)

    def test_a_real_catalogue_says_nothing(self):
        """`model_source` is the agent's name in the normal case, and the
        normal case was promised nothing new on screen."""
        body = re.search(r"function modelSourceNote\(catalogue\) \{(.*?)\n\}", APP, re.S)
        self.assertIsNotNone(body, "modelSourceNote moved or was renamed")
        self.assertIn('if (!reason) return "";', body.group(1))


def auth_modes():
    """Every deployment `web/auth.py` supports, as `{mode: class}`.

    Read off the classes rather than out of the text: `interactive` is the
    thing being checked below, and a regex for it has to guess where one class
    ends and the next begins -- which it got wrong on the first try, in the
    direction that passes.
    """
    from web import auth as auth_module
    return {
        value.mode: value for value in vars(auth_module).values()
        if isinstance(value, type) and getattr(value, "mode", "")
    }


class AuthModeVocabularyTests(unittest.TestCase):
    """There are three ways to answer "who is asking", and the page knew none.

    The one that hurt is `proxy`, the common deployment: every request carried
    an identity the API had already resolved, and the dashboard showed no name
    and no way out -- correctly no way out, since the proxy owns that session,
    but silently, which reads as a missing button rather than a fact.
    """

    def test_the_server_has_the_modes_this_test_thinks_it_does(self):
        """A guard on the guard: finding none would pass everything."""
        self.assertEqual({"none", "proxy", "oidc"}, set(auth_modes()))

    def test_the_dashboard_has_an_answer_for_every_mode(self):
        block = re.search(r"const AUTH_IDENTITY = \{(.*?)\n\};", APP, re.S)
        self.assertIsNotNone(block, "AUTH_IDENTITY moved or was renamed")
        known = set(re.findall(r'"([a-z]+)":', block.group(1)))
        missing = set(auth_modes()) - known
        self.assertEqual(set(), missing,
                         f"modes the dashboard would render as nothing: {sorted(missing)}")

    def test_only_the_mode_that_routes_a_logout_offers_one(self):
        """`/logout` exists in every mode but only clears an OIDC cookie; in
        `proxy` the credential was never here to clear."""
        block = re.search(r"const AUTH_IDENTITY = \{(.*?)\n\};", APP, re.S).group(1)
        offers = set(re.findall(r'"([a-z]+)":[^\n]*logout: true', block))
        interactive = {mode for mode, cls in auth_modes().items() if cls.interactive}
        self.assertEqual({"oidc"}, offers)
        self.assertEqual(interactive, offers)

    def test_the_fields_it_reads_are_the_ones_the_config_endpoint_sends(self):
        served = re.search(r'if path == "/api/config":(.*?)\}\)', SERVER, re.S)
        self.assertIsNotNone(served, "/api/config moved or was renamed")
        for field in ("auth", "user"):
            with self.subTest(field=field):
                self.assertIn(f'"{field}":', served.group(1))
                self.assertIn(f"config?.{field}" if field == "auth" else f"config.{field}", APP)


class ContextPressureTests(unittest.TestCase):
    """One threshold, in one file.

    `policy.CONTEXT_PRESSURE` was measured from real overflows, and the gauge
    carried a second copy of the number as a literal.  Two definitions that
    agree are indistinguishable from two that have drifted until the day one
    moves.
    """

    def test_the_page_reads_the_threshold_rather_than_repeating_it(self):
        import policy
        self.assertIn("state.config?.context_pressure", APP)
        self.assertNotIn(str(policy.CONTEXT_PRESSURE), code_only(APP),
                         "the measured threshold is written out again in the page")

    def test_the_api_serves_the_one_definition(self):
        served = re.search(r'if path == "/api/config":(.*?)\}\)', SERVER, re.S)
        self.assertIn("policy.CONTEXT_PRESSURE", served.group(1))

    def test_the_gauge_and_the_table_cannot_disagree(self):
        """Both go through one function, so a band is decided in one place."""
        for site in ("model.gaugeFill.className = pressureClass(share)",
                     'el("td", pressureClass(row.pressure || 0)'):
            with self.subTest(site=site):
                self.assertIn(site, APP)

    def test_no_band_is_decided_anywhere_but_that_function(self):
        """The gauge used to compare against two bare numbers inline.  Both
        are named now -- one served, one a constant -- and a comparison against
        a literal share is the shape that would bring the drift back."""
        body = re.search(r"function pressureClass\(share\) \{(.*?)\n\}",
                         code_only(APP), re.S)
        self.assertIsNotNone(body, "pressureClass moved or was renamed")
        strays = re.findall(r"share\s*[<>]=?\s*0\.\d+",
                            code_only(APP).replace(body.group(1), ""))
        self.assertEqual([], strays, f"bands decided outside pressureClass: {strays}")

    def test_the_table_marks_only_what_the_loop_itself_flagged(self):
        """`pressure` is present on a row only when the runner emitted
        `context:pressure` for it, so the threshold is never re-decided here."""
        served = (ROOT / "web" / "runs.py").read_text()
        self.assertIn('event.get("event") == "context:pressure"', served)
        self.assertIn("policy.context_pressure(", served)
        self.assertIn("row.pressure", APP)


def thinking_levels():
    """Every thinking level `lmloop run --thinking` names, from its own help.

    The only place the list is written down.  `config` takes the value as an
    opaque string and each agent decides what it accepts, so there is no
    constant to read -- which is exactly why the sheet was able to fall two
    behind without anything saying so.
    """
    found = re.findall(r'help="thinking level: ([^"]+)"',
                       (ROOT / "lmloop.py").read_text())
    assert found, "the --thinking help moved or was reworded"
    return [{level.strip() for level in line.split(",")} for line in found]


class ThinkingVocabularyTests(unittest.TestCase):
    """The new-run sheet offered five of the seven levels the CLI documents.

    `xhigh` and `max` were missing, so a run started from the dashboard could
    not reach a level a run started from a terminal could -- on a catalogue
    where 21 models advertise `xhigh` and 14 advertise `max`.
    """

    def test_the_cli_names_the_levels_this_test_thinks_it_does(self):
        """A guard on the guard: finding none would pass everything."""
        levels = thinking_levels()
        self.assertGreaterEqual(len(levels), 2, "run and resume both take it")
        self.assertIn("xhigh", levels[0])

    def test_run_and_resume_agree_about_them(self):
        first, *rest = thinking_levels()
        for other in rest:
            self.assertEqual(first, other)

    def test_the_sheet_offers_every_one_of_them(self):
        offered = set(re.findall(r'<option value="([a-z]*)"', HTML))
        missing = thinking_levels()[0] - offered
        self.assertEqual(set(), missing,
                         f"levels the dashboard cannot reach: {sorted(missing)}")
        # "" is the sheet's own: it means "send no --thinking at all".
        self.assertIn("", offered)


class RunFieldContractTests(unittest.TestCase):
    """Every `run.<field>` the dashboard reads is one the API really serves.

    The other half of the drift this file exists for. `OUTCOME_CLASS` catches a
    word the dashboard does not know; this catches a field the dashboard thinks
    it knows and the API never sends -- which renders as nothing at all, and so
    reads as "this run has none of that" rather than as a bug.
    """

    def payload_keys(self):
        """The keys `web/runs.py` really returns, from really calling it."""
        import eta
        from web import runs as runs_module

        base = Path(tempfile.mkdtemp())
        run_dir = base / ".worktrees" / "r" / ".lmloop" / "runs" / "r"
        run_dir.mkdir(parents=True)
        (run_dir / "status.json").write_text(json.dumps({
            "phase": "stopped", "model": "local/model", "updated_at": "",
        }))
        (run_dir / "lmloop.log").write_text(
            json.dumps({"event": "run:start", "agent": "omp"}) + "\n"
        )
        served = set(runs_module.detail({"id": "p", "path": str(base)}, run_dir))
        # The estimate is spread into the summary and only for a running run,
        # so its keys are absent above.  Read them from `eta` rather than
        # listing them here, for the same reason as everything else in this
        # file.
        served |= set(eta.estimate(
            [{"event": "iteration:end", "outcome": "ok", "elapsedMs": 1000},
             {"event": "iteration:end", "outcome": "ok", "elapsedMs": 1000}],
            iteration=1, max_iterations=4,
        ))
        return served

    def test_the_api_fixture_this_test_relies_on_still_produces_a_payload(self):
        """A guard on the guard: an empty payload would pass everything."""
        served = self.payload_keys()
        self.assertGreaterEqual(len(served), 30, served)
        for expected in ("state", "agent", "model", "eta_seconds",
                         "peak_output", "truncations"):
            self.assertIn(expected, served)

    def test_every_run_field_the_dashboard_reads_is_one_the_api_sends(self):
        read = set(re.findall(r"\brun\.([a-z_]+)\b", APP))
        missing = read - self.payload_keys()
        self.assertEqual(set(), missing,
                         f"fields the dashboard reads and the API never sends: {sorted(missing)}")


class AgentAttributionTests(unittest.TestCase):
    """lmloop drives pi, omp and opencode, and they fail differently enough
    that reading a failure starts with knowing which one produced it.  The run
    record names it because the `run:start` event used to say "pi" whatever it
    was; the dashboard has to say it for that to be worth anything.
    """

    def function_body(self, name):
        match = re.search(rf"function {name}\(.*?\) \{{(.*?)\n\}}", APP, re.S)
        self.assertIsNotNone(match, f"{name} moved or was renamed")
        return match.group(1)

    def test_the_run_card_names_the_agent(self):
        self.assertIn("run.agent", self.function_body("metaBits"))

    def test_the_run_view_names_the_agent(self):
        self.assertIn("run.agent", self.function_body("patchModel"))

    def test_a_run_from_before_the_field_existed_shows_no_empty_separator(self):
        """Six archived runs predate `agent`, and every run archived by an
        older lmloop always will.  Both surfaces join with a separator, so an
        empty value is not merely invisible -- it is a stray dot."""
        self.assertIn("if (run.agent) bits.push(run.agent)", self.function_body("metaBits"))
        self.assertIn(".filter(Boolean)", self.function_body("patchModel"))


class PlanWindowTests(unittest.TestCase):
    """The collapsed plan view a phone sees.

    It used to keep three steps -- the one just finished, the one running,
    and the one after -- which answered a question nobody asked at the cost
    of two of the few lines a folded plan gets before the rest scrolls off.
    It shows only the step actually in progress now; the neighbours are one
    tap away in the full list.
    """

    def function_body(self, name):
        match = re.search(rf"function {name}\(.*?\) \{{(.*?)\n\}}", APP, re.S)
        self.assertIsNotNone(match, f"{name} moved or was renamed")
        return match.group(1)

    def test_only_one_step_is_ever_appended(self):
        body = self.function_body("planWindow")
        self.assertEqual(1, body.count("holder.append(stepNode("),
                         "a collapsed plan must show only the step in progress")

    def test_the_neighbouring_step_lookups_are_gone(self):
        """The window used to look a step behind (the last done one) and a
        step ahead (`current + 1`); either surviving here means the window
        is back to three steps without this test having been updated."""
        body = self.function_body("planWindow")
        self.assertNotIn("current + 1", body)
        self.assertNotIn("filter((step) => step.done).pop()", body)


class IterationClockTests(unittest.TestCase):
    """The model card on the run detail page -- context gauge, thinking level,
    tok/s -- said nothing about how long the current iteration had been
    running, and that was one navigation away in the list view's row card the
    whole time (`liveElapsed`, already fed by `run.elapsed_seconds`, which is
    the iteration's own clock -- see pi_runner's `started` -- not the run's).
    """

    def function_body(self, name):
        match = re.search(rf"function {name}\(.*?\) \{{(.*?)\n\}}", APP, re.S)
        self.assertIsNotNone(match, f"{name} moved or was renamed")
        return match.group(1)

    def test_the_header_promotes_the_live_iteration_clock(self):
        self.assertIn('"iteration elapsed"', APP)
        body = self.function_body("patchHead")
        self.assertIn("liveIterationClock(run)", body)
        self.assertIn("isWorking(run)", body)

    def test_the_iteration_clock_includes_seconds(self):
        body = self.function_body("clockDuration")
        self.assertIn("padStart(2", body)
        self.assertIn("% 60", body)

    def test_the_per_second_ticker_updates_it_without_a_full_repaint(self):
        """The prominent header figure advances between API polls."""
        self.assertIn('head.cells["iteration elapsed"]', APP)


class IterationDetailTests(unittest.TestCase):
    """Clicking a row in the iterations table expands it to that iteration's
    own slice of notes.md, instead of sending the reader to the Notes section
    to find the right heading by eye.  Confirmed live on the poker-night run:
    two 'agent-error' rows whose reason -- llama-swap rejecting an image the
    agent tried to view -- previously only turned up by grepping the raw
    per-iteration jsonl stream by hand.
    """

    def function_body(self, name):
        match = re.search(rf"function {name}\(.*?\) \{{(.*?)\n\}}", APP, re.S)
        self.assertIsNotNone(match, f"{name} moved or was renamed")
        return match.group(1)

    def test_notes_are_split_by_the_same_heading_rundir_writes(self):
        """rundir.append_notes writes '### Iteration N'; the split pattern
        here has to match that heading exactly or every row shows nothing."""
        body = self.function_body("iterationNotes")
        self.assertIn(r"### Iteration (\d+)", body)

    def test_the_table_is_handed_the_runs_notes(self):
        self.assertIn("iterationTable(run.iterations, run.notes)", APP)

    def test_expansion_uses_the_hidden_attribute_not_a_css_class(self):
        """CLAUDE.md: `[hidden]` loses to any `display` rule, so a class
        pretending to be it is exactly the shipped-twice bug this project
        already paid for once."""
        body = self.function_body("iterationTable")
        self.assertIn("detail.hidden = ", body)
        self.assertNotIn("classList", body)


class RunStateVocabularyTests(unittest.TestCase):
    def test_every_state_the_api_computes_is_one_the_dashboard_knows(self):
        served = set(re.findall(r'return \("?([a-z]+)"', (ROOT / "web" / "runs.py").read_text()))
        served |= set(re.findall(r'return "([a-z]+)", age', (ROOT / "web" / "runs.py").read_text()))
        served -= {"unknown"}
        code = code_only(APP)
        for state in served:
            with self.subTest(state=state):
                self.assertIn(state, code, f"the dashboard never mentions state {state!r}")


class ResumeControlTests(unittest.TestCase):
    def test_stopped_run_resumes_without_overriding_its_iteration_policy(self):
        self.assertIn('control("Resume run", "continue", run)', APP)
        self.assertNotIn('Continue 3 iterations', APP)


class PreviewControlTests(unittest.TestCase):
    """Preview controls must stay honest while the backend owns the state."""

    def test_detail_renders_all_preview_states_and_actions(self):
        self.assertIn('const PREVIEW_STATES = new Set(["disabled", "stopped", "starting", "ready", "failed"]);', APP)
        body = re.search(r"function previewPanel\(run\) \{(.*?)\n\}", APP, re.S).group(1)
        for state in ("stopped", "starting", "ready", "failed"):
            with self.subTest(state=state):
                self.assertIn(f'"{state}"', body)
        for action in ("start", "stop", "restart"):
            self.assertIn(f'"{action}"', body)

    def test_mutations_are_disabled_in_read_only_but_link_is_not(self):
        body = re.search(r"function previewPanel\(run\) \{(.*?)\n\}", APP, re.S).group(1)
        self.assertIn("button.disabled = readOnly", body)
        self.assertIn('link.target = "_blank"', body)
        self.assertIn('link.rel = "noopener"', body)

    def test_preview_url_resolves_remote_browser_host_and_bounds_logs(self):
        body = re.search(r"function previewHref\(preview\) \{(.*?)\n\}", APP, re.S).group(1)
        self.assertIn("window.location.hostname", body)
        self.assertIn("preview.url_template", body)
        self.assertIn("preview.port", body)
        self.assertIn("preview.path", body)
        self.assertIn("PREVIEW_LOG_LIMIT", APP)

    def test_detail_poll_does_not_skip_preview_refresh(self):
        body = re.search(r"async function renderRun\(.*?\n\}", APP, re.S).group(0)
        self.assertIn('api(`/api/runs/${project}/${runId}`)', body)
        self.assertNotIn("if (quiet && key === state.detailKey)", body)

    def test_stopping_runs_are_treated_as_working_for_rate_and_indicators(self):
        self.assertIn('const isWorking = (runOrState) =>', APP)
        self.assertIn('s === "running" || s === "stopping"', APP)
        self.assertIn('model.rate.textContent = isWorking(run) ? rate(run.tokens_per_second) : ""', APP)
        self.assertIn('const working = active.some((run) => isWorking(run));', APP)
        self.assertIn('const live = active.some((r) => isWorking(r));', APP)


class WebPushVocabularyTests(unittest.TestCase):
    """Web Push's three surfaces -- server, service worker, page -- have to
    agree on the same field name and the same two event types, or a
    subscribed browser silently never shows anything."""

    SW = (ROOT / "web" / "static" / "sw.js").read_text()

    def test_the_config_endpoint_serves_the_key_the_page_reads(self):
        served = re.search(r'if path == "/api/config":(.*?)\}\)', SERVER, re.S)
        self.assertIn('"push_public_key":', served.group(1))
        self.assertIn("state.config.push_public_key", APP)

    def test_the_service_worker_handles_both_push_events(self):
        self.assertIn('addEventListener("push"', self.SW)
        self.assertIn('addEventListener("notificationclick"', self.SW)

    def test_the_page_offers_a_subscribe_toggle_wired_to_both_api_routes(self):
        self.assertIn("push-toggle", HTML)
        self.assertIn('"/api/push/subscribe"', APP)
        self.assertIn('"/api/push/unsubscribe"', APP)

    def test_both_routes_are_served(self):
        self.assertIn('path == "/api/push/subscribe"', SERVER)
        self.assertIn('path == "/api/push/unsubscribe"', SERVER)

    def test_the_offline_page_is_precached_and_referenced_by_the_worker(self):
        self.assertIn('"/static/offline.html"', self.SW)
        self.assertIn('caches.match("/static/offline.html")', self.SW)
        self.assertTrue((ROOT / "web" / "static" / "offline.html").is_file())


if __name__ == "__main__":
    unittest.main()
