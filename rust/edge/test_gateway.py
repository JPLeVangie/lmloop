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
                  "/api/runs/lmloop/r1/control", "/api/runs/r1/control/", "/api/runs/r%31/control", "/api/runs/", "/api/runs?x=1", "/api/projects",
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


REV = "0123456789abcdef" * 2
LAUNCH = json.dumps({"project": "demo", "objective": "add the file", "revision": REV}).encode()
OPTIONS = {"enabled": True, "objective_max_bytes": 4096, "projects": [
    {"id": "demo", "label": "Demo", "eligible": True, "scopes": ["src/one.txt", "a b/é.txt"], "revision": REV,
     "budget": {"tasks": 1, "worker_timeout_secs": 30, "worker_startup_secs": None, "worker_idle_secs": None, "gate_timeout_secs": 30}}]}


def typed(d):
    """The one error shape the page parses: {error:{kind,message}} and nothing else."""
    e = json.loads(d)["error"]
    assert set(e) == {"kind", "message"} and all(isinstance(v, str) for v in e.values()), d
    return e["kind"]


class TestLaunch(EdgeCase):
    P, O = "/api/runs", "/api/launch-options"

    def setUp(self):
        super().setUp()
        Backend.routes[("POST", self.P)] = (202, "application/json", b'{"id":"web-1-2-3"}', {})
        Backend.routes[("GET", self.O)] = (200, "application/json", json.dumps(OPTIONS).encode(), {})

    def post(self, body=LAUNCH, **kw):
        return self.call("POST", self.P, kw.pop("headers", None) or self.post_headers(), body)

    def test_options_need_a_session_never_a_bearer_and_are_reprojected(self):
        bh = {"Authorization": f"Bearer {DEVICE}"}
        for h in ({}, bh, {**bh, **self.session_headers(sub=STRANGER)}):
            s, _, d = self.call("GET", self.O, h)
            self.assertEqual((s, typed(d)), (401, "authentication_required"), h)
        self.assertEqual(self.call("GET", self.O, {**self.session_headers(), "Origin": "https://evil.example"})[0], 403)
        self.assertEqual(self.call("GET", self.O + "?x=1", self.session_headers())[0], 404)
        self.assertEqual(Backend.seen, [])
        Backend.routes[("GET", self.O)] = (200, "application/json", json.dumps(
            {**OPTIONS, "leak": "/home/x", "projects": [{**OPTIONS["projects"][0], "manifest": "/secret/m.json"}]}).encode(), {})
        for h in (self.session_headers(), {**self.session_headers(), "Origin": ORIGIN, **bh}):   # a bearer beside a session is ignored
            s, r, d = self.call("GET", self.O, h)
            self.assertEqual((s, json.loads(d)), (200, OPTIONS))   # scopes pass through byte for byte; nothing extra
            self.assertEqual(r.getheader("Cache-Control"), "no-store")
        sent = {k.lower(): v for k, v in Backend.seen[-1]["headers"].items()}
        self.assertEqual((Backend.seen[-1]["path"], sent["authorization"], sent["accept"]), (self.O, self.basic, "application/json"))
        self.assertNotIn("cookie", sent)

    def test_options_disabled_and_backend_surprises(self):
        Backend.routes[("GET", self.O)] = (200, "application/json", b'{"enabled":false,"projects":[],"x":1}', {})
        self.assertEqual(json.loads(self.call("GET", self.O, self.session_headers())[2]), {"enabled": False, "projects": []})
        bad = ({**OPTIONS, "objective_max_bytes": "4096"}, {**OPTIONS, "projects": "x"}, [], {"enabled": True},
               {**OPTIONS, "projects": [{"id": "../x", "label": "x", "eligible": False}]},
               {**OPTIONS, "projects": [{**OPTIONS["projects"][0], "scopes": [{"a": 1}]}]},
               {**OPTIONS, "projects": [{**OPTIONS["projects"][0], "budget": {"tasks": 1}}]},
               {**OPTIONS, "projects": [{**OPTIONS["projects"][0], "revision": "x"}]},
               {**OPTIONS, "projects": [{"id": "a", "label": "a", "eligible": True}]})
        for body in bad:
            Backend.routes[("GET", self.O)] = (200, "application/json", json.dumps(body).encode(), {})
            s, _, d = self.call("GET", self.O, self.session_headers())
            self.assertEqual((s, typed(d)), (502, "upstream_error"), body)
        for raw in (b"[" * 5000, b"x", b"\xff"):
            Backend.routes[("GET", self.O)] = (200, "application/json", raw, {})
            self.assertEqual(self.call("GET", self.O, self.session_headers())[0], 502)
        # an ineligible project keeps only a fixed reason, even if the backend's text is hostile or mistyped
        for reason, want in (("the checkout has uncommitted changes", "the checkout has uncommitted changes"),
                             ("the one-shot launch was already used", "the one-shot launch was already used"),
                             ("the one-shot claim could not be checked", "the one-shot claim could not be checked"),
                             ("/home/pierce/secret", "The project is not ready to launch."), ({"a": 1}, None), (["x"], None)):
            p = {"id": "a", "label": "A", "eligible": False, "reason": reason}
            Backend.routes[("GET", self.O)] = (200, "application/json", json.dumps({**OPTIONS, "projects": [p]}).encode(), {})
            s, _, d = self.call("GET", self.O, self.session_headers())
            self.assertEqual(s, 200)
            self.assertEqual(json.loads(d)["projects"][0].get("reason"), want)

    def test_every_launch_guard_blocks_before_the_backend(self):
        ph = self.post_headers
        cases = {
            "anonymous": ({"Origin": ORIGIN, "Content-Type": "application/json", "X-CSRF-Token": "x"}, 401),
            "bearer only": ({"Authorization": f"Bearer {DEVICE}", "Origin": ORIGIN, "Content-Type": "application/json"}, 401),
            "bearer beside a session": (ph(Authorization=f"Bearer {DEVICE}"), 401),
            "foreign sub": (ph() | self.session_headers(sub=STRANGER), 401),
            "no origin": ({k: v for k, v in ph().items() if k != "Origin"}, 403),
            "foreign origin": (ph(Origin="https://evil.example"), 403),
            "backend origin": (ph(Origin=f"http://127.0.0.1:{self.backend_port}"), 403),
            "no csrf": ({k: v for k, v in ph().items() if k != "X-CSRF-Token"}, 403),
            "wrong csrf": (ph() | {"X-CSRF-Token": "nope"}, 403),
            "cross-site": (ph() | {"Sec-Fetch-Site": "cross-site"}, 403),
            "text/plain": (ph() | {"Content-Type": "text/plain"}, 415),
        }
        for name, (headers, want) in cases.items():
            s, _, d = self.post(headers=headers)
            self.assertEqual(s, want, name)
            typed(d)
        self.assertEqual(Backend.seen, [])

    def test_body_is_exact_and_bounded(self):
        j = lambda **kw: json.dumps({"project": "demo", "objective": "x", "revision": REV, **kw}).encode()  # noqa: E731
        for body in (b"", b"{}", b"[]", b"null", b"{", j(argv=["sh"]), j(run_id="mine"), j(scope_set="x"), j(project="../x"),
                     j(project="a b"), j(revision="abc"), j(revision=REV.upper()), j(objective=5), j(objective=None),
                     b'{"project":"demo","revision":"' + REV.encode() + b'"}', b'{"project":"demo","project":"x","objective":"o","revision":"' + REV.encode() + b'"}',
                     b'{"project":"demo","objective":"\\ud800","revision":"' + REV.encode() + b'"}', j() + b" {}", j() + b"\xff", b"[" * 5000):
            s, _, d = self.post(body=body)
            self.assertEqual((s, typed(d)), (400, "bad_request"), body[:60])
        self.assertEqual(Backend.seen, [])
        # 4096 UTF-8 bytes advertised, 8 KiB on the wire: an escaped-heavy objective is refused with a useful 413, cap unchanged
        s, _, d = self.post(body=j(objective="\n" * 4096))
        self.assertEqual((s, typed(d)), (413, "too_large"))
        self.assertIn("4096", json.loads(d)["error"]["message"])
        self.assertEqual(self.call("POST", "/api/runs/r1/control", self.post_headers(), b" " * 9000)[0], 413)   # others keep their reply
        self.assertEqual(Backend.seen, [])
        # a full 4096-byte multibyte objective fits and is forwarded unescaped, never larger than it arrived
        obj = "\u00e9" * 2048
        wire = json.dumps({"project": "demo", "objective": obj, "revision": REV}, ensure_ascii=False).encode()   # as a browser's JSON.stringify
        self.assertEqual(self.post(body=wire)[0], 202)
        sent = Backend.seen[-1]["body"]
        self.assertEqual(json.loads(sent), {"project": "demo", "objective": obj, "revision": REV})
        self.assertLessEqual(len(sent), len(wire))

    def test_success_is_a_truthful_202_with_a_rebuilt_request(self):
        h = self.post_headers() | {"Sec-Fetch-Site": "same-origin", "X-Forwarded-User": "root", "Referer": "https://evil.example/"}
        s, r, d = self.call("POST", self.P, h, b' { "revision" : "%s", "project":"demo", "objective":"add the file" } ' % REV.encode())
        self.assertEqual((s, json.loads(d)), (202, {"id": "web-1-2-3"}))
        self.assertEqual(len(Backend.seen), 1)
        seen = Backend.seen[0]
        self.assertEqual((seen["method"], seen["path"]), ("POST", self.P))
        self.assertEqual(json.loads(seen["body"]), {"project": "demo", "objective": "add the file", "revision": REV})
        sent = {k.lower(): v for k, v in seen["headers"].items()}
        self.assertEqual((sent["authorization"], sent["origin"], sent["host"]),
                         (self.basic, f"http://127.0.0.1:{self.backend_port}", f"127.0.0.1:{self.backend_port}"))
        self.assertTrue(set(sent) <= {"host", "authorization", "accept", "accept-encoding", "origin", "content-type", "content-length"}, sent)

    def test_backend_errors_are_typed_and_fixed_never_relayed(self):
        for status, kind in ((400, "usage"), (404, "no_project"), (404, "launch_disabled"), (409, "ineligible"),
                             (409, "stale_revision"), (409, "busy"), (504, "launch_pending")):
            Backend.routes[("POST", self.P)] = (status, "application/json", json.dumps(
                {"error": {"kind": kind, "message": "/home/pierce/secret objective=hunter2"}}).encode(), {})
            s, _, d = self.post()
            self.assertEqual((s, typed(d)), (status, kind))
            self.assertNotIn(b"secret", d)
            self.assertNotIn(b"hunter2", d)
        # an ineligible reason is relayed only when it is one of Rust's fixed strings
        Backend.routes[("POST", self.P)] = (409, "application/json", json.dumps(
            {"error": {"kind": "ineligible", "message": "the checkout has uncommitted changes"}}).encode(), {})
        self.assertEqual(json.loads(self.post()[2])["error"]["message"], "the checkout has uncommitted changes")
        # anything else is the uncertain 502 (the run may exist), and a 202 must carry exactly a run id
        for route in ((202, b'{"id":"../x"}'), (202, b'{"id":"a","x":1}'), (202, b"{}"), (202, b"x"), (202, b"[" * 5000),
                      (409, b'{"error":{"kind":"weird","message":"x"}}'), (409, b'{"error":{"kind":["a"],"message":{"b":1}}}'),
                      (409, b'{"error":{"kind":"ineligible","message":{"a":1}}}'), (409, b'{"error":"x"}'), (500, b"{}"),
                      (302, b""), (404, b"x"), (400, b"[]")):
            Backend.routes[("POST", self.P)] = (route[0], "application/json", route[1], {})
            s, _, d = self.post()
            if route[1].startswith(b'{"error":{"kind":"ineligible"'):
                self.assertEqual((s, typed(d)), (409, "ineligible"))
                self.assertEqual(json.loads(d)["error"]["message"], gateway.LAUNCH_MSG['ineligible'])
            else:
                self.assertEqual((s, typed(d)), (502, "upstream_error"), route)
        Backend.routes[("POST", self.P)] = (202, "text/html", b'{"id":"a"}', {})
        self.assertEqual(self.post()[0], 502)

    def test_launch_reply_relays_only_fixed_one_shot_reasons(self):
        def err(reason):
            return json.dumps({"error": {"kind": "ineligible", "message": reason}}).encode()
        for reason in ("the one-shot launch was already used", "the one-shot claim could not be checked"):
            self.assertEqual(gateway.launch_reply(409, err(reason)),
                             (409, {"error": {"kind": "ineligible", "message": reason}}))
        # hostile or near-miss text, e.g. a path or a claim file's contents, is never relayed
        for reason in ("the one-shot claim could not be checked: /tmp/x/v1/one-shot/demo", "/home/pierce/secret"):
            self.assertEqual(gateway.launch_reply(409, err(reason)),
                             (409, {"error": {"kind": "ineligible", "message": gateway.LAUNCH_MSG["ineligible"]}}))

    def test_launch_wait_is_longer_than_rusts_and_a_dead_backend_is_uncertain(self):
        self.assertGreater(gateway.LAUNCH_TIMEOUT, 10)   # Rust waits 10 s for the run to exist
        self.assertLessEqual(gateway.LAUNCH_TIMEOUT, gateway.REQUEST_DEADLINE)
        dead = socket.socket(); dead.bind(("127.0.0.1", 0)); port = dead.getsockname()[1]; dead.close()
        object.__setattr__(self.cfg, "backend_port", port)
        try:
            s, _, d = self.post()
            self.assertEqual((s, typed(d)), (502, "upstream_error"))
        finally:
            object.__setattr__(self.cfg, "backend_port", self.backend_port)

    def test_launch_audit_never_logs_the_objective(self):
        import contextlib, io
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.post(body=LAUNCH.replace(b"add the file", b"topsecretobjective"))
            Backend.routes[("POST", self.P)] = (409, "application/json", b'{"error":{"kind":"busy","message":"topsecretobjective"}}', {})
            self.post(body=LAUNCH.replace(b"add the file", b"topsecretobjective"))
        text = err.getvalue()
        self.assertEqual(len([l for l in text.splitlines() if "audit" in l]), 2)
        self.assertIn("launch project=demo status=202", text)
        for leak in ("topsecretobjective", REV, OPERATOR):
            self.assertNotIn(leak, text)

    def test_resume_and_other_routes_stay_closed(self):
        for p in ("/api/runs/web-1/resume", "/api/runs/web-1", "/api/launch-options"):
            self.assertEqual(self.call("POST", p, self.post_headers(), LAUNCH)[0], 404, p)
        self.assertEqual(self.call("GET", "/api/launch-options/x", self.session_headers())[0], 404)
        self.assertEqual(Backend.seen, [])


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

    def test_sign_in_page_is_styled_from_exact_anonymous_local_assets(self):
        s, r, d = self.call("GET", "/login")
        self.assertIn(b'href="/login.css"', d)
        self.assertIn(b'class="moon"', d)   # the crescent
        self.assertIn(b"Sign in to start", d)
        self.assertNotIn(b"<script", d)
        self.assertNotIn(b"http", d.replace(b"http://www.w3.org", b""))   # no third-party asset, no absolute URL
        self.assertIn("style-src 'self'", r.getheader("Content-Security-Policy"))
        self.assertIn(b"signed out", self.call("GET", "/logout")[2])
        static = Path(gateway.__file__).resolve().parents[2] / "web" / "static"
        for path, ctype, name in (("/login.css", "text/css; charset=utf-8", "login.css"), ("/favicon.png", "image/png", "favicon.png")):
            s, r, d = self.call("GET", path)   # no cookie: anonymous
            self.assertEqual((s, r.getheader("Content-Type"), d), (200, ctype, (static / name).read_bytes()))
            self.assertEqual((r.getheader("X-Content-Type-Options"), r.getheader("Set-Cookie")), ("nosniff", None))
            self.assertEqual(self.call("GET", path + "?v=1")[0], 404)
            self.assertEqual(self.call("POST", path, self.post_headers(), b"{}")[0], 404)
        for p in ("/style.css", "/app.js.map", "/static/login.css", "/login.html", "/manifest.json", "/sw.js", "/icon.svg",
                  "/apple-touch-icon.png", "/login.css/", "/LOGIN.CSS", "/%6cogin.css", "/../web/static/login.css"):
            self.assertIn(self.call("GET", p)[0], (404, 400, 401), p)
        self.assertEqual(self.call("GET", "/login.css", host="evil.example")[0], 421)
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


@unittest.skipUnless(BIN.exists(), "rust/target/debug/lmloop not built")
class TestRealRustLaunch(EdgeCase):
    """The edge in front of a real `lmloop web --launch-registry`: a fake worker in a scratch repo, never a model."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp())
        cls.state, cls.repo = cls.tmp / "state", cls.tmp / "repo"
        cls.repo.mkdir()
        cls.state.mkdir(mode=0o700)   # Rust refuses a state root open to group/other writes
        os.chmod(cls.state, 0o700)
        env = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1", "PATH": os.environ["PATH"]}
        cls.git = staticmethod(lambda *a: subprocess.run(["git", *a], cwd=cls.repo, env=env, capture_output=True, check=True).stdout.decode().strip())
        cls.git("init", "-q", "-b", "main"); cls.git("config", "user.name", "t"); cls.git("config", "user.email", "t@example.invalid")
        (cls.repo / "README").write_text("base\n"); cls.git("add", "-A"); cls.git("commit", "-q", "-m", "base")
        bin_ = cls.tmp / "bin"
        bin_.mkdir()
        shutil.copy(FX / "fake-opencode", bin_ / "opencode")   # the launcher name a pilot template may use
        cls.template, cls.registry = cls.tmp / "template.json", cls.tmp / "registry.json"
        cls.set_mode("ok|src/one.txt|from-template")
        cls.registry.write_text(json.dumps({"registry_version": 1, "projects": [{"id": "demo", "label": "Demo", "manifest": str(cls.template)}]}))
        af = cls.tmp / "auth"
        af.write_text("be-user:be-pass\n")
        for f in (af, cls.registry):
            os.chmod(f, 0o600)
        cls.proc = subprocess.Popen([BIN, "--state-root", cls.state, "web", "--port", "0", "--auth-file", af,
                                     "--launch-registry", cls.registry], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        addr = cls.proc.stdout.readline().strip().removeprefix("listening on http://").removesuffix("/")
        cls.backend_port = int(addr.rsplit(":", 1)[1])
        cls.backend_srv = None
        cls.start_edge()

    @classmethod
    def set_mode(cls, mode):
        cls.template.write_text(json.dumps({"manifest_version": 1, "repo": str(cls.repo),
                                            "worker": {"argv": [str(cls.tmp / "bin/opencode")], "timeout_secs": 30},
                                            "gate": {"argv": [str(FX / "gate-no-fail")], "timeout_secs": 30},
                                            "tasks": [{"id": "t1", "prompt": mode, "scopes": ["src/one.txt"]}]}))
        os.chmod(cls.template, 0o600)

    @classmethod
    def tearDownClass(cls):
        super().tearDownClass()
        cls.proc.terminate()
        cls.proc.wait(10)
        shutil.rmtree(cls.tmp, True)

    def launch(self, rev, objective="add the file", project="demo"):
        body = json.dumps({"project": project, "objective": objective, "revision": rev}).encode()
        s, _, d = self.call("POST", "/api/runs", self.post_headers(), body)
        return s, json.loads(d)

    def option(self):
        s, _, d = self.call("GET", "/api/launch-options", self.session_headers())
        self.assertEqual(s, 200, d)
        return json.loads(d)

    def wait(self, run, phase):
        end = time.monotonic() + 30
        while time.monotonic() < end:
            v = json.loads(self.call("GET", f"/api/runs/{run}", self.session_headers())[2])
            if v["phase"] == phase:
                return v
            time.sleep(0.1)
        self.fail(f"{run} never reached {phase}")

    def test_launch_through_the_edge_stale_stop_and_refusals(self):
        opts = self.option()
        self.assertEqual((opts["enabled"], opts["objective_max_bytes"]), (True, 4096))
        demo = opts["projects"][0]
        self.assertEqual((demo["id"], demo["eligible"], demo["scopes"]), ("demo", True, ["src/one.txt"]))
        self.assertNotIn(str(self.tmp), json.dumps(opts))
        self.assertEqual(self.call("GET", "/api/launch-options", {"Authorization": f"Bearer {DEVICE}"})[0], 401)
        head, rev = self.git("rev-parse", "HEAD"), demo["revision"]
        # refusals create nothing
        for (status, kind), args in (((409, "stale_revision"), ("0" * 32,)), ((404, "no_project"), (rev, "x", "nope")),
                                     ((400, "usage"), (rev, "  ")), ((400, "usage"), (rev, "bad\u001b[31m"))):
            s, d = self.launch(*args)
            self.assertEqual((s, d["error"]["kind"]), (status, kind), d)
            self.assertEqual(set(d["error"]), {"kind", "message"})
        self.assertFalse((self.state / "v1/runs").exists())
        s, d = self.launch(rev, "x" * 4097)   # over Rust's objective cap, under the wire cap: Rust's own refusal, typed
        self.assertEqual((s, d["error"]["kind"]), (400, "usage"))
        # success: 202 {id}, then the real run commits through the gate
        s, d = self.launch(rev)
        self.assertEqual((s, set(d)), (202, {"id"}), d)
        run = d["id"]
        self.assertEqual(self.wait(run, "complete")["kind"], "serial_pilot")
        self.assertNotEqual(self.git("rev-parse", "HEAD"), head)
        self.assertEqual((self.repo / "src/one.txt").read_text(), "from-template\n")
        # the objective reached the template prompt but is in no edge-visible output; a bearer cannot launch
        s, _, d = self.call("POST", "/api/runs", self.post_headers(Authorization=f"Bearer {DEVICE}"), LAUNCH)
        self.assertEqual(s, 401)
        # dirty checkout: refused with Rust's fixed reason, creates no run, and the operator's edit is left untouched
        head, runs = self.git("rev-parse", "HEAD"), sorted(p.name for p in (self.state / "v1/runs").iterdir())
        (self.repo / "src/one.txt").write_text("operator edit\n")
        s, d = self.launch(self.option()["projects"][0]["revision"])
        self.assertEqual((s, d["error"]["kind"], d["error"]["message"]), (409, "ineligible", "the checkout has uncommitted changes"), d)
        self.assertEqual(sorted(p.name for p in (self.state / "v1/runs").iterdir()), runs)
        self.assertEqual((self.repo / "src/one.txt").read_text(), "operator edit\n")
        self.assertEqual((self.git("rev-parse", "HEAD"), self.git("status", "--porcelain")), (head, "M src/one.txt"))
        (self.repo / "src/one.txt").write_text("from-template\n")   # restore only the fixture file to continue
        self.assertEqual(self.git("status", "--porcelain"), "")
        # stale: the operator changes the template after review, so the old revision is refused
        self.set_mode("slow|src/one.txt|x")
        s, d = self.launch(rev)
        self.assertEqual((s, d["error"]["kind"]), (409, "stale_revision"))
        rev = self.option()["projects"][0]["revision"]
        self.assertNotEqual(rev, demo["revision"])
        # stop: a slow worker, one launch at a time (busy), STOP-NOW through the edge ends it
        head = self.git("rev-parse", "HEAD")
        s, d = self.launch(rev)
        self.assertEqual(s, 202, d)
        run = d["id"]
        end = time.monotonic() + 20
        while json.loads(self.call("GET", f"/api/runs/{run}", self.session_headers())[2])["actions"]["stop_now"]["available"] is not True:
            self.assertLess(time.monotonic(), end, "worker never started")
            time.sleep(0.1)
        s, d = self.launch(rev)
        self.assertEqual((s, d["error"]["kind"]), (409, "busy"))
        s, _, d = self.call("POST", f"/api/runs/{run}/control", self.post_headers(), b'{"control":"stop_now"}')
        self.assertEqual(s, 200, d)
        self.wait(run, "stopped")
        self.assertEqual(self.git("rev-parse", "HEAD"), head)   # a stop commits nothing


if __name__ == "__main__":
    unittest.main(verbosity=1)
