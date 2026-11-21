// lmloop operator console (M3 shared vertical slice). No framework, no remote
// assets. Every datum from the API is written with textContent / createTextNode
// only; no HTML-string sink is used anywhere in this file. Fixture evidence only.
"use strict";

const POLL_MS = 5000;
const POLL_MAX_MS = 30000;
const NOTE_MAX_BYTES = 2000; // operator::MAX_NOTE_BYTES
const $ = (id) => document.getElementById(id);
const enc = encodeURIComponent;

const state = {
  runs: null, // null until the first answer
  selected: null,
  view: null, // latest RunView for `selected`
  fails: 0,
  lastGood: 0,
  timer: 0,
  refreshing: false,
  again: false,
  autoPicked: false,
  log: null, // { name, seq, loadedAt }
  logSeq: 0,
  inflight: new Map(), // `${run}:${action}` -> true
  flashes: new Map(), // run -> { kind, text, at }
  dlg: null, // { id, action, invoker }
  auth: undefined, // undefined until /api/config answers; then "oidc" (edge) or null (standalone)
};

/* ───── small helpers ───── */
function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
}
const pad = (n) => String(n).padStart(2, "0");
function clock(unix) {
  const d = new Date(unix * 1000);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}
function stamp(unix) {
  if (!unix) return "unknown time";
  const d = new Date(unix * 1000);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${clock(unix)}`;
}
function ago(unix) {
  const s = Math.max(0, Math.round(Date.now() / 1000 - unix));
  if (s < 90) return `${s}s ago`;
  if (s < 5400) return `${Math.round(s / 60)}m ago`;
  if (s < 172800) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}
function bytesLabel(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1048576) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / 1048576).toFixed(1)} MiB`;
}
const words = (s) => String(s || "").replace(/_/g, " ");
const utf8 = (s) => new TextEncoder().encode(s).length;

/** Rebuild `box` only when `sig` changed, keeping keyboard focus on the same logical control. */
function rebuild(box, sig, build) {
  if (box._sig === sig) return;
  box._sig = sig;
  const active = document.activeElement;
  const key = active && box.contains(active) ? active.dataset.fk : null;
  const top = box.scrollTop;
  box.replaceChildren(...build());
  box.scrollTop = top;
  if (key) {
    const again = [...box.querySelectorAll("[data-fk]")].find((n) => n.dataset.fk === key);
    if (again) again.focus({ preventScroll: true });
  }
}

class ApiError extends Error {
  constructor(kind, message, status) {
    super(message);
    this.kind = kind;
    this.status = status || 0;
  }
}

const SESSION_EXPIRED = "Your sign-in session has expired. Reload and sign in again.";

async function api(path, opts) {
  let r;
  try {
    // "manual": an expired OIDC session answers with a cross-origin redirect to the IdP,
    // which fetch would otherwise report as an unreadable network failure.
    r = await fetch(path, { cache: "no-store", redirect: "manual", ...opts });
  } catch (_) {
    throw new ApiError("network", "Cannot reach the lmloop server.");
  }
  if (r.type === "opaqueredirect") throw new ApiError("session_expired", SESSION_EXPIRED, 401);
  let raw = "";
  try {
    raw = await r.text();
  } catch (_) {
    /* unreadable body */
  }
  let body = null;
  try {
    body = JSON.parse(raw);
  } catch (_) {
    /* not JSON */
  }
  if (!r.ok) {
    // The Rust server sends {error:{kind,message}}; the OIDC edge may send {error:"text"},
    // {message:"text"} or plain text/HTML.
    const e = body && body.error;
    const str = (v) => (typeof v === "string" && v.trim() ? v.trim().slice(0, 200) : "");
    const plain = raw && !/^\s*</.test(raw) && body === null ? str(raw) : "";
    const kind = (e && e.kind) || "";
    // Basic keeps its own {kind:"unauthorized"} reply and the browser's credential prompt.
    if (r.status === 401 && (edge() || (!kind && state.auth === undefined))) throw new ApiError("session_expired", SESSION_EXPIRED, 401);
    throw new ApiError(str(kind) || `http_${r.status}`, str(e && e.message) || str(e) || str(body && body.message) || plain || `HTTP ${r.status}`, r.status);
  }
  if (body === null) throw new ApiError("bad_reply", "The server sent a reply this page cannot read.");
  return body;
}
// Standalone Basic config has no `auth`/`csrf`. The OIDC edge adds `auth: "oidc"`
// and a per-session `csrf`; the token is read per mutation and never stored or put
// in a URL, so an expired session fails at /api/config instead of using a stale one.
const OIDC_NO_RESUME = "resume is not offered through the OIDC edge; use the standalone console";
const edge = () => state.auth === "oidc";

async function post(path, body, action) {
  const cfg = await api("/api/config");
  state.auth = cfg.auth || null;
  const headers = { "Content-Type": "application/json" };
  if (cfg.read_only === true) throw new ApiError("read_only", "This console is read-only.");
  if (edge()) {
    if (action === "resume") throw new ApiError("forbidden", OIDC_NO_RESUME);
    if (typeof cfg.csrf !== "string" || !cfg.csrf) throw new ApiError("csrf_missing", "The session has no CSRF token; reload and sign in again.");
  }
  if (typeof cfg.csrf === "string" && cfg.csrf) headers["X-CSRF-Token"] = cfg.csrf;
  return api(path, { method: "POST", headers, body: JSON.stringify(body || {}) });
}

/* ───── status vocabulary ───── */
function phaseClass(p) {
  if (["worker", "gate", "committing", "idle", "gate_running"].includes(p)) return "live";
  if (p === "paused") return "warn";
  if (["complete", "gate_finished"].includes(p)) return "ok";
  if (["failed", "invalid", "interrupted"].includes(p)) return "bad";
  if (p === "stopped") return "stop";
  return "neutral";
}
function badge(text, cls) {
  return el("span", `badge ${cls || ""}`.trim(), text);
}
function phaseBadge(r) {
  const b = badge(words(r.phase), phaseClass(r.phase));
  return b;
}
function leaseText(l) {
  if (!l) return "unknown";
  if (l.state === "held") return `held${l.pid ? ` by pid ${l.pid}` : ""}`;
  if (l.state === "stale") return `stale${l.pid ? ` (was pid ${l.pid})` : ""}`;
  return "free";
}
const KIND = { serial_fixture: "Serial fixture run", serial_pilot: "Serial pilot run (not production)", diagnostic_gate: "Diagnostic gate run", invalid: "Unreadable run" };

/* ───── sync indicator ───── */
function renderSync(mode) {
  const box = $("sync");
  if (!box._built) {
    box.replaceChildren(el("span", "sync-dot"), el("span", "sync-msg"), el("span", "sync-time"));
    box._built = true;
  }
  box.dataset.state = mode;
  const [, msg, time] = box.children;
  msg.textContent = mode === "live" ? "Live" : mode === "offline" ? "Offline · retrying" : "Connecting…";
  time.textContent = state.lastGood ? `${mode === "offline" ? "stale since " : "updated "}${clock(state.lastGood)}` : "";
  box.querySelector(".sync-dot").setAttribute("aria-hidden", "true");
}

/* ───── routing ───── */
function parseHash() {
  // Android's `#lmloop/<runId>` alias: only project "lmloop", and only an ID the
  // Rust side would itself accept (state::validate_run_id).
  const a = /^#lmloop\/([A-Za-z0-9][A-Za-z0-9_-]{0,63})$/.exec(location.hash);
  if (a) return a[1];
  const m = /^#\/run\/([^/]+)$/.exec(location.hash);
  if (!m) return null;
  try {
    return decodeURIComponent(m[1]);
  } catch (_) {
    return null;
  }
}

function route(userInitiated) {
  const id = parseHash();
  const changed = id !== state.selected;
  state.selected = id;
  document.body.dataset.view = id ? "detail" : "list";
  if (changed) {
    state.view = null;
    state.log = null;
    state.logSeq++;
    $("detail").hidden = true;
    $("detail")._sig = null;
    if (id) setState("detail-state", "loading", `Loading run ${id}…`);
    else setState("detail-state", "empty", "Select a run to inspect its tasks, gate, evidence and events.");
    renderRuns();
    refresh();
    if (userInitiated && id) {
      // Focus lands on the heading once the run has rendered (see applyDetail).
      state.focusTitle = true;
    }
  }
  if (!id && userInitiated) {
    const cur = document.querySelector('.run[aria-current="true"]');
    (cur || $("runs").querySelector(".run") || $("refresh")).focus();
  }
}

function setState(id, kind, text, retry) {
  const box = $(id);
  if (!kind) {
    box.replaceChildren();
    box.removeAttribute("data-kind");
    return;
  }
  box.dataset.kind = kind;
  const kids = [document.createTextNode(text)];
  if (retry) {
    const expired = retry === "signin";
    const b = el("button", "btn ghost small", expired ? "Sign in again" : "Retry");
    b.type = "button";
    b.addEventListener("click", () => (expired ? location.assign("/") : refresh()));
    kids.push(document.createElement("br"), b);
  }
  box.replaceChildren(...kids);
}

/* ───── polling ───── */
async function refresh() {
  if (state.refreshing) {
    state.again = true;
    return;
  }
  state.refreshing = true;
  clearTimeout(state.timer);
  try {
    const id = state.selected;
    // The auth mode decides whether Resume is offered, so it is known before the first render.
    const [l, d] = await Promise.allSettled([
      api("/api/runs"),
      id ? api(`/api/runs/${enc(id)}`) : Promise.resolve(null),
      state.auth === undefined ? api("/api/config").then((c) => (state.auth = c.auth || null)) : null,
    ]);
    let ok = true;
    const expired = [l, d].some((x) => x.status === "rejected" && x.reason.kind === "session_expired");
    if (l.status === "fulfilled") {
      applyList(l.value.runs || []);
    } else {
      ok = false;
      if (state.runs === null) setState("runs-state", "error", `Could not load runs: ${l.reason.message}`, expired ? "signin" : true);
    }
    if (id === state.selected && id) {
      if (d.status === "fulfilled") {
        applyDetail(d.value);
      } else if (d.reason.kind === "no_run" || d.reason.status === 404 || (d.reason.kind === "usage" && d.reason.status === 400)) {
        // An ID the server rejects as malformed is, to the operator, a run that does not exist.
        setState("detail-state", "error", `No such run: ${id}. It may have been removed.`);
        $("detail").hidden = true;
      } else {
        ok = false;
        if (!state.view || expired) setState("detail-state", "error", `Could not load run ${id}: ${d.reason.message}`, expired ? "signin" : true);
      }
    }
    if (ok) {
      state.fails = 0;
      state.lastGood = Math.floor(Date.now() / 1000);
    } else {
      state.fails++;
    }
    renderSync(ok ? "live" : "offline");
  } catch (e) {
    // A render bug must not leave the lock held and the poll timer dead.
    state.fails++;
    console.error("lmloop: refresh failed", e);
    try {
      renderSync("offline");
    } catch (_) {
      /* keep recovering */
    }
  } finally {
    state.refreshing = false;
    if (state.again) {
      state.again = false;
      refresh();
    } else {
      schedule();
    }
  }
}

function schedule() {
  clearTimeout(state.timer);
  if (document.hidden) return; // resumed by visibilitychange
  const wait = state.fails ? Math.min(POLL_MAX_MS, POLL_MS * 2 ** state.fails) : POLL_MS;
  state.timer = setTimeout(refresh, wait);
}

/* ───── runs list ───── */
function applyList(runs) {
  state.runs = runs;
  setState("runs-state", runs.length ? null : "empty", "No runs in this state root yet. Start one with the CLI; this page only observes and steers existing runs.");
  $("runs-count").textContent = runs.length ? `${runs.length} newest first${runs.length >= 100 ? " (list capped at 100)" : ""}` : "";
  if (!state.autoPicked && !state.selected && runs.length && matchMedia("(min-width: 960px)").matches) {
    state.autoPicked = true;
    location.replace(`#/run/${enc(runs[0].run_id)}`);
    route(false);
    return;
  }
  renderRuns();
}

function renderRuns() {
  const box = $("runs");
  const runs = state.runs || [];
  rebuild(box, JSON.stringify([runs, state.selected]), () =>
    runs.map((r) => {
      const li = el("li");
      const a = el("a", "run");
      a.href = `#/run/${enc(r.run_id)}`;
      a.dataset.fk = r.run_id;
      if (r.run_id === state.selected) a.setAttribute("aria-current", "true");
      const head = el("span", "run-id", r.run_id);
      const b = phaseBadge(r);
      if (r.owner_lost) b.after(badge("owner lost", "lost"));
      const meta = el("span", "run-meta");
      meta.append(
        el("span", null, r.kind.startsWith("serial_") ? `${r.tasks_done}/${r.tasks_total} tasks` : words(r.kind)),
        el("span", null, `lease ${leaseText(r.lease)}`),
      );
      if (r.pending_controls && r.pending_controls.length) meta.append(el("span", null, `pending ${r.pending_controls.join(", ")}`));
      a.append(head, b, meta);
      if (r.failure || r.problem) a.append(el("span", "run-fail", r.failure || r.problem));
      li.append(a);
      return li;
    }),
  );
}

/* ───── run detail ───── */
function applyDetail(v) {
  state.view = v;
  setState("detail-state", null);
  const first = $("detail").hidden;
  $("detail").hidden = false;

  $("detail-title").textContent = v.run_id;
  $("d-kind").textContent = KIND[v.kind] || words(v.kind);
  $("d-updated").textContent = `Fetched ${clock(Math.floor(Date.now() / 1000))}`;

  const ph = $("d-phase");
  const sig = JSON.stringify([v.phase, v.owner_lost, v.pending_controls]);
  rebuild(ph, sig, () => {
    const out = [phaseBadge(v)];
    if (v.owner_lost) out.push(badge("owner lost", "lost"));
    for (const c of v.pending_controls || []) out.push(badge(`${c} pending`, "warn"));
    return out;
  });

  const total = v.tasks_total || 0;
  $("d-progress").closest(".prog-row").hidden = total === 0;
  $("d-progress").max = Math.max(1, total);
  $("d-progress").value = v.tasks_done || 0;
  $("d-progress-text").textContent = `${v.tasks_done} of ${total} tasks committed`;

  const fail = v.failure || v.problem;
  $("d-failure").hidden = !fail;
  $("d-failure").textContent = fail ? `${v.problem ? "Problem" : "Failure"}: ${fail}` : "";
  $("d-lost").hidden = !v.owner_lost;
  const warns = v.warnings || [];
  $("d-warnings").hidden = !warns.length;
  rebuild($("d-warnings"), JSON.stringify(warns), () => warns.map((w) => el("li", null, w)));

  renderActions(v);
  renderTasks(v);
  renderFacts(v);
  renderLogs(v);
  renderEvents(v);
  renderFlash();

  if (state.focusTitle) {
    state.focusTitle = false;
    $("detail-title").focus({ preventScroll: false });
  } else if (first && !document.activeElement.closest?.("main, aside")) {
    /* leave focus alone on background renders */
  }
}

const ACTIONS = ["pause", "stop", "stop_now", "resume", "approve", "request_changes"];
const DECISIONS = new Set(["approve", "request_changes"]);
const LABEL = { pause: "Pause", stop: "Stop", stop_now: "Stop now", resume: "Resume", approve: "Approve", request_changes: "Request changes" };
const BUSY_LABEL = { resume: "Resuming…", approve: "Recording…", request_changes: "Recording…", pause: "Sending…", stop: "Sending…", stop_now: "Sending…" };

function busyFor(id, action) {
  if (state.inflight.has(`${id}:${action}`)) return true;
  // One decision at a time per run; every other action stays independent so Stop
  // remains usable while a long Resume request is still open.
  return DECISIONS.has(action) && [...DECISIONS].some((d) => state.inflight.has(`${id}:${d}`));
}

function actionInfo(v, a) {
  if (a === "resume" && edge()) return { available: false, reason: OIDC_NO_RESUME };
  return (v.actions && v.actions[a]) || { available: false, reason: "not reported by the server" };
}

function renderActions(v) {
  for (const a of ACTIONS) {
    const btn = document.querySelector(`[data-action="${a}"]`);
    const info = actionInfo(v, a);
    const busy = busyFor(v.run_id, a);
    const usable = info.available && !busy;
    btn.setAttribute("aria-disabled", usable ? "false" : "true");
    btn.setAttribute("aria-describedby", `why-${a}`);
    if (busy) btn.setAttribute("aria-busy", "true");
    else btn.removeAttribute("aria-busy");
    btn.textContent = busy ? BUSY_LABEL[a] : LABEL[a];
    btn.closest(".act").dataset.ok = String(usable);
    $(`why-${a}`).textContent = busy ? "Request in flight." : info.available ? `Will: ${info.reason}` : `Unavailable: ${info.reason}`;
  }
  const mine = [...state.inflight.keys()].filter((k) => k.startsWith(`${v.run_id}:`)).map((k) => k.split(":")[1]);
  const b = $("act-busy");
  b.hidden = !mine.length;
  if (mine.length) {
    b.textContent = mine.includes("resume")
      ? "Resume request is open until the resumed run ends. The page keeps polling and Pause, Stop and Stop now stay available while the server reports them as available."
      : `Sending ${mine.map((m) => LABEL[m]).join(", ")}…`;
  }
}

function renderTasks(v) {
  const tasks = v.tasks || [];
  $("tasks-empty").hidden = tasks.length > 0;
  $("tasks").hidden = tasks.length === 0;
  $("tasks-note").textContent = tasks.length ? `${v.tasks_done} of ${tasks.length} committed` : "";
  rebuild($("tasks"), JSON.stringify(tasks), () =>
    tasks.map((t, i) => {
      const li = el("li", "task");
      li.dataset.state = t.state;
      const pip = el("span", "pip");
      pip.setAttribute("aria-hidden", "true");
      const id = el("span", "task-id");
      id.append(pip, document.createTextNode(t.id));
      const sb = badge(words(t.state), t.state === "committed" ? "ok" : t.state === "running" ? "live" : t.state === "failed" ? "bad" : "neutral");
      const sub = el("span", "task-sub");
      sub.append(el("span", null, t.commit ? `commit ${t.commit.slice(0, 12)}` : "no commit"));
      if (t.gate_log) {
        const b = el("button", "linkbtn", "Open gate log");
        b.type = "button";
        b.dataset.fk = `gate:${t.id}`;
        b.addEventListener("click", () => {
          selectLog(t.gate_log.replace(/^logs\//, ""));
          $("log-text").scrollIntoView({ block: "nearest" });
        });
        sub.append(b);
      }
      li.append(el("span", "task-n", String(i + 1).padStart(2, "0")), id, sb, sub);
      return li;
    }),
  );
}

/** Rows for the recorded watchdog evidence (watchdog.json). Completed-only: there is no in-flight tool state. */
function watchdogRows(w) {
  if (!w) return [];
  const at = (t) => (t ? stamp(t) : "never");
  const model = w.last_pulse_unix
    ? `${w.state === "watching" ? "Generating" : "Generated"}: ${w.model_pulses} complete model record${w.model_pulses === 1 ? "" : "s"}, last ${at(w.last_pulse_unix)}`
    : `${w.state === "watching" ? "Waiting" : "No output"}: no complete model record seen (startup limit ${w.startup_secs}s)`;
  const rows = [
    ["Watchdog", `${words(w.state)} · task ${w.task}\nrecorded ${at(w.updated_at_unix)} · idle limit ${w.idle_secs}s`],
    ["Model", model],
    [
      "Last tool",
      w.last_tool
        ? `${w.last_tool} (completed) · ${at(w.last_tool_unix)}\n${w.tools_completed} completed, in-flight tools are not tracked`
        : `No tool completed yet\n${w.tools_completed} completed, in-flight tools are not tracked`,
      !w.last_tool,
    ],
  ];
  if (w.repeat_count > 1) rows.push(["Repeats", `${w.repeat_count} identical completed calls over unchanged files`]);
  if (w.warning) rows.push(["Warning", `${w.warning}\nAdvisory only; it does not stop the worker.`]);
  if (w.trip) rows.push(["Tripped", w.trip]);
  return rows;
}

function renderFacts(v) {
  const dec = v.decision;
  const rows = [
    ["Objective", v.objective ? String(v.objective) : "Not exposed by the operator API in this milestone.", !v.objective],
    ["Phase", words(v.phase)],
    ["Kind", KIND[v.kind] || words(v.kind)],
    ["Lease", leaseText(v.lease) + (v.lease && v.lease.command ? ` · ${v.lease.command}` : "") + (v.lease && v.lease.started_at_unix ? ` · since ${stamp(v.lease.started_at_unix)}` : "")],
    ["Evidence", words(v.evidence)],
    ["Complete", v.complete ? "Yes: every task committed" : "No"],
    ["Pending", (v.pending_controls || []).join(", ") || "none"],
    [
      "Decision",
      dec
        ? `${words(dec.decision)} · ${stamp(dec.at_unix)}\n${dec.note ? `Note: ${dec.note}\n` : ""}Effect: ${words(dec.effect)}`
        : "None recorded.",
      !dec,
    ],
    ...watchdogRows(v.watchdog),
  ];
  rebuild($("facts"), JSON.stringify(rows), () =>
    rows.map(([k, val, dim]) => {
      const row = el("div");
      const dd = el("dd", dim ? "dim" : null, val);
      row.append(el("dt", null, k), dd);
      return row;
    }),
  );
}

function renderEvents(v) {
  const evs = (v.events || []).slice().reverse();
  rebuild($("events"), JSON.stringify(evs.map((e) => [e.seq, e.kind, e.data])), () => {
    if (!evs.length) return [el("li", "term-empty", "No events recorded for this run.")];
    return evs.map((e) => {
      const li = el("li");
      const when = el("span", "ev-time", clock(e.at_unix));
      when.title = stamp(e.at_unix);
      const kind = el("span", /fail|error|refus|stop/.test(e.kind) ? "ev-kind warn" : "ev-kind", e.kind);
      if (e.task) kind.append(el("span", "ev-task", ` · ${e.task}`));
      li.append(el("span", "ev-seq", `#${e.seq}`), when, kind);
      if (e.data && e.data !== "null") li.append(el("span", "ev-data", e.data));
      return li;
    });
  });
  $("events-label").textContent = `events.jsonl · ${evs.length} shown`;
}

/* ───── logs ───── */
function renderLogs(v) {
  const names = (v.logs || []).slice().reverse();
  $("logs-empty").hidden = names.length > 0;
  $("log-chips").hidden = names.length === 0;
  let sel = state.log && state.log.name;
  if (sel && !names.includes(sel)) {
    state.log = null;
    state.logSeq++;
    sel = null;
    $("log-info").dataset.kind = "";
    $("log-info").textContent = names.length ? "Select a log file to read its tail." : "";
    $("log-text").hidden = true;
  }
  rebuild($("log-chips"), JSON.stringify([names, sel]), () =>
    names.map((n) => {
      const b = el("button", "chip", n);
      b.type = "button";
      b.dataset.fk = n;
      b.setAttribute("aria-pressed", String(n === sel));
      b.addEventListener("click", () => selectLog(n));
      return b;
    }),
  );
  $("log-reload").setAttribute("aria-disabled", String(!sel));
  if (!sel) {
    $("log-info").dataset.kind = "";
    $("log-info").textContent = names.length ? "Select a log file to read its tail." : "";
    $("log-text").hidden = true;
  }
  // Follow a live log: reload quietly when the run's own progress moved.
  const last = (v.events || []).at(-1);
  const stamp2 = `${v.phase}|${last ? last.seq : 0}|${names.length}`;
  if (sel && state.log.stamp !== stamp2) {
    state.log.stamp = stamp2;
    if (state.log.loaded) loadLog(sel, true);
  } else if (sel && !state.log.stamp) {
    state.log.stamp = stamp2;
  }
}

function selectLog(name) {
  state.log = { name, stamp: null, loaded: false };
  const v = state.view;
  if (v) {
    const last = (v.events || []).at(-1);
    state.log.stamp = `${v.phase}|${last ? last.seq : 0}|${(v.logs || []).length}`;
  }
  for (const c of $("log-chips").children) c.setAttribute("aria-pressed", String(c.dataset.fk === name));
  $("log-reload").setAttribute("aria-disabled", "false");
  loadLog(name, false);
}

async function loadLog(name, quiet) {
  const run = state.selected;
  const seq = ++state.logSeq;
  const info = $("log-info");
  const pre = $("log-text");
  if (!quiet) {
    info.dataset.kind = "loading";
    info.textContent = `Loading ${name}…`;
  }
  try {
    const bytes = $("log-bytes").value;
    const t = await api(`/api/runs/${enc(run)}/logs/${enc(name)}?bytes=${bytes}`);
    if (seq !== state.logSeq || run !== state.selected || state.log?.name !== name) return;
    const atEnd = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 24 || pre.hidden;
    pre.textContent = t.text || "";
    pre.hidden = false;
    pre.setAttribute("aria-label", `Log ${t.name}`);
    if (atEnd) pre.scrollTop = pre.scrollHeight;
    info.dataset.kind = "";
    const shown = utf8(t.text || "");
    info.textContent = `${t.name} · ${t.truncated ? `last ${bytesLabel(shown)} of ${bytesLabel(t.size)}; earlier bytes omitted` : `complete, ${bytesLabel(t.size)}`}${t.text ? "" : " (empty)"} · loaded ${clock(Math.floor(Date.now() / 1000))}`;
    if (state.log) state.log.loaded = true;
  } catch (e) {
    if (seq !== state.logSeq || run !== state.selected || state.log?.name !== name) return;
    info.dataset.kind = "error";
    info.textContent = `Could not load ${name}: ${e.message}`;
    if (!quiet) pre.hidden = true;
  }
}

/* ───── actions and confirmation ───── */
const CONFIRM = {
  pause: { title: "Pause run", ok: "Pause run", warn: "" },
  stop: { title: "Stop run", ok: "Stop run", warn: "A stopped run is terminal and cannot be resumed." },
  stop_now: {
    title: "Stop now",
    ok: "Stop now",
    warn: "A stopped run is terminal and cannot be resumed.",
    ack: "I understand the in-flight task is abandoned and the run cannot be continued.",
  },
  resume: { title: "Resume run", ok: "Resume run", warn: "The request stays open until the resumed run ends. Only one resume runs at a time; Stop stays available meanwhile." },
  approve: {
    title: "Approve run",
    ok: "Record approval",
    warn: "Nothing is merged.",
    ack: "I understand this decision is final for this run and changes nothing in Git.",
    note: "Note (optional)",
  },
  request_changes: {
    title: "Request changes",
    ok: "Record request",
    warn: "",
    ack: "I understand this decision is final for this run.",
    note: "What needs to change (required)",
    needNote: true,
  },
};

function openConfirm(action, invoker) {
  const v = state.view;
  if (!v || $("dlg").open) return;
  const c = CONFIRM[action];
  const info = actionInfo(v, action);
  state.dlg = { id: v.run_id, action, invoker };
  $("dlg-title").textContent = c.title;
  $("dlg-run").textContent = `Run ${v.run_id}`;
  const why = info.reason.charAt(0).toUpperCase() + info.reason.slice(1);
  $("dlg-reason").textContent = `${why}.${c.warn ? ` ${c.warn}` : ""}`;
  $("dlg-note-wrap").hidden = !c.note;
  $("dlg-note").value = "";
  if (c.note) $("dlg-note-label").textContent = c.note;
  $("dlg-ack-wrap").hidden = !c.ack;
  $("dlg-ack").checked = false;
  if (c.ack) $("dlg-ack-text").textContent = c.ack;
  $("dlg-err").hidden = true;
  $("dlg-ok").textContent = c.ok;
  $("dlg-cancel").disabled = false;
  syncDialog();
  $("dlg").showModal();
  (c.note ? $("dlg-note") : c.ack ? $("dlg-ack") : $("dlg-ok")).focus();
}

function dialogGap() {
  const c = CONFIRM[state.dlg.action];
  const note = $("dlg-note").value;
  if (c.note && utf8(note) > NOTE_MAX_BYTES) return "note";
  if (c.needNote && !note.trim()) return "note";
  if (c.ack && !$("dlg-ack").checked) return "ack";
  return null;
}

function syncDialog() {
  const c = CONFIRM[state.dlg.action];
  const n = utf8($("dlg-note").value);
  const count = $("dlg-count");
  count.textContent = c.note ? `${n} / ${NOTE_MAX_BYTES} bytes${c.needNote && !$("dlg-note").value.trim() ? " · a note is required" : ""}` : "";
  count.classList.toggle("over", n > NOTE_MAX_BYTES);
  $("dlg-ok").setAttribute("aria-disabled", String(dialogGap() !== null || $("dlg").dataset.sending === "1"));
}

function flash(id, kind, text) {
  state.flashes.set(id, { kind, text, at: Math.floor(Date.now() / 1000) });
  renderFlash();
}

function renderFlash() {
  const f = state.flashes.get(state.selected);
  $("act-ok").hidden = !(f && f.kind === "ok");
  $("act-err").hidden = !(f && f.kind === "bad");
  if (f) $(f.kind === "ok" ? "act-ok" : "act-err").textContent = `${clock(f.at)} · ${f.text}`;
}

function request(id, action, note) {
  const base = `/api/runs/${enc(id)}`;
  if (action === "resume") return post(`${base}/resume`, {}, action);
  if (DECISIONS.has(action)) return post(`${base}/decision`, { decision: action === "approve" ? "approved" : "changes_requested", note }, action);
  return post(`${base}/control`, { control: action }, action);
}

function describe(action, r) {
  const message = r.message.replace(/^PAUSE: /, "");
  return `${LABEL[action]}: ${r.outcome ? `${words(r.outcome)}. ` : ""}${message}`;
}

async function runAction(id, action, note) {
  const key = `${id}:${action}`;
  state.inflight.set(key, true);
  if (state.view && state.view.run_id === id) renderActions(state.view);
  try {
    const r = await request(id, action, note);
    flash(id, "ok", describe(action, r));
    return null;
  } catch (e) {
    const m = `${LABEL[action]} failed (${e.kind}): ${e.message}`;
    flash(id, "bad", m);
    return m;
  } finally {
    state.inflight.delete(key);
    if (state.view && state.view.run_id === id) renderActions(state.view);
    refresh();
  }
}

async function confirmDialog() {
  const d = state.dlg;
  if (!d || $("dlg").dataset.sending === "1") return;
  const gap = dialogGap();
  if (gap) {
    syncDialog();
    (gap === "note" ? $("dlg-note") : $("dlg-ack")).focus();
    return;
  }
  // Last check against the newest poll: never send what the server now refuses.
  const live = state.view && state.view.run_id === d.id && actionInfo(state.view, d.action);
  if (!live || !live.available) {
    $("dlg-err").textContent = `No longer available: ${live ? live.reason : "run changed"}.`;
    $("dlg-err").hidden = false;
    return;
  }
  const note = $("dlg-note").value;
  if (d.action === "resume") {
    closeDialog();
    runAction(d.id, d.action, note); // deliberately not awaited: it blocks for the whole run
    return;
  }
  $("dlg").dataset.sending = "1";
  $("dlg-ok").textContent = "Sending…";
  $("dlg-ok").setAttribute("aria-disabled", "true");
  $("dlg-err").hidden = true;
  const err = await runAction(d.id, d.action, note);
  $("dlg").dataset.sending = "";
  if (err) {
    $("dlg-err").textContent = err;
    $("dlg-err").hidden = false;
    $("dlg-ok").textContent = CONFIRM[d.action].ok;
    syncDialog();
  } else {
    closeDialog();
  }
}

function closeDialog() {
  if ($("dlg").open) $("dlg").close();
}

/* ───── wiring ───── */
$("acts").addEventListener("click", (ev) => {
  const btn = ev.target.closest("[data-action]");
  if (!btn || btn.getAttribute("aria-disabled") === "true") return;
  openConfirm(btn.dataset.action, btn);
});
$("dlg-ok").addEventListener("click", confirmDialog);
$("dlg-cancel").addEventListener("click", () => {
  if ($("dlg").dataset.sending !== "1") closeDialog();
});
$("dlg").addEventListener("cancel", (ev) => {
  if ($("dlg").dataset.sending === "1") ev.preventDefault();
});
$("dlg").addEventListener("close", () => {
  const d = state.dlg;
  state.dlg = null;
  const back = d && d.invoker && d.invoker.isConnected ? d.invoker : $("detail-title");
  back.focus({ preventScroll: true });
});
$("dlg-note").addEventListener("input", syncDialog);
$("dlg-ack").addEventListener("change", syncDialog);

$("log-bytes").addEventListener("change", () => state.log && loadLog(state.log.name, false));
$("log-reload").addEventListener("click", (ev) => {
  if (ev.currentTarget.getAttribute("aria-disabled") === "true" || !state.log) return;
  loadLog(state.log.name, false);
});
$("refresh").addEventListener("click", () => refresh());
addEventListener("hashchange", () => route(true));
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) refresh();
  else clearTimeout(state.timer);
});

renderSync("loading");
setState("runs-state", "loading", "Loading runs…");
route(false);
// route() only refreshes when the selection changes; a bare "/" load needs the first fetch too.
if (!state.refreshing) refresh();
