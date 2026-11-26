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
  // New run. Filled from /api/launch-options; sending is the duplicate-submit guard and
  // locked blocks a retry whose first attempt may have created a run.
  launch: { enabled: false, max: 0, opts: [], unavail: [], dropped: 0, status: "idle", error: null, readOnly: false, sel: null, step: "form", sending: false, locked: false, done: false, invoker: null, seq: 0 },
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

// `expect` (opt-in): the one success status the caller accepts; any other 2xx is a bad_reply.
async function api(path, { expect, ...opts } = {}) {
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
  if (expect && r.status !== expect) throw new ApiError("bad_reply", `The server answered ${r.status} where ${expect} was expected.`, r.status);
  return body;
}
// Standalone Basic config has no `auth`/`csrf`. The OIDC edge adds `auth: "oidc"`
// and a per-session `csrf`; the token is read per mutation and never stored or put
// in a URL, so an expired session fails at /api/config instead of using a stale one.
const OIDC_NO_RESUME = "resume is not offered through the OIDC edge; use the standalone console";
const edge = () => state.auth === "oidc";

async function post(path, body, action, expect) {
  const cfg = await api("/api/config");
  state.auth = cfg.auth || null;
  const headers = { "Content-Type": "application/json" };
  if (cfg.read_only === true) throw new ApiError("read_only", "This console is read-only.");
  if (edge()) {
    if (action === "resume") throw new ApiError("forbidden", OIDC_NO_RESUME);
    if (typeof cfg.csrf !== "string" || !cfg.csrf) throw new ApiError("csrf_missing", "The session has no CSRF token; reload and sign in again.");
  }
  if (typeof cfg.csrf === "string" && cfg.csrf) headers["X-CSRF-Token"] = cfg.csrf;
  try {
    return await api(path, { method: "POST", headers, body: JSON.stringify(body || {}), expect });
  } catch (e) {
    // Only errors raised after the POST left tell the caller the server may have acted.
    if (e instanceof ApiError) e.sent = true;
    throw e;
  }
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
  setState("runs-state", runs.length ? null : "empty", "No runs in this state root yet. Use New run to start one from an approved policy, or use the CLI.");
  $("runs-count").textContent = runs.length ? `${runs.length} newest first${runs.length >= 100 ? " (capped at 100 in total; older runs, active or not, are not listed)" : ""}` : "";
  if (!state.autoPicked && !state.selected && runs.length && matchMedia("(min-width: 960px)").matches) {
    state.autoPicked = true;
    location.replace(`#/run/${enc(runs[0].run_id)}`);
    route(false);
    return;
  }
  renderRuns();
}

// Unfinished phases, from engine RunPhase and the diagnostic Phase. A lost owner never moves a run
// by itself: serial runs stay in worker/gate/committing, and a diagnostic gate whose owner died is
// already reported by the server as the terminal `interrupted`. Unknown phases are not claimed active.
const ACTIVE_PHASES = new Set(["idle", "worker", "gate", "committing", "paused", "gate_running"]);
const RUN_GROUPS = [
  { key: "active", title: "Active", note: "Unfinished runs, including paused ones and any whose owner is lost." },
  { key: "history", title: "History", note: "Complete, failed, stopped, interrupted, unreadable or unrecognised." },
];
/** Split by phase only, keeping the server's newest-first order inside each group. */
function groupRuns(runs) {
  const g = { active: [], history: [] };
  for (const r of runs) g[ACTIVE_PHASES.has(r.phase) ? "active" : "history"].push(r);
  return g;
}

function runRow(r) {
  const li = el("li");
  const a = el("a", "run");
  a.href = `#/run/${enc(r.run_id)}`;
  a.dataset.fk = r.run_id;
  if (r.run_id === state.selected) a.setAttribute("aria-current", "true");
  const head = el("span", "run-id", r.run_id);
  const badges = el("span", "run-badges");
  badges.append(phaseBadge(r));
  if (r.owner_lost) badges.append(badge("owner lost", "lost"));
  const meta = el("span", "run-meta");
  meta.append(
    el("span", null, r.kind.startsWith("serial_") ? `${r.tasks_done}/${r.tasks_total} tasks` : words(r.kind)),
    el("span", null, `lease ${leaseText(r.lease)}`),
  );
  if (r.pending_controls && r.pending_controls.length) meta.append(el("span", null, `pending ${r.pending_controls.join(", ")}`));
  a.append(head, badges, meta);
  if (r.failure || r.problem) a.append(el("span", "run-fail", r.failure || r.problem));
  li.append(a);
  return li;
}

function renderRuns() {
  const box = $("runs");
  const runs = state.runs || [];
  // One container for both groups so focus on a row survives it moving between them.
  rebuild(box, JSON.stringify([runs, state.selected]), () => {
    const groups = groupRuns(runs);
    return RUN_GROUPS.filter((g) => groups[g.key].length).map((g) => {
      const sec = el("div", "run-group");
      sec.setAttribute("role", "group");
      sec.setAttribute("aria-labelledby", `runs-g-${g.key}`);
      const h = el("h3", null, g.title);
      h.id = `runs-g-${g.key}`;
      h.append(el("span", "tally", String(groups[g.key].length)));
      const ul = el("ul", "runs");
      ul.append(...groups[g.key].map(runRow));
      sec.append(h, el("p", "note", g.note), ul);
      return sec;
    });
  });
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
      const files = Array.isArray(t.changed) && t.changed.length ? ` · ${t.changed.length} file${t.changed.length === 1 ? "" : "s"}` : "";
      sub.append(el("span", null, t.commit ? `commit ${t.commit.slice(0, 12)}${files}` : "no commit"));
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
    ...(v.branch ? [["Worktree branch", v.branch]] : []),
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

/* ───── new run (operator-approved checkout policies) ─────
   The server owns every authority: the page sends only the chosen policy's id, its
   revision and the objective. It never accepts or sends a path, command, worker or budget.
   GET /api/launch-options is {enabled, objective_max_bytes, projects[]}; the API names no
   worker, so none is shown. */
const RUN_ID = /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/;
const LAUNCH_EMPTY = "No approved checkout policies are available. An operator approves them in the server's setup; this page cannot add one and accepts no path, command or time budget.";
const LAUNCH_OFF = "New run is disabled: this server has no launch registry. Nothing can be started from this page.";
const INCOMPLETE = "The server sent incomplete review data for this policy, so it cannot be started from here.";
const UNNAMED = "The server sent an entry this page cannot identify safely.";

function budgetLabel(s) {
  if (s > 0 && s % 3600 === 0) return `${s / 3600} h`;
  if (s > 0 && s % 60 === 0) return `${s / 60} min`;
  return `${s} s`;
}
// null is "unset" (the manifest left it out); that is a fact to show, not a zero.
const secsLabel = (s, why) => (s == null ? `unset${why ? ` (${why})` : ""}` : budgetLabel(s));
const reasonText = (v) => (typeof v === "string" ? v.replace(/[\u0000-\u001f\u007f-\u009f]+/g, " ").trim().slice(0, 200) : "") || "No reason was given.";

// The only place that knows the option schema (rust/src/project.rs `Registry::options`).
// An eligible entry the operator cannot fully review becomes unavailable, never half-filled.
function normalizeOptions(raw) {
  const out = { enabled: false, max: 0, options: [], unavailable: [] };
  if (!raw || raw.enabled !== true) return out;
  out.enabled = true;
  out.max = Number.isInteger(raw.objective_max_bytes) && raw.objective_max_bytes > 0 ? raw.objective_max_bytes : 0;
  const str = (v) => (typeof v === "string" ? v.trim() : "");
  const secs = (v) => (v == null ? null : Number.isInteger(v) && v > 0 ? v : NaN);
  for (const p of Array.isArray(raw.projects) ? raw.projects : []) {
    const id = str(p && p.id);
    const label = str(p && p.label);
    if (!RUN_ID.test(id) || !label) {
      out.unavailable.push({ id: "", label: label || "Unnamed policy", reason: UNNAMED });
      continue;
    }
    if (p.eligible !== true) {
      out.unavailable.push({ id, label, reason: reasonText(p.reason) });
      continue;
    }
    const b = p.budget && typeof p.budget === "object" ? p.budget : {};
    // Scopes are literal Git paths: never trim, so what is reviewed is what the policy holds.
    const scopes = Array.isArray(p.scopes) ? p.scopes : [];
    const limits = { total: secs(b.worker_timeout_secs), startup: secs(b.worker_startup_secs), idle: secs(b.worker_idle_secs), gate: secs(b.gate_timeout_secs) };
    const revision = typeof p.revision === "string" ? p.revision : "";
    const tasks = Number.isInteger(b.tasks) ? b.tasks : 0;
    const worktree = p.worktree === true;
    // Unset limits are shown as unset, not rejected: the server is the authority. Only a
    // malformed value, or a worker with neither a total nor a watchdog, is not reviewable.
    const ok =
      revision &&
      tasks >= 1 &&
      tasks <= 16 &&
      scopes.length > 0 &&
      scopes.every((s) => typeof s === "string" && s.length > 0) &&
      limits.gate !== null && // the engine always runs a hard gate limit, so a missing one is not reviewable
      Object.values(limits).every((v) => !Number.isNaN(v)) &&
      (limits.startup === null) === (limits.idle === null) &&
      (limits.total !== null || limits.startup !== null);
    if (!ok) out.unavailable.push({ id, label, reason: INCOMPLETE });
    else out.options.push({ id, label, scopes, limits, revision, tasks, worktree });
  }
  return out;
}

const WIRE_MAX_BYTES = 8 * 1024; // web.rs MAX_BODY: the whole JSON request, escapes included

/** "" when the server would accept this objective, else why not. Mirrors project.rs `prepare`
 *  (trim, non-empty, UTF-8 byte cap, C0/C1 controls refused except \n and \t) plus web.rs's
 *  8 KiB body cap, which JSON escaping (each \n is two bytes) can reach before the 4 KiB one. */
function objectiveGap(text) {
  const t = text.trim();
  if (!t) return "an objective is required";
  try {
    encodeURIComponent(t); // throws on a lone surrogate, which serde_json would refuse
  } catch (_) {
    return "the objective is not valid Unicode text";
  }
  if (/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/.test(t)) return "the objective contains control characters";
  if (utf8(t) > state.launch.max) return "the objective is too large";
  const o = selectedOption();
  if (utf8(JSON.stringify({ project: o ? o.id : "", objective: t, revision: o ? o.revision : "" })) > WIRE_MAX_BYTES) return "the request would exceed the 8 KiB wire limit once JSON-escaped; shorten it or use fewer line breaks";
  return "";
}

// A 409 that names one of these is a known refusal before any run was created.
const REFUSED_409 = new Set(["busy", "stale_revision", "ineligible", "unavailable"]);
// After these the server may already have created the run, so a blind retry could start two:
// any 5xx (a gateway may answer for a server that went on), an unreadable reply, a lost
// connection after the POST left, or a 409 whose kind is not a known refusal.
const launchUncertain = (e) =>
  e.status >= 500 || e.kind === "bad_reply" || (e.kind === "network" && e.sent === true) || (e.status === 409 && !REFUSED_409.has(e.kind));

async function launchRun(opt, objective) {
  const L = state.launch;
  if (L.sending) throw new ApiError("busy", "A start request is already open.");
  L.sending = true;
  try {
    const r = await post("/api/runs", { project: opt.id, objective, revision: opt.revision }, "launch", 202);
    if (!r || typeof r.id !== "string" || !RUN_ID.test(r.id)) throw new ApiError("bad_reply", "The server did not return a usable run id.");
    return r.id;
  } finally {
    L.sending = false;
  }
}

const selectedOption = () => state.launch.opts.find((o) => o.id === state.launch.sel) || null;

const scopeText = (scopes, sep) => scopes.map((s) => JSON.stringify(s)).join(sep);
const workerLimits = (o) => `worker total ${secsLabel(o.limits.total)}, startup ${secsLabel(o.limits.startup)}, idle ${secsLabel(o.limits.idle)} · gate ${secsLabel(o.limits.gate)}`;

function launchRows(o, objective) {
  const l = o.limits;
  const taskWord = o.tasks === 1 ? "1 task" : `${o.tasks} serial tasks (fresh worker session each, with committed-task handoff)`;
  const checkoutMode = o.worktree
    ? "The run executes in a fresh isolated Git worktree branch; the base checkout is not modified, and the worktree is kept on failure or stop."
    : "The worker makes changes in the approved checkout in place; edits from a failed or stopped run are kept, not rolled back.";
  return [
    ["Policy", o.label],
    ["Allowed commit scope", `${scopeText(o.scopes, "\n")}\nExact paths tasks may commit, quoted so spaces show. This is not a filesystem sandbox.`],
    ["Tasks", `${taskWord}. Your objective is appended to each task prompt. ${checkoutMode}`],
    ["Worker total limit", secsLabel(l.total, l.startup !== null ? "watchdog-only, no overall cap" : "no overall cap")],
    ["Worker startup limit", secsLabel(l.startup, "no watchdog")],
    ["Worker idle limit", secsLabel(l.idle, "no watchdog")],
    ["Gate limit", secsLabel(l.gate)],
    ["Policy revision", o.revision],
    ["Objective", objective],
  ];
}

function renderLaunch() {
  const L = state.launch;
  const nr = $("new-run");
  nr.setAttribute("aria-disabled", String(L.sending));
  nr.textContent = L.sending ? "Starting…" : "New run";
  if (!$("ldlg").open) return;
  const loaded = L.status === "ready" && !L.readOnly && L.enabled && L.max > 0;
  const ready = loaded && L.opts.length > 0;
  const review = ready && L.step === "review" && !!selectedOption();
  $("ldlg-lede").textContent = review
    ? "Check what the server will run. Nothing starts until you confirm."
    : "Choose one of the checkout policies an operator has approved, then describe the objective.";
  if (L.status === "loading") setState("ldlg-state", "loading", "Loading approved policies…");
  else if (L.status === "error") {
    const b = el("button", "btn ghost small", "Retry");
    b.type = "button";
    b.addEventListener("click", loadLaunchOptions);
    $("ldlg-state").dataset.kind = "error";
    $("ldlg-state").replaceChildren(document.createTextNode(`Could not load approved policies: ${L.error ? L.error.message : "unknown error"}`), document.createElement("br"), b);
  } else if (L.readOnly) setState("ldlg-state", "empty", "This console is read-only; runs cannot be started from it.");
  else if (!L.enabled) setState("ldlg-state", "empty", LAUNCH_OFF);
  else if (L.max <= 0) setState("ldlg-state", "empty", "The server did not report an objective size limit, so this page cannot check one. Reload and try again.");
  else if (!L.opts.length && !L.unavail.length) setState("ldlg-state", "empty", LAUNCH_EMPTY);
  else if (!L.opts.length) setState("ldlg-state", "empty", "No policy can be started right now. Each one below says why.");
  else setState("ldlg-state", null);

  $("ldlg-form").hidden = !loaded || review;
  $("ldlg-review").hidden = !review;
  $("ldlg-opts").hidden = !ready;
  $("ldlg-objwrap").hidden = !ready;
  $("ldlg-offwrap").hidden = !loaded || review || !L.unavail.length;
  if (loaded) {
    rebuild($("ldlg-optlist"), JSON.stringify(L.opts), () =>
      L.opts.map((o) => {
        const lab = el("label", "opt");
        const r = el("input");
        r.type = "radio";
        r.name = "ldlg-opt";
        r.value = o.id;
        r.dataset.fk = o.id;
        r.checked = o.id === L.sel;
        const taskCount = o.tasks === 1 ? "1 task" : `${o.tasks} serial tasks`;
        const wtTag = o.worktree ? " · isolated worktree" : "";
        lab.append(r, document.createTextNode(o.label), el("span", "opt-meta", `commit scope ${scopeText(o.scopes, ", ")} · ${taskCount}${wtTag} · ${workerLimits(o)}`));
        return lab;
      }),
    );
    rebuild($("ldlg-off"), JSON.stringify(L.unavail), () =>
      L.unavail.map((u) => {
        const li = el("li", "off");
        li.append(el("span", "off-name", u.label), badge("not selectable", "warn"), el("span", "opt-meta", u.reason));
        return li;
      }),
    );
  }
  const obj = $("ldlg-obj").value;
  const gap = L.max > 0 ? objectiveGap(obj) : "";
  const shown = !!obj.trim() && !!gap;
  $("ldlg-count").textContent = `${utf8(obj.trim())} / ${L.max} bytes${shown ? ` · ${gap}` : ""}`;
  $("ldlg-count").classList.toggle("over", shown);
  $("ldlg-obj").setAttribute("aria-invalid", String(shown));
  if (review) {
    const o = selectedOption();
    rebuild($("ldlg-facts"), JSON.stringify([o, obj.trim()]), () =>
      launchRows(o, obj.trim()).map(([k, v]) => {
        const row = el("div");
        row.append(el("dt", null, k), el("dd", null, v));
        return row;
      }),
    );
  }
  const p = $("ldlg-primary");
  p.hidden = !ready;
  p.textContent = review ? (L.sending ? "Starting…" : L.locked ? "Start locked" : "Start run") : "Review";
  const blocked = review ? !$("ldlg-ack").checked || L.sending || L.locked : !selectedOption() || !!gap;
  p.setAttribute("aria-disabled", String(blocked));
  $("ldlg-back").hidden = !review || L.sending || L.locked;
  $("ldlg-cancel").textContent = L.locked ? "Close and check Runs" : "Cancel";
}

async function loadLaunchOptions() {
  const L = state.launch;
  const seq = ++L.seq;
  L.status = "loading";
  renderLaunch();
  try {
    const [cfg, raw] = await Promise.all([api("/api/config"), api("/api/launch-options").catch((e) => (e.status === 404 ? null : Promise.reject(e)))]);
    if (seq !== L.seq) return;
    state.auth = cfg.auth || null;
    L.readOnly = cfg.read_only === true;
    const n = normalizeOptions(raw);
    Object.assign(L, { enabled: n.enabled, max: n.max, opts: n.options, unavail: n.unavailable, status: "ready" });
    if (L.sel && !L.opts.some((o) => o.id === L.sel)) {
      L.sel = null;
      L.step = "form";
    }
  } catch (e) {
    if (seq !== L.seq) return;
    L.status = "error";
    L.error = e;
  }
  renderLaunch();
  const a = document.activeElement;
  if ($("ldlg").open && L.status === "ready" && (a === $("ldlg-title") || a === $("ldlg"))) {
    (document.querySelector('#ldlg input[name="ldlg-opt"]:checked') || document.querySelector('#ldlg input[name="ldlg-opt"]') || $("ldlg-title")).focus();
  }
}

function openLaunch(invoker) {
  const L = state.launch;
  if (L.sending || $("ldlg").open) return;
  Object.assign(L, { invoker, step: "form", locked: false, done: false });
  $("ldlg-ack").checked = false;
  $("ldlg-err").hidden = true;
  $("ldlg").showModal();
  $("ldlg-title").focus();
  loadLaunchOptions();
}

function closeLaunch() {
  if ($("ldlg").open) $("ldlg").close();
}

async function submitLaunch() {
  const L = state.launch;
  const o = selectedOption();
  const objective = $("ldlg-obj").value.trim();
  if (!o || L.sending || L.locked || L.step !== "review" || !$("ldlg-ack").checked || objectiveGap(objective)) return;
  $("ldlg-err").hidden = true;
  const p = launchRun(o, objective); // sets L.sending synchronously
  renderLaunch();
  try {
    const id = await p;
    L.done = true;
    $("ldlg-obj").value = "";
    L.sel = null;
    closeLaunch();
    renderLaunch();
    location.hash = `#/run/${enc(id)}`; // hashchange routes, refreshes and focuses the run
    return;
  } catch (e) {
    // 504 launch_pending and every other outcome we cannot read leave the run's existence
    // unknown: lock Start and send the operator to the Runs list. Nothing is resubmitted here.
    const pending = e.status === 504;
    const unsure = pending || launchUncertain(e);
    if (unsure) L.locked = true;
    let msg;
    if (pending) {
      msg = `The server has not confirmed the run yet (${e.kind === "launch_pending" ? "launch_pending" : `HTTP 504, ${e.kind}`}). It may still start. Check the Runs list before trying again; this page will not retry for you.`;
    } else {
      msg = `Start failed (${e.kind}): ${e.message}`;
      if (unsure) msg += " The run may still have been created. Check the Runs list before trying again.";
    }
    if (unsure) msg += " Start stays locked until you close this dialog and open it again.";
    else if (e.kind === "stale_revision" || e.kind === "ineligible") {
      msg += " The approved policy list was reloaded. Review it again.";
      L.step = "form";
      $("ldlg-ack").checked = false;
      loadLaunchOptions();
    } else if (e.kind === "busy") msg += " Your objective is kept; try again once it finishes.";
    else if (e.status === 413) msg += " Shorten the objective.";
    $("ldlg-err").textContent = msg;
    $("ldlg-err").hidden = false;
    renderLaunch();
    if (unsure) refresh();
  }
}

/* ───── wiring ───── */
$("new-run").addEventListener("click", (ev) => {
  if (ev.currentTarget.getAttribute("aria-disabled") !== "true") openLaunch(ev.currentTarget);
});
$("ldlg-optlist").addEventListener("change", (ev) => {
  state.launch.sel = ev.target.value;
  renderLaunch();
});
$("ldlg-obj").addEventListener("input", renderLaunch);
$("ldlg-ack").addEventListener("change", renderLaunch);
$("ldlg-primary").addEventListener("click", (ev) => {
  const L = state.launch;
  if (ev.currentTarget.getAttribute("aria-disabled") === "true") return;
  if (L.step === "form") {
    L.step = "review";
    $("ldlg-err").hidden = true;
    renderLaunch();
    $("ldlg-ack").focus();
  } else submitLaunch();
});
$("ldlg-back").addEventListener("click", () => {
  state.launch.step = "form";
  $("ldlg-ack").checked = false;
  $("ldlg-err").hidden = true;
  renderLaunch();
  $("ldlg-obj").focus();
});
$("ldlg-cancel").addEventListener("click", () => {
  if (!state.launch.sending) closeLaunch();
});
$("ldlg").addEventListener("cancel", (ev) => {
  if (state.launch.sending) ev.preventDefault();
});
$("ldlg").addEventListener("close", () => {
  const L = state.launch;
  L.seq++; // drop any option load still in flight
  if (L.done) return;
  // The invoker can be hidden by the time the dialog closes (list pane at <960px); fall back to Refresh.
  L.invoker?.focus({ preventScroll: true });
  if (document.activeElement !== L.invoker) $("refresh").focus({ preventScroll: true });
});
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
