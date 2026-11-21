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
      createElement: extra.els ? () => ({ addEventListener(_, f) { this.click = f; } }) : node,
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
  vm.runInContext(fs.readFileSync(path.join(__dirname, "app.js"), "utf8") + "\n;globalThis.__t = { post, request, api, refresh, state };", ctx);
  const settle = async () => {
    for (let i = 0; i < 50 && ctx.__t.state.refreshing; i++) await new Promise((r) => setImmediate(r));
  };
  return { calls, timers, settle, ...ctx.__t, mutations: () => calls.filter((c) => c.opts.method === "POST") };
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
