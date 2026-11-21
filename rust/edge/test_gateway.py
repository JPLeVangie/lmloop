"""Contract tests for rust/edge/gateway.py: ordinary loopback HTTP fixtures only.

Run: python3 rust/edge/test_gateway.py     (no network beyond 127.0.0.1, no live service)

The real tracked OIDC class signs/verifies cookies and the real DeviceTokens
verify bearers.  The issuer round trip is stubbed (`oidc.callback`); that path
is tracked code the edge does not modify.  A real Rust `lmloop web` backend is
used when rust/target/debug/lmloop exists, otherwise that class is skipped.
"""
import sys

sys.dont_write_bytecode = True

import base64
import http.client
import json
import os
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gateway  # noqa: E402
from gateway import Config, Server  # noqa: E402

ORIGIN = "https://preview.example.test"
HOST = "preview.example.test"
SECRET = "Sess-Secret-0123456789-abcdefghijklmnop"
DEVICE = "Fx-device-token-0123456789-QRSTUVWX"
DEVICE2 = "Fx-other-device-9876543210-ZYXWVUTS"
OPERATOR, STRANGER = "op-sub-1", "someone-else"
os.environ["EDGE_TEST_DEVICE"] = DEVICE


class Backend(BaseHTTPRequestHandler):
    """Records every request; answers from `routes[(method, path)]`."""
    protocol_version = "HTTP/1.0"
    routes, seen = {}, []

    def log_message(self, *a):
        pass

    def answer(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n)
        Backend.seen.append({"method": self.command, "path": self.path, "headers": dict(self.headers), "body": body})
        status, ctype, data, extra = Backend.routes.get((self.command, self.path), (404, "application/json", b'{"error":{"kind":"no_run"}}', {}))
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        for k, v in extra.items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    do_GET = do_POST = answer


def serve(server):
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def make_oidc():
    from web import auth as web_auth
    return web_auth.OIDC("https://issuer.example.test", "cid", "csecret", ORIGIN, SECRET, 1)


def cookie_for(oidc, sub=OPERATOR, csrf="csrf-token-value-123", exp=None):
    value = oidc.sign({"sub": sub, "name": "Op", "email": "", "csrf": csrf, "exp": exp or time.time() + 300})
    return f"{gateway.SESSION_COOKIE}={value}"


class EdgeCase(unittest.TestCase):
    backend_port = 0
    basic = "Basic " + base64.b64encode(b"be-user:be-pass").decode()

    @classmethod
    def setUpClass(cls):
        cls.backend_srv = None
        cls.start_edge()

    @classmethod
    def start_edge(cls):
        if cls.backend_port == 0:
            cls.backend_srv = serve(ThreadingHTTPServer(("127.0.0.1", 0), Backend))
            cls.backend_port = cls.backend_srv.server_port
        cls.oidc = make_oidc()
        from web import device_auth
        cls.cfg = Config(ORIGIN, cls.backend_port, cls.basic, frozenset({OPERATOR}))
        cls.edge = serve(Server(("127.0.0.1", 0), cls.cfg, cls.oidc, device_auth.build("android=env:EDGE_TEST_DEVICE")))
        cls.port = cls.edge.server_port

    @classmethod
    def tearDownClass(cls):
        cls.edge.shutdown()
        if cls.backend_srv:
            cls.backend_srv.shutdown()

    def setUp(self):
        Backend.seen.clear()
        Backend.routes.clear()

    def call(self, method, path, headers=None, body=b"", host=HOST):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        c.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
        if host is not None:
            c.putheader("Host", host)
        for k, v in (headers or {}).items():
            c.putheader(k, v)
        if body or method == "POST":
            c.putheader("Content-Length", str(len(body)))
        c.endheaders(body)
        r = c.getresponse()
        data = r.read()
        c.close()
        return r.status, r, data

    def raw(self, text, encoding="utf-8"):
        s = socket.create_connection(("127.0.0.1", self.port), timeout=10)
        s.sendall(text.encode(encoding))
        s.shutdown(socket.SHUT_WR)   # a short body ends here instead of waiting out the read timeout
        data = b""
        while chunk := s.recv(4096):
            data += chunk
        s.close()
        return int(data.split(b" ", 2)[1])

    def raw_auth(self, value):   # a non-ASCII byte on the wire, as a client could send it
        return self.raw(f"GET /api/runs HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: {value}\r\n\r\n", encoding="latin-1")

    def session_headers(self, **kw):
        return {"Cookie": cookie_for(self.oidc, **kw)}

    def post_headers(self, csrf="csrf-token-value-123", **extra):
        h = {**self.session_headers(csrf=csrf), "Origin": ORIGIN, "X-CSRF-Token": csrf, "Content-Type": "application/json"}
        h.update(extra)
        return h


class TestReads(EdgeCase):
    def test_health_is_fixed_and_never_contacts_backend(self):
        s, r, d = self.call("GET", "/health")
        self.assertEqual((s, json.loads(d)), (200, {"status": "ok", "auth": "oidc", "oidc": True, "read_only": True}))
        self.assertEqual(Backend.seen, [])
        for k in ("Cache-Control", "X-Content-Type-Options", "X-Frame-Options", "Content-Security-Policy", "Referrer-Policy"):
            self.assertIsNotNone(r.getheader(k), k)
        self.assertEqual(r.getheader("Cache-Control"), "no-store")

    def test_foreign_or_missing_host_is_refused(self):
        for host in ("evil.example", "127.0.0.1:%d" % self.port, None):
            self.assertEqual(self.call("GET", "/health", host=host)[0], 421, host)

    def test_anonymous_is_denied_without_touching_backend(self):
        for p in ("/api/config", "/api/runs", "/api/runs/lmloop/r1", "/api/runs/r1", "/api/runs/r1/logs/x.log", "/app.js", "/app.css"):
            self.assertEqual(self.call("GET", p)[0], 401, p)
        s, r, _ = self.call("GET", "/")
        self.assertEqual((s, r.getheader("Location")), (302, "/login"))
        self.assertEqual(Backend.seen, [])

    def test_forged_tampered_expired_and_foreign_sessions_are_denied(self):
        good = cookie_for(self.oidc)
        for h in ({"Cookie": good[:-3] + "AAA"}, {"Cookie": f"{gateway.SESSION_COOKIE}=x.y"},
                  {"Cookie": "lmloop_session=" + good.split("=", 1)[1]},       # the legacy cookie name is not honoured
                  {"Cookie": cookie_for(self.oidc, exp=time.time() - 5)},
                  {"Cookie": cookie_for(self.oidc, sub=STRANGER)},
                  {"Cookie": cookie_for(self.oidc, sub="")}, {"Cookie": "\x01garbage; ;;="}):
            self.assertEqual(self.call("GET", "/api/runs", h)[0], 401, h)
        self.assertEqual(Backend.seen, [])

    def test_allowlist_is_checked_on_every_request(self):
        h = self.session_headers()
        Backend.routes[("GET", "/api/runs")] = (200, "application/json", b'{"runs":[]}', {})
        self.assertEqual(self.call("GET", "/api/runs", h)[0], 200)
        object.__setattr__(self.cfg, "subs", frozenset({"someone-new"}))
        try:
            self.assertEqual(self.call("GET", "/api/runs", h)[0], 401)
        finally:
            object.__setattr__(self.cfg, "subs", frozenset({OPERATOR}))

    def test_session_reads_and_config_shape(self):
        Backend.routes[("GET", "/api/config")] = (200, "application/json", b'{"poll_seconds":5.0,"read_only":false}', {})
        Backend.routes[("GET", "/api/runs")] = (200, "application/json", b'{"runs":[]}', {})
        Backend.routes[("GET", "/api/runs/lmloop/r1")] = (200, "application/json", b'{"route_id":"r1"}', {})
        Backend.routes[("GET", "/api/runs/r1")] = (200, "application/json", b'{"summary":{}}', {})
        Backend.routes[("GET", "/api/runs/r1/logs/gate.log?bytes=100")] = (200, "application/json", b'{"text":""}', {})
        Backend.routes[("GET", "/api/runs/r1/logs/gate.log")] = (200, "application/json", b'{"text":""}', {})
        Backend.routes[("GET", "/app.js")] = (200, "text/javascript; charset=utf-8", b"//js", {})
        h = self.session_headers()
        s, _, d = self.call("GET", "/api/config", h)
        cfg = json.loads(d)
        self.assertEqual((s, cfg["read_only"], cfg["csrf"], cfg["poll_seconds"]), (200, False, "csrf-token-value-123", 5.0))
        for p in ("/api/runs", "/api/runs/lmloop/r1", "/api/runs/r1", "/api/runs/r1/logs/gate.log",
                  "/api/runs/r1/logs/gate.log?bytes=100", "/app.js"):
            self.assertEqual(self.call("GET", p, h)[0], 200, p)

    def test_bearer_is_read_only_and_limited_to_the_android_reads(self):
        Backend.routes[("GET", "/api/config")] = (200, "application/json", b'{"poll_seconds":5.0,"read_only":false}', {})
        Backend.routes[("GET", "/api/runs")] = (200, "application/json", b'{"runs":[]}', {})
        Backend.routes[("GET", "/api/runs/lmloop/r1")] = (200, "application/json", b'{}', {})
        bh = {"Authorization": f"Bearer {DEVICE}"}
        s, _, d = self.call("GET", "/api/config", bh)
        cfg = json.loads(d)
        self.assertEqual((s, cfg["read_only"], cfg["csrf"]), (200, True, "disabled"))
        for p in ("/api/runs", "/api/runs/lmloop/r1"):
            self.assertEqual(self.call("GET", p, bh)[0], 200, p)
        for p in ("/api/runs/r1", "/api/runs/r1/logs/x.log", "/app.js", "/"):
            self.assertIn(self.call("GET", p, bh)[0], (401, 302), p)
        self.assertEqual(self.call("GET", "/api/runs", {"Authorization": f"Bearer {DEVICE}x"})[0], 401)
        self.assertEqual(self.call("GET", "/api/runs", {"Authorization": "Bearer be-pass"})[0], 401)
        self.assertEqual(self.call("GET", "/api/runs", {"Authorization": self.basic})[0], 401)
        for bad in ("Bearer \u00e9" + DEVICE, "Bearer " + DEVICE + "\u00e9", "Bearer", "Bearer ", "bearer " + DEVICE,
                    "Bearer  " + DEVICE, "Bearer " + "A" * 600):
            self.assertEqual(self.raw_auth(bad), 401, bad)

    def test_target_grammar_is_closed(self):
        h = self.session_headers()
        for p in ("/api/runs?x=1", "/api/runs?", "/api/config?a=b", "/api/runs/r1?x=1", "/api/runs/r1/logs/x.log?bytes=0",
                  "/api/runs/r1/logs/x.log?bytes=65537", "/api/runs/r1/logs/x.log?bytes=1&x=2", "/api/runs/r1/logs/x.log?bytes=-1",
                  "/api/runs/r1/logs/x.log?bytes=1&bytes=2", "/api/runs/../x", "/api/runs/%2e%2e/x", "/api/runs/r1/logs/..%2fx",
                  "/api/runs/r1/logs/.hidden", "/api/runs/r1/logs/", "/api/runs/-r/x", "/api/runs/" + "a" * 65,
                  "/api/runs/lmloop/r1/extra", "/api/runs/other/r1/extra", "/api/models", "/api/projects", "/static/app.js",
                  "/logout?x=1", "/login?x=1", "/index.html", "//api/runs", "/api//runs", "/api/runs/", "/health?x=1", "/login/start?x=1",
                  "/api/runs/r1/events", "/api/runs/r1/preview", "/" + "a" * 3000, "/app.js?v=1", "/?x=1"):
            self.assertIn(self.call("GET", p, h)[0], (404, 400), p)
        self.assertEqual(self.call("GET", "/health?x=1")[0], 404)
        self.assertEqual(Backend.seen, [])

    def test_methods_other_than_get_and_post_are_refused(self):
        for m in ("PUT", "PATCH", "DELETE", "OPTIONS", "HEAD"):
            self.assertEqual(self.call(m, "/api/runs", self.post_headers())[0], 405, m)
        self.assertEqual(Backend.seen, [])
        self.assertEqual(self.call("POST", "/api/runs", self.post_headers(), b"{}")[0], 404)
        self.assertEqual(self.call("POST", "/health")[0], 404)
        self.assertEqual(Backend.seen, [])

    def test_upstream_request_is_rebuilt_not_relayed(self):
        Backend.routes[("GET", "/api/runs")] = (200, "application/json", b'{"runs":[]}', {})
        hostile = {**self.session_headers(), "X-Forwarded-User": "root", "Forwarded": "for=1.1.1.1", "Origin": "https://evil.example",
                   "Referer": "https://evil.example/", "X-Forwarded-For": "1.2.3.4", "Range": "bytes=0-1", "Accept-Encoding": "gzip",
                   "X-Real-Ip": "9.9.9.9", "Proxy-Authorization": "x", "Upgrade": "websocket", "Accept": "text/evil"}
        self.assertEqual(self.call("GET", "/api/runs", hostile)[0], 200)
        sent = {k.lower(): v for k, v in Backend.seen[-1]["headers"].items()}
        self.assertEqual(Backend.seen[-1]["path"], "/api/runs")
        self.assertEqual(sent["host"], f"127.0.0.1:{self.backend_port}")
        self.assertEqual(sent["authorization"], self.basic)
        self.assertEqual(sent["accept"], "application/json")
        self.assertLessEqual(set(sent), {"host", "authorization", "accept", "accept-encoding"})

    def test_upstream_redirect_oversize_wrong_type_and_error_status_are_generic_502(self):
        for route in ((302, "application/json", b"", {"Location": "http://127.0.0.1:9/"}),
                      (200, "text/html", b"<html>", {}),
                      (200, "application/json", b"x" * (gateway.MAX_UPSTREAM + 10), {}),
                      (500, "application/json", b'{"secret":"/home/x"}', {}),
                      (401, "application/json", b"{}", {"WWW-Authenticate": "Basic"})):
            Backend.routes[("GET", "/api/runs")] = route
            s, r, d = self.call("GET", "/api/runs", self.session_headers())
            self.assertEqual((s, json.loads(d)), (502, {"error": "upstream_error"}), route[:2])
            self.assertIsNone(r.getheader("Location"))
            self.assertIsNone(r.getheader("WWW-Authenticate"))
        self.assertEqual(len(Backend.seen), 5)   # redirect target never contacted; nothing followed
        # a dead backend is also generic
        dead = socket.socket(); dead.bind(("127.0.0.1", 0)); port = dead.getsockname()[1]; dead.close()
        object.__setattr__(self.cfg, "backend_port", port)
        try:
            self.assertEqual(self.call("GET", "/api/runs", self.session_headers())[0], 502)
        finally:
            object.__setattr__(self.cfg, "backend_port", self.backend_port)


class TestFraming(EdgeCase):
    def test_malformed_framing_and_duplicate_singletons(self):
        base = f"GET /api/runs HTTP/1.1\r\nHost: {HOST}\r\n"
        self.assertEqual(self.raw(base + f"Host: {HOST}\r\n\r\n"), 400)
        self.assertEqual(self.raw(base + "Cookie: a=1\r\nCookie: b=2\r\n\r\n"), 400)
        self.assertEqual(self.raw(base + "Authorization: Bearer a\r\nAuthorization: Bearer b\r\n\r\n"), 400)
        self.assertEqual(self.raw(base + "Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n"), 400)
        self.assertEqual(self.raw(base + "Content-Length: 4\r\n\r\nabcd"), 400)          # a GET carries no body
        post = f"POST /api/runs/r1/control HTTP/1.1\r\nHost: {HOST}\r\n"
        self.assertEqual(self.raw(post + "Content-Length: abc\r\n\r\n"), 400)
        self.assertEqual(self.raw(post + "Content-Length: -1\r\n\r\n"), 400)
        self.assertEqual(self.raw(post + "Content-Length: 1\r\nContent-Length: 1\r\n\r\n{"), 400)
        self.assertEqual(self.raw(post + "Content-Length: 99999\r\n\r\n"), 413)
        self.assertEqual(self.raw(post + "Content-Length: 10\r\n\r\n{}"), 400)            # short body
        self.assertEqual(self.raw(f"POST /api/runs/r1/control HTTP/1.1\r\nHost: {HOST}\r\nOrigin: {ORIGIN}\r\nOrigin: {ORIGIN}\r\n\r\n"), 400)
        self.assertEqual(Backend.seen, [])


class TestDeadlineAndSessions(EdgeCase):
    @classmethod
    def start_edge(cls):
        super().start_edge()
        cls.edge.shutdown()
        cls.edge.server_close()
        cls.cfg = Config(ORIGIN, cls.backend_port, cls.basic, frozenset({OPERATOR}), max_inflight=2, deadline=1.0)
        cls.edge = serve(Server(("127.0.0.1", 0), cls.cfg, cls.oidc, cls.edge.devices))
        cls.port = cls.edge.server_port

    def test_slow_drip_client_is_cut_off_at_the_total_deadline_and_frees_its_slot(self):
        started = time.monotonic()
        conns = [socket.create_connection(("127.0.0.1", self.port), timeout=10) for _ in range(2)]
        for s in conns:
            s.sendall(f"GET /health HTTP/1.1\r\nHost: {HOST}\r\n".encode())
        time.sleep(0.3)
        s = socket.create_connection(("127.0.0.1", self.port), timeout=10)   # both slots held: refused, not queued
        self.assertTrue(s.recv(200).startswith(b"HTTP/1.0 503"))
        s.close()
        for s in conns:   # drip one byte at a time; each recv alone would reset the 10 s per-read timer
            for _ in range(40):
                try:
                    s.sendall(b"X")
                    time.sleep(0.1)
                except OSError:
                    break
            s.close()
        self.assertLess(time.monotonic() - started, 4)
        time.sleep(0.2)
        self.assertEqual(self.call("GET", "/health")[0], 200)   # slots were released

    def test_session_lifetime_is_capped_even_for_a_validly_signed_cookie(self):
        Backend.routes[("GET", "/api/runs")] = (200, "application/json", b'{"runs":[]}', {})
        self.assertEqual(self.call("GET", "/api/runs", self.session_headers())[0], 200)
        long_lived = self.session_headers(exp=time.time() + 30 * 86400)   # make_oidc() allows 1 h
        self.assertEqual(self.call("GET", "/api/runs", long_lived)[0], 401)
        self.assertEqual(self.call("GET", "/api/runs", self.session_headers(exp="9e99"))[0], 401)


class TestWrites(EdgeCase):
    CTL = b'{"control":"pause"}'

    def setUp(self):
        super().setUp()
        Backend.routes[("POST", "/api/runs/r1/control")] = (200, "application/json", b'{"ok":true}', {})
        Backend.routes[("POST", "/api/runs/r1/decision")] = (200, "application/json", b'{"ok":true}', {})

    def test_every_guard_blocks_before_the_backend(self):
        P = "/api/runs/r1/control"
        ph = self.post_headers
        cases = {
            "anonymous": ({"Origin": ORIGIN, "Content-Type": "application/json", "X-CSRF-Token": "x"}, 401),
            "bearer only": ({"Authorization": f"Bearer {DEVICE}", "Origin": ORIGIN, "Content-Type": "application/json"}, 401),
            "bearer beside a valid session": (ph(Authorization=f"Bearer {DEVICE}"), 401),
            "any Authorization beside a valid session": (ph(Authorization=self.basic), 401),
            "foreign-sub session": (ph() | self.session_headers(sub=STRANGER), 401),
            "no origin": ({k: v for k, v in ph().items() if k != "Origin"}, 403),
            "foreign origin": (ph(Origin="https://evil.example"), 403),
            "origin trailing slash": (ph(Origin=ORIGIN + "/"), 403),
            "origin null": (ph(Origin="null"), 403),
            "origin http": (ph(Origin="http://" + HOST), 403),
            "origin backend": (ph(Origin=f"http://127.0.0.1:{self.backend_port}"), 403),
            "no csrf": ({k: v for k, v in ph().items() if k != "X-CSRF-Token"}, 403),
            "wrong csrf": (ph() | {"X-CSRF-Token": "csrf-token-value-124"}, 403),
            "csrf disabled literal": (ph() | {"X-CSRF-Token": "disabled"}, 403),
            "cross-site fetch metadata": (ph() | {"Sec-Fetch-Site": "cross-site"}, 403),
            "same-site fetch metadata": (ph() | {"Sec-Fetch-Site": "same-site"}, 403),
            "text/plain": (ph() | {"Content-Type": "text/plain"}, 415),
            "form": (ph() | {"Content-Type": "application/x-www-form-urlencoded"}, 415),
            "no content type": ({k: v for k, v in ph().items() if k != "Content-Type"}, 415),
        }
        for name, (headers, want) in cases.items():
            self.assertEqual(self.call("POST", P, headers, self.CTL)[0], want, name)
        self.assertEqual(Backend.seen, [])

    def test_body_grammar(self):
        P, h = "/api/runs/r1/control", self.post_headers()
        for body in (b"", b"{}", b"[]", b"null", b'"x"', b"{", b'{"control":"resume"}', b'{"control":"PAUSE"}', b'{"control":["pause"]}',
                     b'{"control":"pause","x":1}', b'{"control":"pause","control":"stop"}', b'{"control":"pause"} {}',
                     b'{"control":"pause"}\xff', b"\xff\xfe", b"{" * 100000, b'{"control":"' + b"a" * 9000 + b'"}'):
            self.assertIn(self.call("POST", P, h, body)[0], (400, 413), body[:30])
        D = "/api/runs/r1/decision"
        for body in (b"{}", b'{"decision":"merge"}', b'{"decision":"approved","x":1}', b'{"decision":"approved","note":5}',
                     b'{"decision":"approved","note":"' + b"a" * 2001 + b'"}', b'{"decision":"approved","decision":"changes_requested"}',
                     b'{"note":"x"}', b'{"decision":"approved","note":"\\ud800"}', b'{"decision":"approved","note":"a\\udfffb"}'):
            self.assertEqual(self.call("POST", D, h, body)[0], 400, body[:40])
        self.assertEqual(Backend.seen, [])

    def test_target_grammar_for_writes(self):
        h = self.post_headers()
        for p in ("/api/runs/r1/resume", "/api/runs/r1/control?x=1", "/api/runs/r1/decision?x=1", "/api/runs/../control",
                  "/api/runs/lmloop/r1/control", "/api/runs/r1/control/", "/api/runs/r%31/control", "/api/runs", "/api/projects",
                  "/api/runs/r1/preview", "/api/system-prompt", "/api/push/subscribe", "/api/runs/-r1/control"):
            self.assertEqual(self.call("POST", p, h, self.CTL)[0], 404, p)
        self.assertEqual(Backend.seen, [])

    def test_valid_writes_are_rebuilt_and_forwarded_once(self):
        h = self.post_headers() | {"Sec-Fetch-Site": "same-origin", "X-Forwarded-User": "root", "Referer": "https://evil.example/"}
        s, _, d = self.call("POST", "/api/runs/r1/control", h, b' { "control" : "stop_now" } ')
        self.assertEqual((s, json.loads(d)), (200, {"ok": True}))
        s, _, _ = self.call("POST", "/api/runs/r1/decision", h, b'{"decision":"changes_requested","note":"redo \xc3\xa9"}')
        self.assertEqual(s, 200)
        s, _, _ = self.call("POST", "/api/runs/r1/decision", h, b'{"decision":"approved"}')
        self.assertEqual(s, 200)
        self.assertEqual([(r["method"], r["path"]) for r in Backend.seen],
                         [("POST", "/api/runs/r1/control"), ("POST", "/api/runs/r1/decision"), ("POST", "/api/runs/r1/decision")])
        first = Backend.seen[0]
        self.assertEqual(json.loads(first["body"]), {"control": "stop_now"})
        sent = {k.lower(): v for k, v in first["headers"].items()}
        self.assertEqual(sent["origin"], f"http://127.0.0.1:{self.backend_port}")
        self.assertEqual(sent["host"], f"127.0.0.1:{self.backend_port}")
        self.assertEqual(sent["authorization"], self.basic)
        self.assertEqual(sent["content-type"], "application/json")
        self.assertTrue(set(sent) <= {"host", "authorization", "accept", "accept-encoding", "origin", "content-type", "content-length"}, sent)
        self.assertEqual(json.loads(Backend.seen[1]["body"]), {"decision": "changes_requested", "note": "redo é"})

    def test_write_audit_is_actor_correlated_and_leaks_nothing(self):
        import contextlib, hashlib, io
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            h = self.post_headers()
            self.call("POST", "/api/runs/r1/decision", h, b'{"decision":"changes_requested","note":"topsecretnote"}')
            self.call("POST", "/api/runs/r1/control", h, b'{"control":"stop"}')
            self.call("POST", "/api/runs/r1/control", h, b'{"control":"bogus"}')   # rejected: not audited as a forward
        actor = hashlib.sha256(OPERATOR.encode()).hexdigest()[:12]
        lines = [l for l in err.getvalue().splitlines() if "audit" in l]
        self.assertEqual(lines, [f"lmloop edge: audit actor={actor} run=r1 decision=changes_requested note_bytes=13 status=200",
                                 f"lmloop edge: audit actor={actor} run=r1 control=stop status=200"])
        for leak in ("topsecretnote", OPERATOR, h["Cookie"].split("=", 1)[1], "csrf-token-value-123"):
            self.assertNotIn(leak, err.getvalue())

    def test_upstream_write_refusals_stay_generic_or_allowlisted(self):
        Backend.routes[("POST", "/api/runs/r1/control")] = (409, "application/json", b'{"error":{"kind":"unavailable","message":"not held"}}', {})
        self.assertEqual(self.call("POST", "/api/runs/r1/control", self.post_headers(), self.CTL)[0], 409)
        Backend.routes[("POST", "/api/runs/r1/control")] = (500, "application/json", b'{"leak":"/home"}', {})
        s, _, d = self.call("POST", "/api/runs/r1/control", self.post_headers(), self.CTL)
        self.assertEqual((s, json.loads(d)), (502, {"error": "upstream_error"}))


class TestLogin(EdgeCase):
    def setUp(self):
        super().setUp()
        self.oidc._metadata = {"authorization_endpoint": "https://issuer.example.test/authorize"}

    def tearDown(self):
        self.oidc.__dict__.pop("callback", None)

    def test_login_start_redirects_with_a_preview_scoped_transaction_cookie(self):
        s, r, _ = self.call("GET", "/login/start")
        c = r.getheader("Set-Cookie")
        self.assertEqual(s, 302)
        self.assertTrue(r.getheader("Location").startswith("https://issuer.example.test/authorize?"))
        for part in (f"{gateway.OIDC_COOKIE}=", "Path=/oauth/callback", "HttpOnly", "Secure", "SameSite=Lax"):
            self.assertIn(part, c)
        self.assertNotIn("lmloop_oidc=", c)
        self.assertIn("code_challenge_method=S256", r.getheader("Location"))

    def test_logout_clears_edge_and_legacy_cookies_and_serves_a_sign_in_page(self):
        s, r, d = self.call("GET", "/logout", self.session_headers())
        self.assertEqual((s, r.getheader("Content-Type")), (200, "text/html; charset=utf-8"))
        self.assertIsNone(r.getheader("Location"))   # no redirect: neither /login/start nor the issuer, so SSO cannot undo it
        self.assertIn(b'href="/login/start"', d)
        self.assertNotIn(b"<script", d)
        cleared = {c.split("=", 1)[0]: c for c in r.headers.get_all("Set-Cookie")}
        self.assertEqual(set(cleared), {gateway.SESSION_COOKIE, "lmloop_session", gateway.OIDC_COOKIE})
        for c in cleared.values():
            for part in ("=;", "Max-Age=0", "HttpOnly", "Secure"):
                self.assertIn(part, c)
        self.assertIn("Path=/oauth/callback", cleared[gateway.OIDC_COOKIE])
        s, r, d = self.call("GET", "/login")   # the same page, anonymously, setting nothing
        self.assertEqual((s, r.getheader("Set-Cookie"), r.getheader("Location")), (200, None, None))
        self.assertIn(b'href="/login/start"', d)
        self.assertEqual(self.call("GET", "/logout")[0], 200)   # works with no session at all
        self.assertEqual(self.call("POST", "/logout", self.post_headers(), b"{}")[0], 404)
        self.assertEqual(Backend.seen, [])

    def stub(self, sub):
        self.oidc.callback = lambda code, state, tx: self.oidc.sign(
            {"sub": sub, "name": "n", "email": "", "csrf": "c" * 20, "exp": time.time() + 60})

    def test_callback_sets_session_only_for_an_allowlisted_subject(self):
        self.stub(OPERATOR)
        s, r, _ = self.call("GET", "/oauth/callback?code=c&state=s", {"Cookie": f"{gateway.OIDC_COOKIE}=tx"})
        cookies = r.headers.get_all("Set-Cookie")
        self.assertEqual((s, r.getheader("Location")), (302, "/"))
        sess = next(c for c in cookies if c.startswith(gateway.SESSION_COOKIE + "="))
        for part in ("Path=/", "HttpOnly", "Secure", "SameSite=Lax", "Max-Age=3600"):
            self.assertIn(part, sess)
        self.assertTrue(any(c.startswith(gateway.OIDC_COOKIE + "=;") and "Max-Age=0" in c for c in cookies))
        self.assertFalse(any(c.startswith("lmloop_session=") for c in cookies))
        self.stub(STRANGER)
        s, r, _ = self.call("GET", "/oauth/callback?code=c&state=s", {"Cookie": f"{gateway.OIDC_COOKIE}=tx"})
        self.assertEqual((s, r.getheader("Set-Cookie")), (403, None))

    def test_callback_rejects_malformed_queries_and_issuer_failures(self):
        self.stub(OPERATOR)
        for q in ("", "?code=c", "?state=s", "?code=c&state=s&code=d", "?code=c&state=s&x=1", "?code=c&state=s&error=access_denied",
                  "?code=&state=s", "?code", "?code=c&state=s&" + "&".join(f"iss{i}=1" for i in range(20))):
            s, r, _ = self.call("GET", "/oauth/callback" + q, {"Cookie": f"{gateway.OIDC_COOKIE}=tx"})
            self.assertEqual((s, r.getheader("Set-Cookie")), (400, None), q)

        def boom(*a):
            raise ValueError("invalid OIDC state https://issuer secret-detail")
        self.oidc.callback = boom
        s, _, d = self.call("GET", "/oauth/callback?code=c&state=s")
        self.assertEqual((s, json.loads(d)), (400, {"error": "login_failed"}))


class TestBuild(unittest.TestCase):
    def args(self, **kw):
        import argparse
        a = dict(public_origin=ORIGIN, issuer="https://issuer.example.test", client_id="cid", listen_port=0, backend_port=18083,
                 auth_file=None, session_hours=12, check=True)
        a.update(kw)
        return argparse.Namespace(**a)

    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir, True)
        self.auth = os.path.join(self.dir, "auth")
        Path(self.auth).write_text("u:p\n")
        os.chmod(self.auth, 0o600)
        self.env = {"LMLOOP_EDGE_OPERATOR_SUBS": f" {OPERATOR}, ,b", "LMLOOP_EDGE_OIDC_CLIENT_SECRET": "csecret",
                    "LMLOOP_EDGE_SESSION_SECRET": SECRET}

    def build(self, env=None, **kw):
        return gateway.build(self.args(auth_file=kw.pop("auth_file", self.auth), **kw), self.env if env is None else env)

    def test_good_configuration(self):
        cfg, oidc, devices = self.build()
        self.assertEqual((cfg.subs, cfg.authority, cfg.basic), (frozenset({OPERATOR, "b"}), HOST, "Basic " + base64.b64encode(b"u:p").decode()))
        self.assertTrue(oidc.enabled and not devices)
        self.assertEqual(gateway.main(["--public-origin", ORIGIN, "--issuer", "https://issuer.example.test", "--client-id", "c",
                                       "--listen-port", "0", "--backend-port", "18083", "--auth-file", self.auth, "--check"], self.env), 0)

    def die(self, **kw):
        import contextlib, io
        err, out = io.StringIO(), io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(out), self.assertRaises(SystemExit) as cm:
            self.build(**kw)
        return f"{cm.exception} {err.getvalue()} {out.getvalue()}"

    def test_device_tokens_are_strong_distinct_ascii_and_never_echoed(self):
        os.environ["EDGE_T1"], os.environ["EDGE_T2"] = DEVICE, DEVICE2
        ok = lambda raw: self.build(env={**self.env, "LMLOOP_EDGE_DEVICE_TOKENS": raw})[2]
        self.assertEqual(sorted(ok("a=env:EDGE_T1, b-2=env:EDGE_T2")._tokens), ["a", "b-2"])
        self.assertFalse(ok(""))
        weak = "short-token"
        for name, value in (("weak", weak), ("repeat", "a" * 40), ("non-ascii", "\u00e9" * 20 + DEVICE), ("space", "has space " + DEVICE),
                            ("long", "A1" * 200), ("secret reuse", SECRET)):
            os.environ["EDGE_BAD"] = value
            for raw in ("x=env:EDGE_BAD", "x=env:EDGE_T1,y=env:EDGE_BAD"):
                msg = self.die(env={**self.env, "LMLOOP_EDGE_DEVICE_TOKENS": raw})
                self.assertNotIn(value, msg, name)
                self.assertNotIn(DEVICE, msg)
        for raw, secret in (("x=env:EDGE_T1,y=env:EDGE_T1", DEVICE), ("x=env:EDGE_T1,x=env:EDGE_T2", DEVICE),
                            ("justalabelwithnoequals", "justalabelwithnoequals"), ("BAD LABEL=env:EDGE_T1", DEVICE),
                            ("x=env:EDGE_UNSET_VAR", SECRET)):
            msg = self.die(env={**self.env, "LMLOOP_EDGE_DEVICE_TOKENS": raw})
            self.assertNotIn(secret, msg.replace("EDGE_T1", ""), raw)
        # a device token equal to the session secret is refused, and so is a session secret equal to the client secret
        os.environ["EDGE_BAD"] = SECRET
        self.assertIn("differ", self.die(env={**self.env, "LMLOOP_EDGE_DEVICE_TOKENS": "x=env:EDGE_BAD"}))
        self.assertIn("differ", self.die(env={**self.env, "LMLOOP_EDGE_OIDC_CLIENT_SECRET": SECRET}))

    def test_weak_or_non_ascii_session_secret_is_refused_without_echo(self):
        for bad in ("a" * 40, "\u00e9" * 40, "short", "x" * 300, "has space in it 0123456789 abcdefghijklmnop"):
            msg = self.die(env={**self.env, "LMLOOP_EDGE_SESSION_SECRET": bad})
            self.assertNotIn(bad, msg)
        for hours in (0, -1, 25, 10**6):
            self.assertIn("session-hours", self.die(session_hours=hours))

    def test_legacy_dashboard_cookies_are_not_accepted_with_the_same_secret(self):
        from web import auth as web_auth
        _, oidc, _ = self.build()
        legacy = web_auth.OIDC("https://issuer.example.test", "cid", "csecret", ORIGIN, SECRET, 12)   # raw secret, as the old dashboard keyed it
        claims = {"sub": OPERATOR, "name": "n", "email": "", "csrf": "c" * 20, "exp": time.time() + 60}
        self.assertIsNone(oidc.session(legacy.sign(claims)))
        self.assertEqual(oidc.session(oidc.sign(claims))["sub"], OPERATOR)

    def test_every_missing_or_unsafe_setting_refuses_to_start(self):
        for origin in ("http://preview.example.test", "https://preview.example.test/", "https://preview.example.test/x", "https://Preview.example.test",
                       "https://preview.example.test:443", "https://user@preview.example.test", "https://preview.example.test:0",
                       "https://preview.example.test:99999", "preview.example.test", "", "https://", "https://a b"):
            with self.assertRaises(SystemExit, msg=origin):
                self.build(public_origin=origin)
        for bad in ({"LMLOOP_EDGE_OPERATOR_SUBS": ""}, {"LMLOOP_EDGE_OPERATOR_SUBS": " , "}, {"LMLOOP_EDGE_OIDC_CLIENT_SECRET": ""},
                    {"LMLOOP_EDGE_SESSION_SECRET": "short"}, {"LMLOOP_EDGE_SESSION_SECRET": "env:NOPE_UNSET_VAR"},
                    {"LMLOOP_EDGE_DEVICE_TOKENS": "bad"}, {"LMLOOP_EDGE_DEVICE_TOKENS": "x=short"}):
            with self.assertRaises(SystemExit, msg=bad):
                self.build(env={**self.env, **bad})
        for kw in ({"issuer": "http://issuer.example.test"}, {"issuer": ""}, {"client_id": ""}, {"backend_port": 0}, {"listen_port": 18083},
                   {"backend_port": 70000}, {"auth_file": os.path.join(self.dir, "missing")}):
            with self.assertRaises(SystemExit, msg=kw):
                self.build(**kw)
        os.chmod(self.auth, 0o644)
        with self.assertRaises(SystemExit):
            self.build()
        os.chmod(self.auth, 0o600)
        link = os.path.join(self.dir, "link")
        os.symlink(self.auth, link)
        with self.assertRaises(SystemExit):
            self.build(auth_file=link)
        Path(self.auth).write_text("nocolon\n")
        with self.assertRaises(SystemExit):
            self.build()


BIN = HERE.parents[0] / "target/debug/lmloop"
FX = HERE.parents[0] / "tests/fixtures"


@unittest.skipUnless(BIN.exists(), "rust/target/debug/lmloop not built")
class TestRealRust(EdgeCase):
    """The same edge in front of a real fresh-state Rust `lmloop web` (loopback, Basic)."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp())
        cls.state = cls.tmp / "state"
        repo = cls.tmp / "repo"
        repo.mkdir()
        env = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1", "PATH": os.environ["PATH"]}
        git = lambda *a: subprocess.run(["git", *a], cwd=repo, env=env, capture_output=True, check=True)  # noqa: E731
        git("init", "-q", "-b", "main"); git("config", "user.name", "t"); git("config", "user.email", "t@example.invalid")
        (repo / "README").write_text("base\n"); git("add", "-A"); git("commit", "-q", "-m", "base")
        pause = cls.state / "v1/runs/r1/PAUSE"
        manifest = {"manifest_version": 1, "repo": str(repo), "worker": {"argv": [str(FX / "fake-opencode")], "timeout_secs": 30},
                    "gate": {"argv": [str(FX / "gate-no-fail")], "timeout_secs": 30},
                    "tasks": [{"id": "t1", "prompt": f"ctl|src/one.txt|{pause}", "scopes": ["src/one.txt"]},
                              {"id": "t2", "prompt": "ok|docs/two.txt|second", "scopes": ["docs/two.txt"]}]}
        (cls.tmp / "m.json").write_text(json.dumps(manifest))
        done = subprocess.run([BIN, "--state-root", cls.state, "fixture-run", "r1", "--manifest", cls.tmp / "m.json"], capture_output=True)
        assert done.returncode == 12, done.stderr
        af = cls.tmp / "auth"
        af.write_text("be-user:be-pass\n")
        os.chmod(af, 0o600)
        cls.proc = subprocess.Popen([BIN, "--state-root", cls.state, "web", "--port", "0", "--auth-file", af],
                                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        addr = cls.proc.stdout.readline().strip().removeprefix("listening on http://").removesuffix("/")
        cls.backend_port = int(addr.rsplit(":", 1)[1])
        cls.backend_srv = None
        cls.start_edge()

    @classmethod
    def tearDownClass(cls):
        super().tearDownClass()
        cls.proc.terminate()
        cls.proc.wait(10)
        shutil.rmtree(cls.tmp, True)

    def test_reads_and_guarded_writes_reach_real_rust(self):
        h = self.session_headers()
        self.assertEqual(self.call("GET", "/", h)[0], 200)
        self.assertEqual(self.call("GET", "/app.css", h)[1].getheader("Content-Type"), "text/css; charset=utf-8")
        self.assertEqual(self.call("GET", "/app.js", h)[0], 200)
        s, _, d = self.call("GET", "/api/config", h)
        self.assertEqual((s, json.loads(d)["read_only"]), (200, False))
        bh = {"Authorization": f"Bearer {DEVICE}"}
        s, _, d = self.call("GET", "/api/config", bh)
        self.assertEqual((s, json.loads(d)["read_only"]), (200, True))
        runs = json.loads(self.call("GET", "/api/runs", bh)[2])["runs"]
        self.assertEqual((runs[0]["run_id"], runs[0]["state"]), ("r1", "paused"))
        self.assertEqual(json.loads(self.call("GET", "/api/runs/lmloop/r1", bh)[2])["route_id"], "r1")
        self.assertEqual(self.call("GET", "/api/runs/r1", h)[0], 200)
        self.assertEqual(self.call("GET", "/api/runs/nope", h)[0], 404)
        self.assertEqual(self.call("GET", "/api/runs/r1/logs/none.log", h)[0], 404)
        self.assertEqual(self.call("GET", "/api/runs/r1/logs/none.log?bytes=99", h)[0], 404)
        # Rust's own refusal passes through: a paused run cannot be approved, and nothing is recorded.
        before = sorted(p.name for p in (self.state / "v1/runs/r1").iterdir())
        s, _, d = self.call("POST", "/api/runs/r1/decision", self.post_headers(), b'{"decision":"approved","note":"fixture"}')
        self.assertEqual((s, json.loads(d)["error"]["kind"]), (409, "unavailable"), d)
        self.assertEqual(self.call("POST", "/api/runs/r1/decision", self.post_headers(), b'{"decision":"changes_requested"}')[0], 400)
        self.assertEqual(self.call("POST", "/api/runs/r1/resume", self.post_headers(), b"{}")[0], 404)
        self.assertEqual(self.call("POST", "/api/runs/r1/decision", self.post_headers(Authorization=f"Bearer {DEVICE}"),
                                   b'{"decision":"approved"}')[0], 401)
        self.assertEqual(sorted(p.name for p in (self.state / "v1/runs/r1").iterdir()), before)
        # a control write is accepted by Rust (control file only), through the edge
        s, _, d = self.call("POST", "/api/runs/r1/control", self.post_headers(), b'{"control":"stop"}')
        self.assertIn(s, (200, 409), d)


if __name__ == "__main__":
    unittest.main(verbosity=1)
