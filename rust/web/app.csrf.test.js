// Run: node --test rust/web/app.csrf.test.js
// Loads app.js against a stub DOM and checks how mutations treat /api/config.
"use strict";
const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function load(config, extra = {}) {
  const calls = [];
  const timers = [];
  const els = extra.els || {};
  for (const e of Object.values(els)) e.addEventListener ||= () => {};
  const node = () =>
    new Proxy(function () {}, {
      get: (_, k) => (k === "children" ? [node(), node(), node()] : k === "dataset" ? {} : k === "classList" ? { toggle() {}, add() {}, remove() {} } : node()),
      set: () => true,
      apply: () => node(),
    });
  const respond = (body, status = 200, type = "basic") => ({
    ok: status < 400,
    status,
    type,
    text: async () => (typeof body === "string" ? body : JSON.stringify(body)),
  });
  const ctx = {
    console,
    document: {
      getElementById: (id) => els[id] || node(),
      querySelector: node,
      querySelectorAll: () => [],
      createElement: extra.createElement || (extra.els ? () => ({ addEventListener(_, f) { this.click = f; } }) : node),
      get activeElement() { return extra.activeElement; },
      createTextNode: extra.els ? (text) => ({ text }) : node,
      addEventListener() {},
      body: { dataset: {} },
      hidden: false,
    },
    location: { hash: extra.hash || "", replace() {}, assign: (u) => (extra.assigned = u) },
    matchMedia: () => ({ matches: false }),
    addEventListener() {},
    setTimeout: (f, ms) => timers.push(ms),
    clearTimeout() {},
    TextEncoder,
    fetch: async (url, opts = {}) => {
      calls.push({ url, opts });
      if (extra.fetch) return extra.fetch(url, opts, respond);
      if (url === "/api/config") return respond(config);
      if (url === "/api/runs") return respond({ runs: [] });
      return respond({ outcome: "ok", message: "done" });
    },
  };
  vm.createContext(ctx);
  vm.runInContext(fs.readFileSync(path.join(__dirname, "app.js"), "utf8") + "\n;globalThis.__t = { post, request, api, refresh, state, groupRuns, renderRuns, normalizeOptions, objectiveGap, launchRun, submitLaunch, loadLaunchOptions, launchRows };", ctx);
  const settle = async () => {
    for (let i = 0; i < 50 && ctx.__t.state.refreshing; i++) await new Promise((r) => setImmediate(r));
  };
  return { calls, timers, settle, location: ctx.location, ...ctx.__t, mutations: () => calls.filter((c) => c.opts.method === "POST") };
}

test("standalone Basic: no CSRF header, resume allowed", async () => {
  const t = load({ poll_seconds: 5, read_only: false });
  await t.request("r1", "resume");
  await t.request("r1", "pause");
  assert.equal(t.mutations().length, 2);
  for (const m of t.mutations()) assert.equal(m.opts.headers["X-CSRF-Token"], undefined);
});

test("oidc: CSRF fetched per mutation and sent as header only", async () => {
  const t = load({ auth: "oidc", csrf: "tok-1", read_only: false });
  await t.request("r1", "stop");
  await t.request("r1", "approve", "n");
  const m = t.mutations();
  assert.equal(m.length, 2);
  for (const x of m) {
    assert.equal(x.opts.headers["X-CSRF-Token"], "tok-1");
    assert.ok(!x.url.includes("tok-1") && !String(x.opts.body).includes("tok-1"));
  }
  // one startup read for the auth mode, then a fresh read per mutation
  assert.equal(t.calls.filter((c) => c.url === "/api/config").length, 3);
});

test("oidc: missing csrf fails closed, no POST", async () => {
  for (const cfg of [{ auth: "oidc", read_only: false }, { auth: "oidc", csrf: "", read_only: false }]) {
    const t = load(cfg);
    await assert.rejects(t.request("r1", "stop"), { kind: "csrf_missing" });
    assert.equal(t.mutations().length, 0);
  }
});

test("oidc: resume refused client-side", async () => {
  const t = load({ auth: "oidc", csrf: "tok", read_only: false });
  await assert.rejects(t.request("r1", "resume"), { kind: "forbidden" });
  assert.equal(t.mutations().length, 0);
});

test("read_only config blocks mutation", async () => {
  const t = load({ auth: "oidc", csrf: "tok", read_only: true });
  await assert.rejects(t.request("r1", "stop"), { kind: "read_only" });
  assert.equal(t.mutations().length, 0);
});

const failing = (status, body, type) => ({ fetch: (url, _o, respond) => respond(body, status, type) });

test("edge error shapes stay readable", async () => {
  const t = load({ auth: "oidc" }, failing(403, { error: "csrf token mismatch" }));
  t.state.auth = "oidc";
  await assert.rejects(t.api("/x"), { message: "csrf token mismatch", status: 403 });
  for (const [body, msg] of [[{ message: "denied" }, "denied"], ["plain text", "plain text"], ["<html>x</html>", "HTTP 502"]]) {
    const u = load({}, failing(502, body));
    await assert.rejects(u.api("/x"), { message: msg });
  }
});

test("expired OIDC session: redirect or 401 gives sign-in recovery; Basic 401 untouched", async () => {
  const r = load({ auth: "oidc" }, failing(0, "", "opaqueredirect"));
  await assert.rejects(r.api("/x"), { kind: "session_expired" });
  assert.equal(r.calls.at(-1).opts.redirect, "manual");
  const e = load({ auth: "oidc" }, failing(401, "<html>login</html>"));
  e.state.auth = "oidc";
  await assert.rejects(e.api("/x"), { kind: "session_expired" });
  const b = load({}, failing(401, { error: { kind: "unauthorized", message: "authentication required" } }));
  b.state.auth = null;
  await assert.rejects(b.api("/x"), { kind: "unauthorized", message: "authentication required" });
});

test("expired session offers a Sign in again button that goes to /", async () => {
  const extra = { els: { "runs-state": { dataset: {}, replaceChildren(...k) { this.kids = k; }, removeAttribute() {} } }, ...failing(401, "", "opaqueredirect") };
  const t = load({}, extra);
  await t.settle();
  const btn = extra.els["runs-state"].kids.at(-1);
  assert.equal(btn.textContent, "Sign in again");
  btn.click();
  assert.equal(extra.assigned, "/");
});

test("invalid or missing run detail reads as no such run", async () => {
  for (const status of [400, 404]) {
    const extra = {
      hash: "#/run/bad..id",
      els: { "detail-state": { dataset: {}, replaceChildren(...k) { this.kids = k; }, removeAttribute() {} } },
      fetch: (url, _o, respond) => (url.startsWith("/api/runs/") ? respond({ error: { kind: status === 400 ? "usage" : "no_run", message: "x" } }, status) : respond({ runs: [] })),
    };
    const t = load({}, extra);
    await t.settle();
    assert.equal(extra.els["detail-state"].kids[0].text, "No such run: bad..id. It may have been removed.");
  }
});

test("refresh recovers lock and timer after a render error", async () => {
  const els = { "runs-count": { set textContent(_) { throw new Error("render boom"); } } };
  const t = load({}, { els, fetch: (url, _o, respond) => respond(url === "/api/runs" ? { runs: [] } : {}) });
  await t.settle();
  assert.equal(t.state.refreshing, false);
  assert.ok(t.timers.length > 0, "next poll scheduled");
  assert.ok(t.state.fails > 0);
});

/* ───── New run ───── */
// Shape of rust/src/project.rs `Registry::options`.
const BUDGET = { tasks: 1, worker_timeout_secs: 5400, worker_startup_secs: null, worker_idle_secs: null, gate_timeout_secs: 600 };
const P = { id: "lmloop-main", label: "lmloop main", eligible: true, scopes: ["rust/web"], budget: BUDGET, revision: "r7" };
const wrap = (...projects) => ({ enabled: true, objective_max_bytes: 4096, projects });

test("launch options: disabled, eligible, ineligible and malformed entries", () => {
  const t = load({});
  assert.deepEqual(t.normalizeOptions({ enabled: false, projects: [] }), { enabled: false, max: 0, options: [], unavailable: [] });
  for (const junk of [null, undefined, [], "x", { projects: [P] }]) assert.equal(t.normalizeOptions(junk).enabled, false);

  const n = t.normalizeOptions(
    wrap(P, { id: "dirty", label: "Dirty", eligible: false, reason: "the checkout has uncommitted changes" }, { id: "x", label: "No reason", eligible: false }, { id: "../bad", label: "Bad id", eligible: true }),
  );
  assert.equal(n.max, 4096);
  assert.deepEqual(n.options.map((o) => o.id), ["lmloop-main"]);
  assert.deepEqual(n.unavailable.map((u) => [u.id, u.reason]), [
    ["dirty", "the checkout has uncommitted changes"],
    ["x", "No reason was given."],
    ["", "The server sent an entry this page cannot identify safely."],
  ]);
  // an ineligible entry never becomes selectable, even when it carries a revision and budget
  assert.equal(t.normalizeOptions(wrap({ ...P, eligible: false, reason: "no" })).options.length, 0);
  assert.equal(t.normalizeOptions({ enabled: true, projects: [P] }).max, 0);
});

test("launch options: unset limits stay unset; malformed eligible entries become unavailable", () => {
  const t = load({});
  const dog = { ...BUDGET, tasks: 4, worker_timeout_secs: null, worker_startup_secs: 1200, worker_idle_secs: 300 };
  const [o] = t.normalizeOptions(wrap({ ...P, worktree: true, budget: dog })).options;
  assert.deepEqual({ ...o.limits }, { total: null, startup: 1200, idle: 300, gate: 600 });
  assert.equal(o.tasks, 4);
  assert.equal(o.worktree, true);
  const rows = t.launchRows(o, "ship it");
  const taskRow = rows.find(([k]) => k === "Tasks")[1];
  const totalRow = rows.find(([k]) => k === "Worker total limit")[1];
  assert.match(taskRow, /4 serial tasks/);
  assert.match(taskRow, /isolated Git worktree/);
  assert.match(totalRow, /watchdog-only/);
  const bad = [
    { ...BUDGET, gate_timeout_secs: null }, // the backend always has a hard gate limit
    { ...BUDGET, gate_timeout_secs: undefined },
    { ...BUDGET, tasks: 0 },
    { ...BUDGET, tasks: 17 },
    { ...BUDGET, worker_timeout_secs: 0 },
    { ...BUDGET, worker_timeout_secs: "9" },
    { ...BUDGET, worker_timeout_secs: null }, // neither a total nor a watchdog
    { ...BUDGET, worker_startup_secs: 5 }, // watchdog halves must come together
  ];
  for (const budget of bad) assert.deepEqual(t.normalizeOptions(wrap({ ...P, budget })).options, [], JSON.stringify(budget));
  for (const patch of [{ scopes: [] }, { scopes: [""] }, { revision: "" }, { revision: 3 }, { budget: null }]) {
    const n = t.normalizeOptions(wrap({ ...P, ...patch }));
    assert.equal(n.options.length, 0);
    assert.equal(n.unavailable.length, 1);
  }
});

test("launch options: scope strings are kept exactly, spaces included, and shown quoted", () => {
  const t = load({});
  const scopes = [" lead.txt", "trail.txt ", "in ner.txt"];
  const [o] = t.normalizeOptions(wrap({ ...P, scopes })).options;
  assert.deepEqual([...o.scopes], scopes);
  assert.equal(t.normalizeOptions(wrap({ ...P, scopes: ["   "] })).options.length, 1, "whitespace-only is still a literal path");
  assert.equal(t.normalizeOptions(wrap({ ...P, scopes: [""] })).options.length, 0);
  assert.equal(t.normalizeOptions(wrap({ ...P, scopes: [7] })).options.length, 0);
});

test("objective gap mirrors the Rust limits", () => {
  const t = load({});
  t.state.launch.max = 4096;
  assert.match(t.objectiveGap("  "), /required/);
  assert.equal(t.objectiveGap("line one\n\tline two"), "", "newline and tab are allowed");
  for (const c of ["\u0000", "\u0007", "\u000b", "\u001b", "\u007f", "\u0085", "\u009f"]) assert.match(t.objectiveGap(`a${c}b`), /control/, JSON.stringify(c));
  assert.match(t.objectiveGap("a\ud800b"), /Unicode/);
  assert.equal(t.objectiveGap("é".repeat(2048)), "", "4096 UTF-8 bytes is the limit, inclusive");
  assert.match(t.objectiveGap("é".repeat(2049)), /too large/, "counted in bytes, not characters");
  assert.match(t.objectiveGap(`${"a".repeat(4095)}${"\n"}z`), /too large/);
  // 4000 tabs escape to ~8.05 KiB: inside the 4 KiB objective cap and the 8 KiB wire cap
  assert.equal(t.objectiveGap(`a${"\t".repeat(4000)}a`), "");
  // 4094 tabs are exactly 4096 objective bytes (allowed) but escape past the 8 KiB wire cap
  assert.match(t.objectiveGap(`a${"\t".repeat(4094)}a`), /8 KiB/);
});

const opt = () => ({ id: "lmloop-main", revision: "r7" });
const accepted = { fetch: (url, o, respond) => (url === "/api/config" ? respond({ auth: "oidc", csrf: "tok-9" }) : respond({ id: "run-9" }, 202)) };

test("launch: body is only project, objective, revision; CSRF sent under oidc", async () => {
  const t = load({}, accepted);
  assert.equal(await t.launchRun(opt(), "ship it"), "run-9");
  const [m] = t.mutations();
  assert.equal(m.url, "/api/runs");
  assert.deepEqual(JSON.parse(m.opts.body), { project: "lmloop-main", objective: "ship it", revision: "r7" });
  assert.equal(m.opts.headers["X-CSRF-Token"], "tok-9");
  assert.equal(t.state.launch.sending, false);
});

test("launch: oidc without csrf and read_only both fail closed, no POST", async () => {
  for (const cfg of [{ auth: "oidc" }, { read_only: true }]) {
    const t = load(cfg);
    await assert.rejects(t.launchRun(opt(), "x"));
    assert.equal(t.mutations().length, 0);
    assert.equal(t.state.launch.sending, false);
  }
});

test("launch: a second submit while one is open sends nothing", async () => {
  let release;
  const gate = new Promise((r) => (release = r));
  const t = load({}, { fetch: async (url, o, respond) => (url === "/api/config" ? respond({}) : (await gate, respond({ id: "run-1" }, 202))) });
  const first = t.launchRun(opt(), "a");
  await assert.rejects(t.launchRun(opt(), "b"), { kind: "busy" });
  release();
  assert.equal(await first, "run-1");
  assert.equal(t.mutations().length, 1);
});

test("launch: only an actual 202 counts as accepted", async () => {
  for (const status of [200, 201, 204]) {
    const { t } = submitFixture(reply(status, { id: "run-9" }));
    await t.submitLaunch();
    assert.equal(t.location.hash, "", String(status));
    assert.equal(t.state.launch.locked, true, String(status));
    assert.equal(t.calls.filter((c) => c.opts.method === "POST").length, 1);
  }
});

test("launch: reply without a usable id is rejected", async () => {
  for (const body of [{}, { id: "" }, { id: "../x" }, { id: 7 }]) {
    const t = load({}, { fetch: (url, o, respond) => (url === "/api/config" ? respond({}) : respond(body, 202)) });
    await assert.rejects(t.launchRun(opt(), "x"), { kind: "bad_reply" });
  }
});

function submitFixture(fetchImpl, extra = {}) {
  const els = { ldlg: { open: false, close() {} }, "ldlg-ack": { checked: true }, "ldlg-obj": { value: " do it " }, "ldlg-err": {}, ...extra };
  const t = load({}, { els, fetch: fetchImpl });
  const limits = { total: 60, startup: null, idle: null, gate: 30 };
  Object.assign(t.state.launch, { enabled: true, max: 4096, opts: [{ id: "lmloop-main", label: "L", scopes: ["a"], limits, revision: "r7" }], sel: "lmloop-main", step: "review", status: "ready" });
  return { t, els };
}
const reply = (status, body) => (url, o, respond) => (url === "/api/config" ? respond({}) : respond(body, status));

test("launch: navigates to the run only after an accepted id", async () => {
  const { t, els } = submitFixture(accepted.fetch);
  await t.submitLaunch();
  assert.equal(t.location.hash, "#/run/run-9");
  assert.equal(JSON.parse(t.mutations()[0].opts.body).objective, "do it");
  assert.equal(els["ldlg-obj"].value, "");
});

test("launch: an invalid objective is never sent", async () => {
  const { t } = submitFixture(accepted.fetch, { "ldlg-obj": { value: "bad\u0000" } });
  await t.submitLaunch();
  assert.equal(t.mutations().length, 0);
});

test("launch: a known refusal keeps the dialog and does not lock", async () => {
  const { t, els } = submitFixture(reply(403, { error: { kind: "refused", message: "nope" } }));
  await t.submitLaunch();
  assert.equal(t.location.hash, "");
  assert.match(els["ldlg-err"].textContent, /refused.*nope/);
  assert.equal(t.state.launch.locked, false);
  assert.equal(t.state.launch.sending, false);
});

test("launch: 409 busy keeps the form and objective, no forced re-review", async () => {
  const { t, els } = submitFixture(reply(409, { error: { kind: "busy", message: "a launch or resume is already running in this server" } }));
  await t.submitLaunch();
  assert.equal(t.state.launch.locked, false);
  assert.equal(t.state.launch.step, "review");
  assert.equal(els["ldlg-obj"].value, " do it ");
  assert.match(els["ldlg-err"].textContent, /objective is kept/);
  assert.equal(t.calls.filter((c) => c.url === "/api/launch-options").length, 0, "no reload");
});

test("launch: stale_revision and ineligible send the operator back to review", async () => {
  for (const kind of ["stale_revision", "ineligible"]) {
    const { t, els } = submitFixture((url, o, respond) => (url === "/api/config" ? respond({}) : url === "/api/runs" ? respond({ error: { kind, message: "changed" } }, 409) : respond({ enabled: true, objective_max_bytes: 4096, projects: [] })));
    await t.submitLaunch();
    assert.equal(t.state.launch.step, "form", kind);
    assert.equal(els["ldlg-ack"].checked, false, kind);
    assert.equal(t.state.launch.locked, false, kind);
    assert.match(els["ldlg-err"].textContent, /Review it again/);
    await t.settle();
    assert.equal(t.calls.filter((c) => c.url === "/api/launch-options").length, 1, kind);
  }
});

test("launch: 504 launch_pending locks Start, names Runs, and is never resubmitted", async () => {
  const { t, els } = submitFixture(reply(504, { error: { kind: "launch_pending", message: "the launch has not created its run yet; check the run list before retrying" } }));
  await t.submitLaunch();
  assert.equal(t.state.launch.locked, true);
  assert.equal(t.location.hash, "");
  assert.match(els["ldlg-err"].textContent, /Check the Runs list before trying again/);
  assert.match(els["ldlg-err"].textContent, /will not retry for you/);
  await t.submitLaunch();
  await t.submitLaunch();
  assert.equal(t.mutations().length, 1);
});

test("launch: ambiguous 5xx (502, 503, 500, html, untyped) is treated as possibly accepted", async () => {
  for (const [status, body] of [[502, "<html>bad gateway</html>"], [503, { error: "x" }], [500, { error: { kind: "internal", message: "m" } }], [504, "<html>"], [502, { error: { kind: "upstream", message: "m" } }]]) {
    const { t, els } = submitFixture(reply(status, body));
    await t.submitLaunch();
    assert.equal(t.state.launch.locked, true, `${status} ${JSON.stringify(body)}`);
    assert.match(els["ldlg-err"].textContent, /Runs list/);
    await t.submitLaunch();
    assert.equal(t.mutations().length, 1);
  }
});

test("launch: unreadable 202 and a connection lost after sending lock retry", async () => {
  const lost = (url, o, respond) => {
    if (url === "/api/config") return respond({});
    throw new TypeError("connection reset");
  };
  for (const impl of [reply(202, "<html>"), lost]) {
    const { t, els } = submitFixture(impl);
    await t.submitLaunch();
    assert.equal(t.location.hash, "");
    assert.equal(t.state.launch.locked, true);
    assert.match(els["ldlg-err"].textContent, /may still have been created/);
    await t.submitLaunch();
    assert.equal(t.calls.filter((c) => c.opts.method === "POST").length, 1);
  }
});

test("launch: a network failure before the POST (config read) does not lock", async () => {
  const { t } = submitFixture(() => {
    throw new TypeError("down");
  });
  await t.submitLaunch();
  assert.equal(t.state.launch.locked, false);
  assert.equal(t.calls.filter((c) => c.opts.method === "POST").length, 0);
});

test("launch: missing /api/launch-options reads as disabled", async () => {
  const els = { ldlg: { open: false } };
  const t = load({}, { els, fetch: (url, o, respond) => (url === "/api/config" ? respond({}) : respond({ error: { kind: "not_found", message: "x" } }, 404)) });
  await t.loadLaunchOptions();
  assert.equal(t.state.launch.status, "ready");
  assert.equal(t.state.launch.enabled, false);
  assert.deepEqual(t.state.launch.opts, []);
});

/* ───── run grouping ───── */
// Minimal element tree: enough to read back what renderRuns built.
function fakeEl(tag) {
  return {
    tag, className: "", dataset: {}, attrs: {}, kids: [], _text: "",
    set textContent(v) { this._text = String(v); },
    get textContent() { return this._text + this.kids.map((k) => k.textContent).join(""); },
    setAttribute(k, v) { this.attrs[k] = v; },
    append(...k) { this.kids.push(...k); },
    replaceChildren(...k) { this.kids = k; },
    contains(n) { return n === this || this.kids.some((k) => k.contains && k.contains(n)); },
    querySelectorAll() { const out = []; walk(this, (n) => n.dataset && n.dataset.fk && out.push(n)); return out; },
    focus() { this.focused = true; },
  };
}
const run = (id, phase, extra = {}) => ({ run_id: id, kind: "serial_fixture", phase, owner_lost: false, tasks_done: 0, tasks_total: 1, lease: { state: "free" }, pending_controls: [], ...extra });
const walk = (n, f) => { f(n); for (const k of n.kids || []) walk(k, f); };

test("run grouping: phase decides the group, server order is kept, nothing is dropped", () => {
  const t = load({});
  const runs = [
    run("a", "worker"), run("b", "complete"), run("c", "paused"), run("d", "failed"), run("e", "gate_running", { kind: "diagnostic_gate" }),
    run("f", "interrupted", { kind: "diagnostic_gate" }), run("g", "invalid", { kind: "invalid" }), run("h", "stopped"), run("i", "idle"), run("j", "committing"), run("k", "brand_new_phase"),
  ];
  const g = t.groupRuns(runs);
  assert.deepEqual(g.active.map((r) => r.run_id), ["a", "c", "e", "i", "j"]);
  assert.deepEqual(g.history.map((r) => r.run_id), ["b", "d", "f", "g", "h", "k"]);
  // owner_lost does not move a run: it is still the phase it was persisted in
  const lost = t.groupRuns([run("x", "worker", { owner_lost: true }), run("y", "complete", { owner_lost: true })]);
  assert.deepEqual([lost.active.length, lost.history.length], [1, 1]);
  // the 100-run cap is the server's; grouping keeps every row it was given
  const many = Array.from({ length: 100 }, (_, i) => run(`r${i}`, i % 3 ? "complete" : "paused"));
  const m = t.groupRuns(many);
  assert.equal(m.active.length + m.history.length, 100);
});

test("run list renders labelled groups, truthful badges, and keeps focus keys across a regroup", () => {
  const box = fakeEl("div");
  const note = fakeEl("span");
  const extra = { els: { runs: box, "runs-count": note }, createElement: fakeEl };
  const t = load({}, extra);
  t.state.runs = [run("p1", "paused", { pending_controls: ["STOP"] }), run("l1", "worker", { owner_lost: true, lease: { state: "stale", pid: 7 } }), run("h1", "complete")];
  t.state.selected = "l1";
  t.renderRuns();
  const groups = box.kids;
  assert.deepEqual(groups.map((x) => x.kids[0].textContent), ["Active2", "History1"]);
  const rows = (grp) => grp.kids[2].kids.map((li) => li.kids[0]);
  assert.deepEqual(rows(groups[0]).map((a) => a.dataset.fk), ["p1", "l1"]);
  assert.deepEqual(rows(groups[1]).map((a) => a.dataset.fk), ["h1"]);
  const [, lostRow] = rows(groups[0]);
  assert.equal(lostRow.attrs["aria-current"], "true");
  const labels = [];
  walk(lostRow, (n) => n.className.startsWith("badge") && labels.push(n.textContent));
  assert.deepEqual(labels, ["worker", "owner lost"]);
  const text = (a) => a.textContent;
  assert.match(text(rows(groups[0])[0]), /paused/);
  assert.match(text(rows(groups[0])[0]), /pending STOP/);
  assert.match(text(lostRow), /lease stale \(was pid 7\)/);
  // a poll that finishes the run regroups it; same list => no rebuild
  const before = box.kids;
  t.renderRuns();
  assert.equal(box.kids, before);
  t.state.runs = [run("p1", "paused", { pending_controls: ["STOP"] }), run("l1", "failed"), run("h1", "complete")];
  extra.activeElement = lostRow; // keyboard focus is on l1 while the poll moves it Active -> History
  t.renderRuns();
  const moved = box.querySelectorAll().find((n) => n.dataset.fk === "l1");
  assert.ok(moved !== lostRow && moved.focused, "focus follows the run into its new group");
  extra.activeElement = undefined;
  assert.deepEqual(box.kids.map((x) => x.kids[0].textContent), ["Active1", "History2"]);
  // an empty group is omitted rather than shown as a zero heading
  t.state.runs = [run("h1", "complete")];
  t.renderRuns();
  assert.deepEqual(box.kids.map((x) => x.kids[0].textContent), ["History1"]);
});
