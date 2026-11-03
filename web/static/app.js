/* lmloop dashboard.
 *
 * Three decisions worth stating.
 *
 * **Views, not dialogs.** A run detail is a page of content, and a modal is the
 * wrong container for one: it fights the scroll, it has no back button, and it
 * reads as provisional. Routing on the hash gives real navigation, working
 * browser back, and links you can send yourself.
 *
 * **Rows are patched, never rebuilt.** The poll used to replace every node each
 * cycle, which restarted animations and made the page flinch every few seconds.
 * Each row is created once, keyed by run id, and updated in place; only the
 * fields that changed are touched.
 *
 * **Polling, not streaming.** A run emits an event every few seconds at most and
 * lives for hours, so a socket would buy nothing and cost a reconnect story.
 * Polling also degrades correctly when a phone sleeps, which is most of the time
 * a run is being watched.
 */

const $ = (id) => document.getElementById(id);
// Present only inside the Android app's WebView (see NativeShellBridge.kt,
// bound before this script ever runs), absent in every browser tab and the
// installed PWA -- those paths are unaffected by anything gated on this.
const NATIVE_SHELL = Boolean(window.LmloopNative);
// MainActivity floats a couple of icon buttons over the header's trailing
// corner (new-run/watch, settings) instead of reserving its own bar -- see
// its "icons float directly over the WebView" comment. `#new-run` itself is
// hidden below since native owns that action outright, but other trailing
// content (`#bar-state` on a run) has no native equivalent and would just
// get covered -- this class reserves room for the icons so nothing the web
// page draws there is ever actually underneath them.
document.documentElement.classList.toggle("native-shell", NATIVE_SHELL);
const state = {
  config: null, runs: [], project: null, timer: null,
  route: { name: "list" }, rows: new Map(), detailKey: null, shell: null,
};

// `vh`/`dvh` compute to a bogus 0px on at least one real Android WebView
// build (confirmed live: `CSS.supports("height", "100dvh")` reports true and
// the declaration is accepted, but `getComputedStyle` resolves it to 0
// regardless) -- so every viewport-relative cap in style.css reads this
// custom property, set from window.innerHeight, through calc() instead of a
// bare vh/dvh token. Re-run on resize/rotation, the only ways this WebView's
// allotted height actually changes (it owns no dynamic show/hide toolbar).
function setViewportUnit() {
  document.documentElement.style.setProperty("--vh1", `${window.innerHeight / 100}px`);
}
setViewportUnit();
addEventListener("resize", setViewportUnit);
addEventListener("orientationchange", setViewportUnit);

/* ── Fetch ─────────────────────────────────────────────────────────────── */

async function api(path, options = {}) {
  const init = { headers: {}, ...options };
  if (options.body) {
    init.method = options.method || "POST";
    init.headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(options.body);
    if (state.config?.csrf) init.headers["X-CSRF-Token"] = state.config.csrf;
  }
  const method = (init.method || "GET").toUpperCase();
  let response;
  for (const delay of method === "GET" ? [0, 250, 750] : [0]) {
    if (delay) await new Promise((resolve) => setTimeout(resolve, delay));
    try {
      response = await fetch(path, init);
      break;
    } catch (error) {
      // A suspended mobile PWA can resume before its network process is ready.
      // Only GET is safe to replay; a mutation must surface its original result.
      if (!(error instanceof TypeError) || delay === 750 || method !== "GET") throw error;
    }
  }
  if (response.status === 401) { location.href = "/login"; throw new Error("unauthenticated"); }
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(payload.error || `HTTP ${response.status}`);
  return payload;
}

/* ── Formatting ────────────────────────────────────────────────────────── */

const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;

function duration(seconds) {
  if (seconds == null) return "";
  const m = Math.floor(seconds / 60);
  return m < 60 ? `${m}m` : `${Math.floor(m / 60)}h${String(m % 60).padStart(2, "0")}`;
}

function clockDuration(seconds) {
  const whole = Math.max(0, Math.floor(seconds || 0));
  const h = Math.floor(whole / 3600);
  const m = String(Math.floor((whole % 3600) / 60)).padStart(2, "0");
  const s = String(whole % 60).padStart(2, "0");
  return `${h}:${m}:${s}`;
}

const ago = (s) => (s == null ? "never" : s < 60 ? "just now" : `${duration(s)} ago`);

/* Token counts run to five figures and are read on a phone. Three significant
 * digits is the whole of the signal: 42.5k against a 57.3k window says what
 * 42506 against 57344 says, in half the width. */
function compact(n) {
  if (n == null) return "";
  if (n < 1000) return String(n);
  const k = n / 1000;
  // Tested against the rounded value, not the raw one: 9999 rounds to 10.0 at
  // one decimal, which is a wider string than the two significant figures this
  // is for.
  return k < 9.95 ? `${k.toFixed(1)}k` : `${Math.round(k)}k`;
}

const rate = (r) => (r ? `${r < 10 ? r.toFixed(1) : Math.round(r)} tok/s` : "");

// A run in "stopping" state has had a stop requested, but its current
// iteration is still actively generating tokens and calling tools until it
// commits and halts at the boundary. It is actively working.
const isWorking = (runOrState) => {
  const s = typeof runOrState === "string" ? runOrState : runOrState?.state;
  return s === "running" || s === "stopping";
};

/* A run id is `<date>-<slug>-<hash>`, and the two ends are the identifying
 * parts: the date says which attempt, the hash says which objective.  Letting
 * CSS truncate it drops the hash -- the half that distinguishes two runs of the
 * same objective on the same day -- so the middle goes instead. */
function elideMiddle(text, head = 20, tail = 7) {
  if (!text || text.length <= head + tail + 1) return text;
  return `${text.slice(0, head)}…${text.slice(-tail)}`;
}

/* The poll is every few seconds; a clock that only moves when it lands looks
 * stuck. Elapsed is extrapolated from when the figure was fetched, so the page
 * keeps time on its own between updates. */
function liveElapsed(run) {
  const drift = (Date.now() - (state.fetchedAt || Date.now())) / 1000;
  return duration(Math.round((run.elapsed_seconds || 0) + drift));
}

function liveIterationClock(run) {
  const drift = isWorking(run)
    ? (Date.now() - (state.fetchedAt || Date.now())) / 1000
    : 0;
  return clockDuration((run.elapsed_seconds || 0) + drift);
}

function elapsed(run) {
  const seconds = run.run_elapsed_seconds ?? run.elapsed_seconds;
  if (seconds == null) return "";
  if (!isWorking(run)) return duration(seconds);
  const drift = (Date.now() - (state.fetchedAt || Date.now())) / 1000;
  return duration(Math.round(seconds + drift));
}

function timingText(run) {
  const bits = [];
  const spent = elapsed(run);
  if (spent) bits.push(`Elapsed ${spent}`);
  if (isWorking(run) && run.eta_seconds != null) {
    bits.push(`Estimated ${duration(run.eta_seconds)} left`);
  }
  return bits.join(" · ");
}

/* The same drawn chevron the back button and the strip use.  Built rather than
 * written as a glyph so all three are one mark at one weight: `▾` renders at a
 * different size in every font that has it, and at 10px it stopped reading as
 * a control at all. */
function chevron(d = "M5 9l7 7 7-7") {
  const NS = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS(NS, "path");
  path.setAttribute("d", d);
  svg.append(path);
  return svg;
}

function el(tag, className, content) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (content != null) node.textContent = content;
  return node;
}

function syncSpans(container, bits, className = () => "") {
  while (container.children.length > bits.length) container.lastChild.remove();
  bits.forEach((bit, index) => {
    const span = container.children[index] || container.appendChild(el("span"));
    if (span.textContent !== bit) span.textContent = bit;
    span.className = className(bit, index);
  });
}

function metaBits(run) {
  const bits = [];
  if (run.iteration) bits.push(`iter ${run.iteration}/${run.max_iterations ?? "?"}`);
  if (run.commits) bits.push(plural(run.commits, "commit"));
  const live = isWorking(run);
  if (!live && run.last_tool) bits.push(run.last_tool);
  if (live && run.elapsed_seconds != null) bits.push(liveElapsed(run));
  if (live && run.eta_seconds) bits.push(`~${duration(run.eta_seconds)} left`);
  if (live && run.tokens_per_second) bits.push(rate(run.tokens_per_second));
  if (run.compactions) bits.push(`${run.compactions} ovf`);
  if (run.defects?.length) bits.push(`${run.defects.length} broken`);
  if (run.agent) bits.push(run.agent);
  if (run.model) bits.push(run.model.split("/").pop());
  bits.push(ago(run.age_seconds));
  return bits;
}

/* ── Row (created once, patched thereafter) ────────────────────────────── */

function makeRow(run) {
  const node = el("button", "row");
  node.type = "button";

  const head = el("div", "row-head");
  const where = el("span", "where");
  const stateLabel = el("span", "state");
  head.append(where, stateLabel);

  const title = el("h3", "title");
  const progress = el("div", "progress");
  const track = el("div", "track");
  const fill = el("span");
  const steps = el("small");
  track.append(fill);
  progress.append(track, steps);

  const now = el("div", "now");
  const nowStep = el("span", "now-step");
  const nowAct = el("span", "now-act");
  now.append(nowStep, nowAct);

  const timing = el("div", "timing");
  const pips = el("div", "pips");
  const meta = el("div", "meta");

  node.append(head, title, progress, now, timing, pips, meta);
  node.addEventListener("click", () => go(`#${run.project}/${run.route_id || run.run_id}`));

  const parts = { node, where, stateLabel, title, progress, track, fill, steps,
                  now, nowStep, nowAct, timing, pips, meta, last: null };
  patchRow(parts, run);
  return parts;
}

function patchRow(parts, run) {
  // Cheap guard: nothing below runs unless something the row shows has moved.
  const key = JSON.stringify([
    run.state, run.title, run.plan_done, run.plan_total, run.outcomes,
    run.iteration, run.commits, run.last_tool, run.last_target, run.current_step,
    run.elapsed_seconds, run.run_elapsed_seconds, run.eta_seconds, run.compactions, run.agent, run.model,
    run.age_seconds, run.tokens_per_second,
  ]);
  if (key === parts.last) return;
  parts.last = key;

  parts.where.textContent = run.project;
  parts.stateLabel.textContent = run.state;
  parts.stateLabel.className = `state ${run.state}`;
  parts.title.textContent = run.title;

  // What it is doing, right now, without opening anything.
  const live = isWorking(run);

  // The bar stays up for a live run with no plan yet, showing nothing done.
  // That is the first iteration, which is the longest one and the one with the
  // least to show for itself -- exactly when a moving bar is worth the most.
  parts.progress.hidden = !run.plan_total && !live;
  parts.fill.style.width = run.plan_total
    ? `${Math.round((run.plan_done / run.plan_total) * 100)}%`
    : "0%";
  parts.steps.textContent = run.plan_total
    ? `${run.plan_done}/${run.plan_total}`
    : (live ? "planning" : "");

  parts.track.classList.toggle("working", live);
  parts.now.hidden = !(live && (run.current_step || run.last_tool));
  parts.nowStep.textContent = run.current_step || "";
  parts.nowAct.textContent = live
    ? [run.last_tool, run.last_target].filter(Boolean).join(" ")
    : "";

  parts.timing.textContent = timingText(run);
  parts.timing.hidden = !parts.timing.textContent;

  parts.node.classList.toggle("broken", Boolean(run.defects?.length));

  const outcomes = run.outcomes || [];
  parts.pips.hidden = outcomes.length === 0;
  while (parts.pips.children.length > outcomes.length) parts.pips.lastChild.remove();
  outcomes.forEach((outcome, index) => {
    const pip = parts.pips.children[index] || parts.pips.appendChild(el("span"));
    pip.className = `pip ${outcome}`;
  });
  parts.pips.title = outcomes.join(", ");

  const bits = metaBits(run);
  const clockAt = bits.indexOf(live && run.elapsed_seconds != null ? liveElapsed(run) : "\u0000");
  syncSpans(parts.meta, bits, (_bit, index) => index === clockAt ? "clock" : "");
}

function syncGroup(container, runs) {
  runs.forEach((run, index) => {
    const id = `${run.project}/${run.route_id || run.run_id}`;
    let parts = state.rows.get(id);
    if (!parts) {
      parts = makeRow(run);
      state.rows.set(id, parts);
    } else {
      patchRow(parts, run);
    }
    const atIndex = container.children[index];
    if (atIndex !== parts.node) container.insertBefore(parts.node, atIndex || null);
  });
  while (container.children.length > runs.length) container.lastChild.remove();
}

/* ── List ──────────────────────────────────────────────────────────────── */

const ACTIVE = new Set(["running", "paused", "stopping"]);

function renderList() {
  const runs = state.project ? state.runs.filter((r) => r.project === state.project) : state.runs;
  const active = runs.filter((r) => ACTIVE.has(r.state));
  // Archived runs are history that has outlived its worktree, and mixing them
  // into History would bury the runs that still have one.  Their own group,
  // last, so the list still opens on what is alive.
  const archived = runs.filter((r) => r.archived);
  const rest = runs.filter((r) => !ACTIVE.has(r.state) && !r.archived);

  syncGroup($("active"), active);
  syncGroup($("finished"), rest);
  syncGroup($("archived"), archived);
  $("active-count").textContent = active.length;
  $("finished-count").textContent = rest.length;
  $("archived-count").textContent = archived.length;
  $("active-group").hidden = active.length === 0;
  $("finished-group").hidden = rest.length === 0;
  $("archived-group").hidden = archived.length === 0;
  $("empty").hidden = runs.length > 0;

  const live = active.some((r) => isWorking(r));
  const stale = runs.some((r) => r.state === "stale");
  $("moon").className = `moon ${live ? "live" : stale ? "stale" : ""}`;
  if (state.route.name === "list") {
    $("bar-sub").textContent = live
      ? `${plural(active.length, "run")} working`
      : `${plural(runs.length, "run")}, idle`;
    $("bar-sub").classList.toggle("warn", stale);
  }
}

function renderFilters() {
  const projects = [...new Set(state.runs.map((r) => r.project))].sort();
  const holder = $("filters");
  const values = projects.length < 2 ? [] : [null, ...projects];
  const key = JSON.stringify(values);
  if (holder.dataset.key === key) {
    [...holder.children].forEach((node, index) =>
      node.setAttribute("aria-pressed", String(state.project === values[index])));
    return;
  }
  holder.dataset.key = key;
  holder.replaceChildren();
  if (!values.length) return;
  const tab = (label, value) => {
    const node = el("button", "tab", label);
    node.type = "button";
    node.setAttribute("aria-pressed", String(state.project === value));
    node.addEventListener("click", () => { state.project = value; renderFilters(); renderList(); });
    return node;
  };
  holder.append(tab("all", null), ...projects.map((p) => tab(p, p)));
}

/* ── Run view ──────────────────────────────────────────────────────────── */

/* ── The live head (built once per run, patched thereafter) ───────────────
 *
 * Everything on the run page that moves lives in here, for one reason: the rest
 * of the page is rebuilt on every poll, and a rebuilt node restarts its CSS
 * animation. The travelling highlight that says "this run is alive" was reset
 * every few seconds by that rebuild and so never actually travelled -- which is
 * the whole of what it exists to do. Re-parenting restarts animations too, so
 * this node is never passed through `replaceChildren`; it stays put, and only
 * the body below it is replaced.
 */

// `state` used to lead this grid. The sticky header carries it now. Keep four
// cells for a balanced 2×2 phone grid; the live iteration clock replaces the
// less useful duplicate of the page's existing update-age status.
const FACTS = ["plan", "commits", "iter", "iteration elapsed"];

function makeHead() {
  const node = el("div", "live");
  node.hidden = true;

  // Where the progress bar used to be.  The header carries that bar now, and
  // two of them a centimetre apart measuring the same plan was the duplication
  // that made the page look unedited.  The name is what belongs at the top of a
  // page about one run, and it had nowhere to be at all.
  const name = el("h2", "run-name");

  const facts = el("div", "facts");
  const cells = {};
  for (const label of FACTS) {
    const box = el("div", "fact");
    cells[label] = el("dd");
    box.append(el("dt", null, label), cells[label]);
    facts.append(box);
  }

  const doing = el("div", "doing");
  const doingLabel = el("div", "label");
  const doingStep = el("div", "step");
  const doingAct = el("div", "act");
  doing.append(doingLabel, doingStep, doingAct);

  const timing = el("div", "timing detail-timing");
  const parts = { node, name, cells, doing, doingLabel, doingStep, doingAct, timing };
  node.append(name, facts, timing, doing, makeModel(parts));
  return parts;
}

/* The model card. A run is a model doing work, and until now the page named the
 * model and stopped there -- which left the two questions actually worth asking
 * of a local model unanswerable from the UI: how fast is it going, and how close
 * is this prompt to the window it will compact at. */
function makeModel(head) {
  const card = el("div", "model");
  head.model = {
    id: el("span", "model-id"),
    tag: el("span", "model-tag"),
    rate: el("span", "model-rate"),
    gaugeText: el("span", "gauge-value"),
    gaugeFill: el("span"),
    meta: el("div", "model-meta"),
  };

  const line = el("div", "model-head");
  line.append(head.model.id, head.model.tag, head.model.rate);

  const gauge = el("div", "gauge");
  const label = el("div", "gauge-label");
  label.append(el("span", null, "context"), head.model.gaugeText);
  const bar = el("div", "gauge-track");
  bar.append(head.model.gaugeFill);
  gauge.append(label, bar);
  head.model.gauge = gauge;

  card.append(line, gauge, head.model.meta);
  head.model.card = card;
  return card;
}

function patchHead(head, run) {
  head.node.hidden = false;

  if (head.name.textContent !== run.title) head.name.textContent = run.title;

  const values = {
    plan: run.plan_total ? `${run.plan_done}/${run.plan_total}` : "—",
    commits: String(run.commits),
    iter: `${run.iteration ?? "?"}/${run.max_iterations ?? "?"}`,
    "iteration elapsed": run.elapsed_seconds == null ? "—" : liveIterationClock(run),
  };
  for (const label of FACTS) {
    const dd = head.cells[label];
    if (dd.textContent !== values[label]) dd.textContent = values[label];
    dd.className = (label === "plan" && ACTIVE.has(run.state))
      || (label === "iteration elapsed" && isWorking(run)) ? "hot" : "";
  }

  head.timing.textContent = timingText(run);
  head.timing.hidden = !head.timing.textContent;
  head.timing.title = run.eta_basis
    ? `Estimate based on ${run.eta_basis}${run.eta_samples ? ` · ${run.eta_samples} samples` : ""}`
    : "";

  const doing = ACTIVE.has(run.state) && (run.current_step || run.last_tool);
  head.doing.hidden = !doing;
  if (doing) {
    head.doingLabel.textContent = run.paused ? "paused on" : "working on";
    head.doingStep.textContent = run.current_step || "";
    head.doingStep.hidden = !run.current_step;
    const act = [run.last_tool, run.last_target].filter(Boolean).join(" ");
    const quiet = run.quiet_seconds > 60 ? ` · quiet ${duration(run.quiet_seconds)}` : "";
    const activity = `${act}${quiet}`;
    if (head.doingAct.textContent !== activity) head.doingAct.textContent = activity;
  }

  patchModel(head.model, run);
}

function patchModel(model, run) {
  model.card.hidden = !run.model;
  if (!run.model) return;

  model.id.textContent = run.model.split("/").pop();
  // Role and thinking are per-iteration, not per-run: planning gets the wide
  // model, a thrash retry gets a wider one still, and knowing which of those is
  // on screen is the difference between "slow" and "wrong model".
  model.tag.textContent = [run.agent, run.role, run.thinking && `thinking ${run.thinking}`]
    .filter(Boolean).join(" · ");
  model.rate.textContent = isWorking(run) ? rate(run.tokens_per_second) : "";

  const used = run.input_tokens || 0;
  const window = run.context_window || 0;
  model.gauge.hidden = !used;
  if (used) {
    const share = window ? used / window : 0;
    model.gaugeText.textContent = window
      ? `${compact(used)} / ${compact(window)} · ${Math.round(share * 100)}%`
      : `${compact(used)} · window unmeasured`;
    model.gaugeFill.style.width = `${Math.round(Math.min(share, 1) * 100)}%`;
    model.gaugeFill.className = pressureClass(share);
  }

  const bits = [];
  // `run.elapsed_seconds` is the *current* iteration's own clock (see
  // pi_runner's `started`), not the run's -- that total already has its own
  // line above this card. Without this, the only way to notice an iteration
  // running long was the row card in the list, one navigation away.
  const live = isWorking(run) && run.elapsed_seconds != null;
  if (live) bits.push(`iteration ${liveElapsed(run)}`);
  if (run.output_tokens) bits.push(`${compact(run.output_tokens)} out`);
  /* The cap is per reply and `output_tokens` is the whole iteration's total,
   * so printing the two side by side -- "13.7k out, 24.6k reply cap" -- read
   * as a budget half spent when the largest single reply was 14% of it. The
   * pair shown now is the pair that can actually be compared. */
  if (run.max_output_tokens && run.peak_output) {
    bits.push(`${compact(run.peak_output)}/${compact(run.max_output_tokens)} peak reply`);
  } else if (run.max_output_tokens) {
    bits.push(`${compact(run.max_output_tokens)} reply cap`);
  }
  // `plural` appends an "s"; this word does not take one.
  if (run.truncations) {
    bits.push(`${run.truncations} ${run.truncations === 1 ? "reply" : "replies"} cut off`);
  }
  if (run.tool_calls) bits.push(`${run.tool_calls} tools`);
  if (run.writes) bits.push(plural(run.writes, "write"));
  bits.push(run.compactions ? plural(run.compactions, "overflow") : "no overflow");
  syncSpans(model.meta, bits, (_bit, index) => (live && index === 0 ? "clock" : ""));
}

/* Plan steps are markdown the agent wrote for itself, so they arrive with
 * `**emphasis**` and `backticks` in them. Rendering those two is the difference
 * between a plan and a dump of a file; anything more would be a markdown
 * parser, which this is deliberately not. */
function inlineMarkdown(text) {
  const holder = document.createDocumentFragment();
  const pattern = /\*\*(.+?)\*\*|`([^`]+)`/g;
  let last = 0;
  for (const match of text.matchAll(pattern)) {
    if (match.index > last) holder.append(text.slice(last, match.index));
    if (match[1]) {
      const strong = el("strong");
      strong.append(inlineMarkdown(match[1]));  // `code` nests inside **bold**
      holder.append(strong);
    } else {
      holder.append(el("code", null, match[2]));
    }
    last = match.index + match[0].length;
  }
  if (last < text.length) holder.append(text.slice(last));
  return holder;
}

/* Parsed once into a list, because the plan is now rendered twice: in full, and
 * as the three-step window a collapsed plan shows on a phone. */
function planParse(plan) {
  const steps = [];
  let seenNext = false;
  for (const line of plan.split("\n")) {
    const match = /^[-*]\s\[( |x|X)\]\s*(.*)$/.exec(line.trim());
    if (!match) continue;
    const done = match[1].toLowerCase() === "x";
    const isNext = !done && !seenNext;
    if (isNext) seenNext = true;
    steps.push({ done, isNext, text: match[2] });
  }
  return steps;
}

function stepNode(step) {
  const node = el("div", `step${step.done ? " done" : step.isNext ? " next" : ""}`);
  const label = el("span");
  label.append(inlineMarkdown(step.text));
  node.append(el("span", "box", step.done ? "✓" : step.isNext ? "▸" : "·"), label);
  return node;
}

function planNodes(steps) {
  const holder = el("div", "plan");
  for (const step of steps) holder.append(stepNode(step));
  return holder;
}

/* What a collapsed plan is worth keeping on screen: the step actually running.
 * One line answers "is it moving, and on what" -- the only question a plan
 * gets asked at a glance -- without the neighbours, which are one tap away in
 * the full list and were costing two lines nobody had asked about. */
function planWindow(steps) {
  const holder = el("div", "plan plan-window");
  const current = steps.findIndex((step) => step.isNext);
  // Nothing outstanding: the plan is done, and the last step is the news.
  const shown = current === -1 ? steps[steps.length - 1] : steps[current];
  if (shown) holder.append(stepNode(shown));
  return holder;
}

/* The bands are about what happens next, not about tidiness: past the warn
 * threshold the next tool result is what triggers a compaction, and a
 * compaction is where a slow run starts thrashing.
 *
 * The warn threshold is `policy.CONTEXT_PRESSURE`, served by /api/config.  It
 * was measured -- iterations that ended in an overflow sat at 80% and up on a
 * 24,576-token window, clean ones far below -- and it used to be written out
 * again here as a literal `0.75`, agreeing with the runner by coincidence.
 * The 0.9 below is this page's own: the runner has nothing to say at 90% that
 * it did not already say at 75%, and the difference is only how loudly. */
const CONTEXT_DANGER = 0.9;

function pressureClass(share) {
  const warn = state.config?.context_pressure;
  if (share > CONTEXT_DANGER) return "bad";
  return warn && share > warn ? "warn" : "";
}

const OUTCOME_CLASS = {
  ok: "ok", thrashing: "warn", truncated: "warn", "no-action": "warn",
  looping: "warn",
  stalled: "bad", timeout: "bad", "tool-timeout": "bad", "agent-error": "bad",
  interrupted: "",
};

/* notes.md is one file for the whole run, but each iteration's own summary,
 * changed files and learnings sit under their own "### Iteration N" heading
 * -- see rundir.append_notes. Splitting it here, client-side, means a click
 * on the iterations table can show just the one that matters instead of
 * sending the reader to a wall of text and a find-on-page. No new endpoint:
 * `run.notes` is already the whole file, sent on every poll. */
function iterationNotes(notes) {
  const map = new Map();
  if (!notes) return map;
  const parts = notes.split(/^### Iteration (\d+)$/m);
  for (let i = 1; i < parts.length; i += 2) {
    map.set(Number(parts[i]), parts[i + 1].trim());
  }
  return map;
}

function iterationTable(rows, notes) {
  const wrap = el("div", "scroll-x");
  const table = document.createElement("table");
  const head = document.createElement("tr");
  // `in` is the prompt as the model counted it -- the number that decides
  // whether the next iteration compacts -- and is the one column here that
  // explains a thrash rather than just recording one.  An iteration the loop
  // flagged carries its share of the window beside the count: without it, the
  // run that motivated this -- three iterations at 80-84% and then an overflow
  // -- reads as ordinary work followed by something inexplicable.
  for (const label of ["#", "outcome", "time", "wr", "ovf", "in", "out", "plan", "commit"]) {
    head.append(el("th", null, label));
  }
  table.append(head);
  const byIteration = iterationNotes(notes);
  for (const row of rows) {
    const tr = document.createElement("tr");
    tr.className = "iter-row";
    tr.append(el("td", null, row.iteration));
    tr.append(el("td", OUTCOME_CLASS[row.outcome] ?? "", row.outcome ?? "…"));
    tr.append(el("td", null, row.seconds ? duration(row.seconds) : ""));
    tr.append(el("td", null, row.writes ?? ""));
    tr.append(el("td", null, row.compactions || ""));
    // `row.pressure` exists only for an iteration the loop itself flagged, so
    // the threshold is never re-decided here -- only how hard to say it.
    tr.append(el("td", pressureClass(row.pressure || 0),
                 row.pressure
                   ? `${compact(row.input_tokens)} · ${Math.round(row.pressure * 100)}%`
                   : compact(row.input_tokens)));
    tr.append(el("td", null, compact(row.output_tokens)));
    tr.append(el("td", null, row.plan_total ? `${row.plan_done}/${row.plan_total}` : ""));
    tr.append(el("td", null, row.commit ? row.commit.slice(0, 8) : ""));
    table.append(tr);

    const detail = el("tr", "iter-detail");
    detail.hidden = true;
    const cell = el("td");
    cell.colSpan = 9;
    cell.append(el("pre", null, byIteration.get(row.iteration) || "nothing recorded for this iteration"));
    detail.append(cell);
    table.append(detail);

    tr.addEventListener("click", () => { detail.hidden = !detail.hidden; });
  }
  wrap.append(table);
  return wrap;
}

function control(label, action, run, { risk = false, body = {}, confirm: ask = null,
                                       done = null } = {}) {
  const button = el("button", risk ? "risk" : "quiet", label);
  button.type = "button";
  button.addEventListener("click", async () => {
    // Anything that removes something asks first, and the question names what
    // is about to go rather than saying "are you sure".
    if (ask && !window.confirm(ask)) return;
    button.disabled = true;
    const was = button.textContent;
    button.textContent = "working…";
    try {
      const result = await api(`/api/runs/${run.project}/${run.route_id || run.run_id}/${action}`, { body });
      if (done) done(result);
      state.detailKey = null;
      // A run that was archived or deleted is no longer at this URL.
      if (action === "delete") { go("#"); return; }
      await poll();
    } catch (error) {
      button.disabled = false;
      button.textContent = was;
      alert(error.message);
    }
  });
  return button;
}

const PREVIEW_STATES = new Set(["disabled", "stopped", "starting", "ready", "failed"]);
const PREVIEW_LOG_LIMIT = 4000;

function previewPayload(run) {
  // Preview is a nested capability in the detail payload. Keep the fallback
  // deliberately narrow: older payloads simply render as disabled.
  const preview = run["preview"];
  return preview && typeof preview === "object" ? preview : { enabled: false, state: "disabled" };
}

function previewState(preview) {
  if (!preview.enabled) return "disabled";
  return PREVIEW_STATES.has(preview.state) ? preview.state : "stopped";
}

function previewHref(preview) {
  const raw = String(preview.url || preview.url_template || "").trim();
  if (!raw) return "";
  const host = window.location.hostname;
  const port = preview.port == null ? "" : String(preview.port);
  const path = preview.path == null ? "" : String(preview.path);
  // Backends may use browser-host/port/path tokens when the server and browser
  // are not on the same machine. Absolute configured URLs are otherwise kept.
  const resolved = raw
    .replace(/\{(?:browser[-_])?host(?:name)?\}|<browser-host>|browser-host|\{host\}/gi, host)
    .replace(/\{port\}|<port>|:port\b/gi, port)
    .replace(/\{path\}|<path>/gi, path);
  try {
    const absolute = /^[a-z][a-z\d+.-]*:\/\//i.test(resolved);
    const url = new URL(resolved, window.location.origin);
    if (!absolute && port) url.port = port;
    if (!absolute && path) url.pathname = path.startsWith("/") ? path : `/${path}`;
    return url.href;
  } catch {
    return "";
  }
}

function previewLogTail(log) {
  const text = String(log || "");
  if (text.length <= PREVIEW_LOG_LIMIT) return text;
  return `…${text.slice(-PREVIEW_LOG_LIMIT)}`;
}

function previewControl(label, previewAction, run, { risk = false } = {}) {
  const button = el("button", risk ? "risk" : "quiet", label);
  button.type = "button";
  button.addEventListener("click", async () => {
    button.disabled = true;
    const was = button.textContent;
    button.textContent = "working…";
    try {
      await api(`/api/runs/${run.project}/${run.route_id || run.run_id}/preview`, {
        body: { action: previewAction },
      });
      state.detailKey = null;
      await poll();
    } catch (error) {
      button.disabled = false;
      button.textContent = was;
      alert(error.message);
    }
  });
  return button;
}

function previewPanel(run) {
  const preview = previewPayload(run);
  const current = previewState(preview);
  const panel = el("div", "preview-panel");
  const status = el("div", "preview-status");
  status.setAttribute("role", "status");
  status.setAttribute("aria-live", "polite");
  status.append(el("span", "preview-label", "State"), el("strong", `preview-state ${current}`, current));
  panel.append(status);

  const actions = el("div", "controls preview-controls");
  const readOnly = Boolean(state.config?.read_only);
  const addAction = (label, action, risk = false) => {
    const button = previewControl(label, action, run, { risk });
    button.disabled = readOnly;
    if (readOnly) button.title = "Read-only mode";
    actions.append(button);
  };
  if (current === "stopped" || current === "failed") addAction("Start", "start");
  if (current === "starting" || current === "ready") addAction("Stop", "stop", true);
  if (current === "ready" || current === "failed") addAction("Restart", "restart");

  const href = current === "ready" ? previewHref(preview) : "";
  if (href) {
    const link = el("a", "act preview-link", "Open preview");
    link.href = href;
    link.target = "_blank";
    link.rel = "noopener";
    link.setAttribute("aria-label", "Open preview in a new tab");
    actions.append(link);
  }
  if (actions.children.length) panel.append(actions);

  const errorText = String(preview.error || "").trim();
  if (errorText) {
    const error = el("p", "alert preview-error");
    error.setAttribute("role", "alert");
    error.textContent = `${errorText} Try Start again or inspect the preview log.`;
    panel.append(error);
  }
  const tail = previewLogTail(preview.log ?? preview.log_tail);
  if (tail) {
    const logs = document.createElement("details");
    logs.className = "preview-logs";
    logs.dataset.key = "preview-logs";
    const summary = document.createElement("summary");
    summary.textContent = "Preview logs";
    logs.append(summary, el("pre", null, tail));
    panel.append(logs);
  }
  return panel;
}

/* The run page is a fixed head plus a replaceable body. `#view-run` itself is
 * never cleared while a run is on screen, because clearing it would take the
 * head with it. */
function runShell(runId) {
  const view = $("view-run");
  if (state.shell?.runId === runId && view.contains(state.shell.head.node)) return state.shell;
  const head = makeHead();
  const body = el("div", "run-body");
  view.replaceChildren(head.node, body);
  state.shell = { runId, head, body };
  return state.shell;
}

async function renderRun(project, runId, { quiet = false } = {}) {
  // The list poll does not promise the detail-only preview fields. Fetch the
  // normal detail resource on every quiet poll so preview state is authoritative
  // and never inferred from a button click.
  $("bar-title").textContent = project;
  $("bar-sub").textContent = elideMiddle(runId);
  $("bar-sub").title = runId;

  const { head, body } = runShell(runId);
  if (!quiet) body.replaceChildren(el("p", "empty", "Loading…"));

  let run;
  try {
    run = await api(`/api/runs/${project}/${runId}`);
  } catch (error) {
    body.replaceChildren(el("p", "alert", error.message));
    return;
  }

  patchHead(head, run);

  // Preserve what the reader was doing across a background refresh.  Keyed by
  // section, never by index: the sections present depend on the run (a plan may
  // not exist yet, broken files usually do not), so an index-keyed list reopened
  // whichever section happened to land in that slot this time.
  const scroll = window.scrollY;
  const open = new Map(
    [...body.querySelectorAll("details[data-key]")].map((node) => [node.dataset.key, node.open])
  );

  const parts = [];

  /* Every block on this page is the same kind of thing -- a labelled section
   * that can be folded away -- so they are all one component.  Folding is the
   * point: the run page had grown past three screens of scroll on a phone, and
   * the two longest blocks were the two nobody rereads. */
  const section = (key, label, build, { start = true, extra = "" } = {}) => {
    const box = document.createElement("details");
    box.className = `sect ${extra}`.trim();
    box.dataset.key = key;
    box.open = open.has(key) ? open.get(key) : start;
    const summary = el("summary");
    // The label and the caret share a row of their own.  They used to be flex
    // children of the summary itself, alongside the plan's three-step window --
    // which is full width, so it wrapped, and pushed the caret onto a third
    // line at the bottom of a 180px block, a long way from the thing it opens.
    const head = el("div", "sect-head");
    head.append(el("span", "sect-label", label));
    const caret = el("span", "sect-caret");
    caret.append(chevron());
    head.append(caret);
    summary.append(head);
    box.append(summary);
    const inner = el("div", "sect-body");
    build(inner, summary);
    box.append(inner);
    parts.push(box);
    return box;
  };

  if (!state.config?.read_only) {
    const controls = el("div", "controls");
    if (run.archived) {
      // No worktree, so nothing to pause, continue or archive.  The only thing
      // left to decide about an archived run is whether to keep it.
      controls.append(control("Delete permanently", "delete", run, {
        risk: true,
        body: { branch: false },
        confirm: `Permanently delete the archived record of ${run.run_id}?\n\n`
               + `Its plan, handoff and every iteration log will be gone. The `
               + `git branch lmloop/${run.run_id} is kept.`,
      }));
    } else if (ACTIVE.has(run.state)) {
      controls.append(run.paused ? control("Resume", "resume", run) : control("Pause", "pause", run));
      if (!run.stopping) controls.append(control("Stop after this iteration", "stop", run, { risk: true }));
      controls.append(control("Stop now", "stop-now", run, { risk: true }));
    } else {
      controls.append(control("Resume run", "continue", run));
      if (run.commits) {
        controls.append(control("Open pull request", "pr", run, {
          done: (result) => {
            if (result?.url) window.open(result.url, "_blank", "noopener");
          },
        }));
      }
      controls.append(control("Archive & remove worktree", "archive", run, {
        risk: true,
        confirm: `Archive ${run.run_id}?\n\n`
               + `Its record is copied out first, then the worktree is removed. `
               + `The git branch and every commit on it are kept, and the run `
               + `stays readable under Archived.`,
      }));
    }
    parts.push(controls);
  }

  section("preview", "Preview", (inner) => inner.append(previewPanel(run)), { start: true });

  if (run.defects?.length) {
    section("defects", `Broken files — ${run.defects.length}`, (inner) => {
      for (const defect of run.defects) inner.append(el("div", "defect", defect));
    }, { extra: "broken-panel" });
  }

  // Closed by default: the objective is what you already know -- you wrote it --
  // and it was holding the first screen of every visit hostage.
  section("objective", "Objective", (inner) => {
    inner.append(el("div", "objective", run.objective));
  }, { start: false });

  // Above the plan, deliberately.  The plan says what is meant to happen; the
  // iteration table says what actually has been, and which is the question being
  // asked when someone opens a running job.
  if (run.iterations?.length) {
    section("iterations", "Iterations", (inner) => {
      inner.append(iterationTable(run.iterations, run.notes));
    });
  }

  if (run.plan) {
    const steps = planParse(run.plan);
    // Folded to its three-step window on a phone, open on anything wider.  A
    // fifteen-step plan is most of the page's scroll on a 390px screen, and the
    // twelve steps already ticked off are not what the scrolling is for.
    const narrow = window.matchMedia("(max-width: 640px)").matches;
    section("plan", `Plan — ${run.plan_done} of ${run.plan_total}`, (inner, summary) => {
      // Lives in the summary so it is what a folded plan shows; CSS drops it on
      // wide screens, where folding away means folding away.
      if (steps.length) summary.append(planWindow(steps));
      inner.append(planNodes(steps));
    }, { start: !narrow });
  }

  for (const [key, label, content] of [["handoff", "Handoff", run.handoff], ["notes", "Notes", run.notes]]) {
    if (!content) continue;
    section(key, label, (inner) => inner.append(el("pre", null, content)), { start: false });
  }

  body.replaceChildren(...parts);
  if (quiet) window.scrollTo(0, scroll);
}

/* ── New run ───────────────────────────────────────────────────────────── */

/* Why the model list is what it is.
 *
 * `/api/models` asks the configured agent for its catalogue, and says in
 * `model_source` when it could not: the agent's own name when the list really
 * came from asking it, one of these when it did not.  Rendered as <option>s
 * the two are indistinguishable, and picking from a list nobody asked for
 * produces a run that dies on its first request, minutes later, for a reason
 * nothing on this page would explain.
 *
 * Every other value of `model_source` is an agent's name -- so this map is
 * also the definition of "the list is real", and `test_web_frontend.py` reads
 * both it and `web/server.py` to keep the two spellings the same. */
const MODEL_SOURCE_REASON = {
  "unavailable": "The agent could not be asked: not installed, or it did not answer.",
  "unknown agent": "The configured agent is not one lmloop knows.",
  "fallback": "The agent listed no models.",
};
const CANNOT_LIST = " cannot list";

function modelSourceNote(catalogue) {
  const source = catalogue.model_source || "";
  const reason = MODEL_SOURCE_REASON[source] || (source.endsWith(CANNOT_LIST)
    ? `${source.slice(0, -CANNOT_LIST.length)} has no way to list its models.`
    : "");
  if (!reason) return "";
  // What the operator does about it differs by which of these it is.  An empty
  // select is not a dead end: `start_run` sends no --model at all when nothing
  // is picked, and the run takes the model from the project's own config.
  return `${reason} ${(catalogue.models || []).length
    ? "This is the configured default, not a catalogue."
    : "Nothing to pick from \u2014 the run will use the model in the project\u2019s own .lmloop.toml."}`;
}

async function renderNew() {
  $("bar-title").textContent = "New run";
  $("bar-sub").textContent = "one objective, many iterations";
  $("launch-error").hidden = true;

  const { projects } = await api("/api/projects");
  $("project").replaceChildren(...projects.map((p) => {
    const option = document.createElement("option");
    option.value = p.id;
    option.textContent = p.runs ? `${p.name} · ${plural(p.runs, "run")}` : p.name;
    return option;
  }));
  const NEW = "\u0000new";
  const fresh = document.createElement("option");
  fresh.value = NEW;
  fresh.textContent = "+ new project…";
  $("project").append(fresh);
  if (state.project) $("project").value = state.project;
  toggleNewProject();

  // Fetched here rather than at startup: it costs a couple of seconds and only
  // this form needs it.
  const catalogue = state.models || (state.models = await api("/api/models"));
  $("model").replaceChildren(...(catalogue.models || []).map((id) => {
    const option = document.createElement("option");
    option.value = option.textContent = id;
    return option;
  }));
  $("model").value = state.config.default_model;
  const source = modelSourceNote(catalogue);
  $("model-source").textContent = source;
  $("model-source").hidden = !source;
  $("thinking").value = state.config.default_thinking || "";
  $("iterations").value = state.config.default_max_iterations;
}

function toggleNewProject() {
  const creating = $("project").value === "\u0000new";
  $("new-project-fields").hidden = !creating;
  $("project-name").required = creating;
  $("launch-submit").textContent = creating ? "Create project and start" : "Start run";
}

$("project").addEventListener("change", toggleNewProject);

$("launch-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const button = $("launch-submit");
  button.disabled = true;
  try {
    let project = $("project").value;
    if (project === "\u0000new") {
      // Create first, then run against it: two calls, but one action as far as
      // anyone using it is concerned.
      const made = await api("/api/projects", {
        body: { name: $("project-name").value.trim(), objective: $("objective").value },
      });
      project = made.id;
      state.project = made.id;
    }
    await api("/api/runs", {
      body: {
        project,
        objective: $("objective").value,
        model: $("model").value,
        thinking: $("thinking").value,
        max_iterations: Number($("iterations").value),
      },
    });
    $("objective").value = "";
    $("project-name").value = "";
    state.models = state.models;   // catalogue is still good
    await poll();
    go("#");
  } catch (error) {
    $("launch-error").textContent = error.message;
    $("launch-error").hidden = false;
  } finally {
    button.disabled = false;
  }
});

/* ── Sticky run status ───────────────────────────────────────
 *
 * A run is watched over hours, from whatever page happens to be open, and until
 * now "is anything still working" meant navigating back to the list to find
 * out.  The strip answers it from anywhere and costs nothing when the answer is
 * no: with no active runs it is not on the page at all, rather than sitting
 * there saying zero.
 *
 * It is patched, never rebuilt, for the same reason the rows are -- it carries
 * the sweep, and a rebuilt node restarts the animation that is the whole point.
 */

/* Two shapes, one component.  On a phone it is a strip across the bottom that
 * opens upward; past 1080px -- where the 760px column leaves a third of the
 * screen empty anyway -- it stops being a strip at all and becomes a rail down
 * the side, permanently open.  A drawer you have to keep opening is a phone
 * affordance; on a desktop there is room to just show the thing. */
const RAIL = window.matchMedia("(min-width: 1080px)");
const runbar = { rows: new Map(), openPanel: false };

function runbarLine(run) {
  return run.current_step
    || [run.last_tool, run.last_target].filter(Boolean).join(" ")
    || run.title;
}

function makeRunbarRow(run) {
  const node = el("button", "runbar-row");
  node.type = "button";

  const head = el("div", "runbar-row-head");
  const where = el("span", "where");
  const stateLabel = el("span", "state");
  head.append(where, stateLabel);

  const title = el("h3", "runbar-row-title");
  const line = el("div", "runbar-row-line");

  const progress = el("div", "progress");
  const track = el("div", "track");
  const fill = el("span");
  const steps = el("small");
  track.append(fill);
  progress.append(track, steps);

  const meta = el("div", "meta");

  node.append(head, title, line, progress, meta);
  node.addEventListener("click", () => {
    if (!RAIL.matches) toggleRunbar(false);
    go(`#${run.project}/${run.route_id || run.run_id}`);
  });
  return { node, where, stateLabel, title, line, track, fill, steps, meta, last: null };
}

/* Expanded says more than the strip does, and different things: the strip
 * answers "is it alive and what is it touching", which is one line.  Once
 * someone has opened it they are asking the other question -- how far, how
 * fast, how long, how much landed -- so that is what the row carries. */
function patchRunbarRow(parts, run) {
  const key = JSON.stringify([
    run.state, run.project, run.title, run.plan_done, run.plan_total,
    run.current_step, run.last_tool, run.last_target, run.iteration,
    run.max_iterations, run.commits, run.tokens_per_second, run.elapsed_seconds,
    run.eta_seconds,
  ]);
  if (key === parts.last) return;
  parts.last = key;

  parts.where.textContent = run.project;
  parts.stateLabel.textContent = run.state;
  parts.stateLabel.className = `state ${run.state}`;
  parts.title.textContent = run.title;
  parts.line.textContent = runbarLine(run);
  parts.line.hidden = !runbarLine(run);

  const live = isWorking(run);
  parts.fill.style.width = run.plan_total
    ? `${Math.round((run.plan_done / run.plan_total) * 100)}%`
    : "0%";
  parts.steps.textContent = run.plan_total
    ? `${run.plan_done}/${run.plan_total}`
    : (live ? "planning" : "");
  parts.track.classList.toggle("working", live);

  const bits = [];
  if (run.iteration) bits.push(`iter ${run.iteration}/${run.max_iterations ?? "?"}`);
  if (run.commits) bits.push(plural(run.commits, "commit"));
  if (live && run.elapsed_seconds != null) bits.push(liveElapsed(run));
  if (live && run.eta_seconds != null) bits.push(`~${duration(run.eta_seconds)} left`);
  if (live && run.tokens_per_second) bits.push(rate(run.tokens_per_second));
  if (run.compactions) bits.push(`${run.compactions} ovf`);
  const clockAt = live && run.elapsed_seconds != null ? bits.indexOf(liveElapsed(run)) : -1;
  syncSpans(parts.meta, bits, (_bit, index) => index === clockAt ? "clock" : "");
}

function toggleRunbar(open) {
  // In rail mode there is nothing to toggle: the panel is the rail.
  runbar.openPanel = RAIL.matches ? true : (open ?? !runbar.openPanel);
  // Deliberately not `hidden`: `display: none` cannot be transitioned, and the
  // open needs to be an animation rather than an appearance.  The class is the
  // only switch, and CSS does the rest.
  $("runbar-strip").setAttribute("aria-expanded", String(runbar.openPanel));
  $("runbar").classList.toggle("open", runbar.openPanel);
}

function renderRunbar() {
  const active = state.runs.filter((run) => ACTIVE.has(run.state));
  const bar = $("runbar");

  // Nothing running: take the whole thing off the page, and take its reserved
  // space with it -- a strip that says "0 runs" is furniture, not information.
  if (!active.length) {
    bar.hidden = true;
    document.body.classList.remove("has-runbar");
    if (runbar.openPanel) toggleRunbar(false);
    return;
  }
  bar.hidden = false;
  document.body.classList.add("has-runbar");
  if (RAIL.matches && !runbar.openPanel) toggleRunbar(true);

  const working = active.some((run) => isWorking(run));
  // Lit only for a run that is actually generating.  Paused gets the dark moon,
  // not the red one: red is this palette's word for "something is wrong", and a
  // run someone deliberately paused is the one case where nothing is.
  $("runbar-moon").className = `moon ${working ? "live" : ""}`.trim();

  // The strip names the run; it does not narrate it.  The current step changes
  // every few minutes and is a sentence long, so a collapsed strip showing it
  // was a line of text that rewrote itself under the eye and still had to be
  // truncated -- unreadable as either a label or a status.  The step is one tap
  // away in the panel, and in full on the run page.
  const single = active.length === 1 ? active[0] : null;
  $("runbar-text").textContent = RAIL.matches
    ? `${plural(active.length, "run")} active`
    : (single ? single.title : `${plural(active.length, "run")} active`);
  $("runbar-count").textContent = single && single.plan_total
    ? `${single.plan_done}/${single.plan_total}`
    : (active.length > 1 ? String(active.length) : "");

  const panel = $("runbar-panel");
  active.forEach((run, index) => {
    const id = `${run.project}/${run.route_id || run.run_id}`;
    let parts = runbar.rows.get(id);
    if (!parts) {
      parts = makeRunbarRow(run);
      runbar.rows.set(id, parts);
    }
    patchRunbarRow(parts, run);
    const atIndex = panel.children[index];
    if (atIndex !== parts.node) panel.insertBefore(parts.node, atIndex || null);
  });
  while (panel.children.length > active.length) panel.lastChild.remove();
  for (const [id] of runbar.rows) {
    if (!active.some((run) => `${run.project}/${run.route_id || run.run_id}` === id)) {
      runbar.rows.delete(id);
    }
  }
}

$("runbar-strip").addEventListener("click", () => { if (!RAIL.matches) toggleRunbar(); });
// Crossing the breakpoint changes which shape this is, and the strip's text
// with it, so it has to re-render rather than wait for the next poll.
RAIL.addEventListener("change", () => { if (state.runs.length) renderRunbar(); });

/* ── Routing ───────────────────────────────────────────────────────────── */

function go(hash) {
  if (location.hash === hash) route();
  else location.hash = hash;
}

function parseHash() {
  const raw = decodeURIComponent(location.hash.slice(1));
  if (raw === "new") return { name: "new" };
  const [project, runId] = raw.split("/");
  if (project && runId) return { name: "run", project, run_id: runId };
  return { name: "list" };
}

/* The sticky header carries the two things worth having while scrolled: what
 * state the run is in, and how far through the plan it is.  Both used to live
 * only in the facts grid, which is the first thing off the top of the screen. */
function paintBar(run) {
  const chip = $("bar-state");
  const track = $("bar-track");
  if (!run) {
    chip.hidden = true;
    track.hidden = true;
    return;
  }
  chip.hidden = false;
  chip.textContent = run.state;
  // `bar-chip` carries the shared typography of this slot and has to survive
  // the rewrite -- assigning className drops every class not named here.
  chip.className = `state bar-chip ${run.state}`;

  const live = isWorking(run);
  track.hidden = !run.plan_total && !live;
  track.firstChild.style.width = run.plan_total
    ? `${Math.round((run.plan_done / run.plan_total) * 100)}%`
    : "0%";
  track.classList.toggle("working", live);
}

// Only once the page has actually moved: a flat surface at rest is the design,
// and the shadow exists to say there is content underneath.
addEventListener("scroll", () => {
  $("bar").classList.toggle("scrolled", window.scrollY > 4);
}, { passive: true });

async function route({ quiet = false } = {}) {
  const next = parseHash();
  const changed = JSON.stringify(next) !== JSON.stringify(state.route);
  state.route = next;
  if (changed) state.detailKey = null;

  $("view-list").hidden = next.name !== "list";
  $("view-run").hidden = next.name !== "run";
  $("view-new").hidden = next.name !== "new";
  // One or the other, never both: they share a grid cell, so leaving the moon
  // up painted it straight over the chevron.  The back button takes the mark's
  // place rather than pushing it aside, which is what keeps the title still.
  $("back").hidden = next.name === "list";
  $("moon").hidden = next.name !== "list";
  // The native app's TopAppBar owns this action when embedded -- see
  // MainActivity.kt's Scaffold, which drives the same go('#new') route.
  $("new-run").hidden = next.name !== "list" || Boolean(state.config?.read_only) || NATIVE_SHELL;

  paintBar(next.name === "run"
    ? state.runs.find((run) => run.run_id === next.run_id)
    : null);

  if (next.name === "list") {
    $("bar-title").textContent = "lmloop";
    renderList();
  } else if (next.name === "run") {
    await renderRun(next.project, next.run_id, { quiet: quiet && !changed });
  } else if (changed) {
    await renderNew();
  }
}

window.addEventListener("hashchange", () => route());
$("back").addEventListener("click", () => (history.length > 1 ? history.back() : go("#")));
$("new-run").addEventListener("click", () => go("#new"));

/* ── Poll ──────────────────────────────────────────────────────────────── */

async function poll() {
  try {
    const { runs } = await api("/api/runs");
    state.runs = runs;
    state.fetchedAt = Date.now();
    renderFilters();
    // Before the route: the strip is on every page, so it must not depend on
    // which one is showing.
    renderRunbar();
    await route({ quiet: true });
  } catch (error) {
    $("bar-sub").textContent = error.message;
    $("bar-sub").classList.add("warn");
    $("moon").className = "moon stale";
  }
}

function schedule() {
  clearTimeout(state.timer);
  const seconds = document.hidden ? state.config.hidden_poll_seconds : state.config.poll_seconds;
  state.timer = setTimeout(async () => { await poll(); schedule(); }, seconds * 1000);
}

function resumePolling() {
  if (document.hidden || !state.config) return;
  clearTimeout(state.timer);
  void poll().finally(schedule);
}

document.addEventListener("visibilitychange", () => {
  if (document.hidden) schedule();
  else resumePolling();
});
window.addEventListener("pageshow", (event) => {
  if (event.persisted) resumePolling();
});
window.addEventListener("online", resumePolling);

// One second, only while something is running and the tab is visible.
setInterval(() => {
  if (document.hidden) return;
  if (!state.runs.some(isWorking)) return;
  for (const [id, parts] of state.rows) {
    const run = state.runs.find((item) =>
      `${item.project}/${item.route_id || item.run_id}` === id);
    if (!run || !isWorking(run)) continue;
    parts.timing.textContent = timingText(run);
  }
  const shown = state.runs.find((run) => run.run_id === state.route.run_id);
  const panel = document.querySelector("#view-run .detail-timing");
  if (panel && shown && isWorking(shown)) panel.textContent = timingText(shown);
  const iterationClock = state.shell?.head.cells["iteration elapsed"];
  if (iterationClock && shown && isWorking(shown)) {
    iterationClock.textContent = liveIterationClock(shown);
  }
  const modelClock = document.querySelector("#view-run .model-meta .clock");
  if (modelClock && shown && isWorking(shown)) {
    modelClock.textContent = `iteration ${liveElapsed(shown)}`;
  }
  // The strip carries the same clock, and it is on screen on every page.
  for (const [runId, parts] of runbar.rows) {
    const run = state.runs.find((item) => item.run_id === runId);
    if (!run || !isWorking(run)) continue;
    const clock = parts.meta.querySelector(".clock");
    if (clock) clock.textContent = liveElapsed(run);
  }
}, 1000);

/* ── Who is asking ─────────────────────────────────────────────────────── */

/* The server has three answers and the page had none of them.
 *
 * `web/auth.py` supports `none`, `proxy` and `oidc`, and /api/config reports
 * which is live as `auth` along with the name it resolved.  The page read
 * neither, so a proxy-authenticated deployment -- the common one, behind an
 * ingress that already does SSO -- showed no identity at all even though every
 * request carried one, and offered no way to sign out or any reason why.
 *
 * Only `oidc` owns /login and /logout; in `proxy` the session belongs to the
 * proxy and a sign-out button here would clear a cookie that was never the
 * credential.  Saying that is the point: an absent button reads as an
 * oversight, and this one is a fact about the deployment. */
const AUTH_IDENTITY = {
  // `none` is loopback-only by construction -- the server refuses to bind a
  // network without an identity boundary -- so there is nobody to name.
  "none": null,
  "proxy": { note: "sign out at your proxy" },
  "oidc": { logout: true },
};

function renderIdentity(config) {
  const shape = AUTH_IDENTITY[config?.auth];
  $("who").hidden = !shape || !config.user;
  if (!shape || !config.user) return;
  $("who-name").textContent = `Signed in as ${config.user}`;
  $("who-out").hidden = !shape.logout;
  $("who-note").hidden = !shape.note;
  if (shape.note) $("who-note").textContent = shape.note;
}

/* ── Web Push ──────────────────────────────────────────────────────────────
 *
 * Independent of identity (see index.html): shown whenever the server
 * offers a `push_public_key`, in every auth mode including `none`. A run is
 * hours long and unattended by design -- this is the moment nobody is
 * looking, and previously the only way to hear about it was ntfy or having
 * the tab open. See `web/push.py` and `webpush.py` for the server side. */

function urlBase64ToUint8Array(base64) {
  const padded = base64.padEnd(base64.length + (4 - (base64.length % 4)) % 4, "=");
  const raw = atob(padded.replace(/-/g, "+").replace(/_/g, "/"));
  return Uint8Array.from(raw, (char) => char.charCodeAt(0));
}

async function currentPushSubscription() {
  const registration = await navigator.serviceWorker.ready;
  return registration.pushManager.getSubscription();
}

async function setPushToggleLabel() {
  const button = $("push-toggle");
  const subscription = await currentPushSubscription().catch(() => null);
  button.textContent = subscription ? "Notifications on" : "Notify this device";
  button.setAttribute("aria-pressed", String(Boolean(subscription)));
}

async function togglePush() {
  const button = $("push-toggle");
  button.disabled = true;
  try {
    const existing = await currentPushSubscription();
    if (existing) {
      await api("/api/push/unsubscribe", { body: { endpoint: existing.endpoint } });
      await existing.unsubscribe();
    } else {
      if (await Notification.requestPermission() !== "granted") return;
      const registration = await navigator.serviceWorker.ready;
      const subscription = await registration.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: urlBase64ToUint8Array(state.config.push_public_key),
      });
      await api("/api/push/subscribe", { body: subscription.toJSON() });
    }
  } catch (error) {
    console.error("push toggle failed", error); // eslint-disable-line no-console
  } finally {
    button.disabled = false;
    await setPushToggleLabel();
  }
}

async function initPush() {
  if (!("serviceWorker" in navigator) || !("PushManager" in window) || !state.config.push_public_key) return;
  const button = $("push-toggle");
  button.hidden = false;
  button.addEventListener("click", togglePush);
  await setPushToggleLabel();
}

(async function start() {
  try {
    state.config = await api("/api/config");
  } catch {
    return;
  }
  renderIdentity(state.config);
  initPush();
  await poll();
  schedule();
})();
