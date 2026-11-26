#!/usr/bin/env python3
"""Loopback auth edge for the Rust `lmloop web` preview (lm-1x4).

Browser/Android -> TLS proxy (public HTTPS origin) -> THIS edge (127.0.0.1,
plain HTTP) -> Rust `lmloop web` (127.0.0.1, Basic auth, fresh native state).

The edge owns identity and nothing else: it reuses the tracked `web/auth.py`
OIDC class and `web/device_auth.py` bearer verifier unchanged, checks that the
OIDC `sub` is on an operator allowlist at the callback and on EVERY request,
and forwards only a fixed allowlist of routes, each rebuilt from scratch.

Never forwarded: any incoming header, any incoming path.  Each upstream request
is reconstructed (Host/Origin numeric 127.0.0.1, the Rust Basic credential from
its 0600 auth file, a fixed Accept; for writes a re-serialised validated body).
The upstream is never followed through a redirect and its status, content type
and size are checked before anything reaches the client.

Routes (everything else is 404):
  open          GET /health (fixed JSON, no upstream), /login/start, /oauth/callback
  session only  GET /  /app.css  /app.js  /api/runs/<id>
                GET /api/runs/<id>/logs/<name>[?bytes=N]
                POST /api/runs/<id>/control   {"control": pause|stop|stop_now}
                POST /api/runs/<id>/decision  {"decision": approved|changes_requested, "note"?}
                GET /api/launch-options   (New run review data, re-typed from the allowlisted fields)
                POST /api/runs  {"project","objective","revision"}  (exactly these; bearer never; typed errors)
  bearer|session GET /api/config  /api/runs  /api/runs/lmloop/<id>   (Android)
  open          GET /login (sign-in page, no redirect), /logout (clears this edge's cookies, then the same page)
                GET /login.css, /favicon.png (exact files from tracked web/static, no query; nothing else)
No resume, no query string other than the log `bytes`.  /logout never redirects
to /login/start or the issuer: an automatic SSO round trip would undo it.
Decisions are recorded-only by the Rust side; this edge adds no approval power.

Run (see --help); secrets come from the environment, never argv:
  LMLOOP_EDGE_OIDC_CLIENT_SECRET   client secret      (config.secret syntax: env:/file:/literal)
  LMLOOP_EDGE_SESSION_SECRET       cookie HMAC key, >= 32 ASCII chars, never reused from another service
  LMLOOP_EDGE_OPERATOR_SUBS        comma-separated allowlisted OIDC `sub` values (required)
  LMLOOP_EDGE_DEVICE_TOKENS        optional `label=reference,...` Android bearer tokens; each resolves
                                   to 32-256 printable ASCII chars, distinct from every other secret
`--check` validates the whole configuration and exits without binding or
touching the network.  Configuration errors name the setting, never its value.

Cookies: this edge's own names (SESSION_COOKIE/OIDC_COOKIE) and an HMAC key
derived from the session secret under a private label, so a cookie from a
service already on this hostname (`lmloop_session`, same or reused secret) is
neither read nor verifiable.  Sessions older than --session-hours (max 24) are refused.

A request has one total deadline (REQUEST_DEADLINE) covering client reads,
upstream and client writes; at most MAX_INFLIGHT run at once.
"""
import sys

sys.dont_write_bytecode = True  # importing tracked modules must not write __pycache__ into the repo

import argparse
import base64
import hashlib
import hmac
import http.client
import json
import os
import re
import socket
import stat
import threading
import time
from dataclasses import dataclass
from http.cookies import SimpleCookie
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qsl

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
import config as config_module  # noqa: E402  (tracked; resolves env:/file: secret references)
from web import auth as web_auth, device_auth  # noqa: E402  (tracked verifiers, reused unchanged)

SESSION_COOKIE = "lmloop_preview_session"
OIDC_COOKIE = "lmloop_preview_oidc"
MAX_BODY = 8 * 1024
MAX_LOG_BYTES = 64 * 1024
MAX_UPSTREAM = 1024 * 1024
IO_TIMEOUT = 10
UPSTREAM_DEADLINE = 15
REQUEST_DEADLINE = 30   # total wall clock per request, whatever the client or upstream drips
MAX_INFLIGHT = 16
MAX_SESSION_HOURS = 24
LEGACY_COOKIE = "lmloop_session"   # the Python dashboard's name; never read here, only expired at logout
BEARER = re.compile(r"Bearer ([\x21-\x7e]{1,512})")
TOKEN = re.compile(r"[\x21-\x7e]{32,256}")
LABEL = re.compile(r"[a-z0-9][a-z0-9_-]{0,31}")
# The sign-in page mirrors tracked web/static/login.html (crescent drawn in CSS) minus its inline script,
# manifest and PWA tags, which the CSP and this edge's closed route set would refuse anyway.
_SIGN_IN = ('<!doctype html><html lang="en"><head><meta charset="utf-8">'
            '<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">'
            '<meta name="theme-color" content="#0c0c0f"><link rel="icon" href="/favicon.png" sizes="32x32">'
            '<title>lmloop \u2014 sign in</title><link rel="stylesheet" href="/login.css"></head><body>'
            '<main class="card"><div class="sky" aria-hidden="true"><div class="moon"><span></span></div>'
            '<i class="star s1"></i><i class="star s2"></i><i class="star s3"></i><i class="star s4"></i></div>'
            '<p class="eyebrow">slow models \u00b7 long nights</p><h1>lmloop</h1><p class="sub">%s</p>'
            '<a class="btn" href="/login/start"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm-1 17.93c-3.95-.49-7-3.85-7-7.93 0-.62.08-1.21.21-1.79L9 15v1c0 1.1.9 2 2 2v1.93zm6.9-2.54c-.26-.81-1-1.39-1.9-1.39h-1v-3c0-.55-.45-1-1-1H8v-2h2c.55 0 1-.45 1-1V7h2c1.1 0 2-.9 2-2v-.41c2.93 1.19 5 4.06 5 7.41 0 2.08-.8 3.97-2.1 5.39z"/></svg>'
            'Sign in</a></main></body></html>')
SIGN_IN = (_SIGN_IN % "Sign in to start, watch and steer your runs.").encode()
SIGNED_OUT = (_SIGN_IN % "You are signed out.").encode()
_STATIC = Path(__file__).resolve().parents[2] / "web" / "static"
# Exact anonymous public files (no query, nothing else under web/static): path -> (content type, bytes read once at start)
PUBLIC = {p: (t, (_STATIC / p[1:]).read_bytes())
          for p, t in (("/login.css", "text/css; charset=utf-8"), ("/favicon.png", "image/png"))}
SINGLETONS = ("Host", "Origin", "Cookie", "Authorization", "Content-Type", "Content-Length",
              "X-CSRF-Token", "Sec-Fetch-Site")
CSP = ("default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; "
       "img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'")
SECURITY = (("Cache-Control", "no-store"), ("X-Content-Type-Options", "nosniff"), ("X-Frame-Options", "DENY"),
            ("Referrer-Policy", "no-referrer"), ("Content-Security-Policy", CSP))
JSON, HTML = "application/json", "text/html; charset=utf-8"
ASSETS = {"/": HTML, "/app.css": "text/css; charset=utf-8", "/app.js": "text/javascript; charset=utf-8"}
RID, LOG = r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}", r"[A-Za-z0-9_-][A-Za-z0-9._-]{0,127}"
CONTROLS = {"pause", "stop", "stop_now"}
DECISIONS = {"approved", "changes_requested"}
CALLBACK_KEYS = {"code", "state", "iss", "session_state", "error", "error_description"}
RUN_ID = re.compile(RID)
REVISION = re.compile(r"[0-9a-f]{32}")
LAUNCH_KEYS = {"project", "objective", "revision"}
BUDGET_KEYS = {"tasks", "worker_timeout_secs", "worker_startup_secs", "worker_idle_secs", "gate_timeout_secs"}
LAUNCH_TIMEOUT = 14     # Rust waits up to 10 s for the run to exist; the default IO_TIMEOUT would race that answer
LAUNCH_STATUSES = (202, 400, 404, 409, 504)
# Reasons the Rust side may give for an unusable or ineligible project (fixed text, no paths).
REASONS = frozenset({"the project template is not usable", "the checkout has uncommitted changes",
                     "the checkout is not on a branch", "a Git operation is in progress in the checkout",
                     "the checkout hides changes (assume-unchanged or skip-worktree)", "the checkout is not ready",
                     "the isolated worktree directory is not ready",
                     "the worker progress file is not fresh or usable", "the one-shot launch was already used",
                     "the one-shot claim could not be checked"})
# Every launch error body is {"error":{"kind","message"}} with THESE messages: backend text is never relayed
# (except an `ineligible` reason found in REASONS).  Kinds below the first group are the only backend refusals relayed.
LAUNCH_MSG = {
    "usage": "The objective was not accepted: it must be non-empty text of at most 4096 bytes without control characters other than newline and tab.",
    "no_project": "No such project.",
    "launch_disabled": "This server has no launch registry.",
    "ineligible": "The project is not ready to launch.",
    "stale_revision": "The project's policy changed since it was reviewed; reload and review again.",
    "busy": "A launch or resume is already running; try again when it ends.",
    "launch_pending": "The launch has not created its run yet; check the run list before retrying.",
    "authentication_required": "Sign in again.",
    "bad_origin": "The request origin was refused.",
    "bad_csrf": "The session token was refused; reload the page.",
    "bad_content_type": "The request must be application/json.",
    "bad_request": "The request must be a JSON object with exactly project, objective and revision.",
    "too_large": "The request is over the 8 KiB limit. An objective of up to 4096 bytes can still exceed it once JSON-escaped; shorten it.",
    "not_found": "Not found.",
    "upstream_error": "The server gave no usable answer. The run may or may not have started; check the run list before retrying.",
}
LAUNCH_REFUSALS = {(400, "usage"), (404, "no_project"), (404, "launch_disabled"), (409, "ineligible"),
                   (409, "stale_revision"), (409, "busy"), (504, "launch_pending")}

# (method, full-match path regex, kind).  The whole target grammar lives here.
ROUTES = [
    ("GET", r"/health", "health"),
    ("GET", r"/login/start", "login"),
    ("GET", r"/login", "signin"),
    ("GET", r"/logout", "logout"),
    ("GET", "|".join(re.escape(p) for p in PUBLIC), "public"),
    ("GET", r"/oauth/callback", "callback"),
    ("GET", r"/|/app\.css|/app\.js", "asset"),
    ("GET", r"/api/config|/api/runs|/api/runs/lmloop/" + RID, "android"),
    ("GET", r"/api/launch-options", "launch_options"),
    ("POST", r"/api/runs", "launch"),
    ("GET", r"/api/runs/" + RID, "session"),
    ("GET", rf"/api/runs/{RID}/logs/{LOG}", "log"),
    ("POST", rf"/api/runs/{RID}/control", "control"),
    ("POST", rf"/api/runs/{RID}/decision", "decision"),
]
ROUTES = [(m, re.compile(p), k) for m, p, k in ROUTES]


@dataclass(frozen=True)
class Config:
    public_origin: str      # https://host[:port], exactly what browsers send as Origin
    backend_port: int
    basic: str              # complete Authorization value for the Rust backend
    subs: frozenset         # allowlisted OIDC subjects
    max_inflight: int = MAX_INFLIGHT
    deadline: float = REQUEST_DEADLINE

    @property
    def authority(self):    # the Host the proxy must present
        return self.public_origin[len("https://"):]

    @property
    def backend_authority(self):
        return f"127.0.0.1:{self.backend_port}"


def _die(message):
    raise SystemExit("lmloop edge: " + message)


def valid_public_origin(origin):
    m = re.fullmatch(r"https://[a-z0-9]([a-z0-9.-]*[a-z0-9])?(?::([1-9][0-9]{0,4}))?", origin or "")
    return bool(m) and (m.group(2) is None or 0 < int(m.group(2)) <= 65535 and m.group(2) != "443")


def read_basic(path):
    """`Authorization` value from the Rust backend's auth file, under the same rules Rust applies to it."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError:
        _die("cannot open the auth file")
    with os.fdopen(fd, "rb") as f:
        st = os.fstat(f.fileno())
        if not stat.S_ISREG(st.st_mode) or st.st_uid != os.geteuid() or stat.S_IMODE(st.st_mode) != 0o600 or st.st_size > 1024:
            _die("auth file must be a regular file owned by this user, mode 0600, at most 1024 bytes")
        text = f.read(1025).decode("utf-8", "replace")
    text = text[:-1] if text.endswith("\n") else text
    user, sep, password = text.partition(":")
    if not sep or not user or not password or any(c in text for c in "\r\n"):
        _die("auth file must hold one non-empty username:password line")
    return "Basic " + base64.b64encode(text.encode()).decode()


def strong(value):
    return bool(TOKEN.fullmatch(value)) and len(set(value)) >= 12


def derive_key(session_secret):
    """The cookie HMAC key: a cookie signed with the raw secret (e.g. by the old dashboard on this host) never verifies."""
    return hmac.new(session_secret.encode(), b"lmloop-edge/preview-session/v1", hashlib.sha256).hexdigest()


def parse_devices(raw, reserved):
    """DeviceTokens from `label=reference,...`.  Fail closed; messages carry a position or a validated label, never a value."""
    tokens = {}
    for n, entry in enumerate([e.strip() for e in raw.split(",") if e.strip()], 1):
        label, sep, ref = entry.partition("=")
        label = label.strip()
        if not sep or not LABEL.fullmatch(label):
            _die(f"LMLOOP_EDGE_DEVICE_TOKENS entry {n} is malformed; expected label=reference with a [a-z0-9_-] label")
        if label in tokens:
            _die(f"LMLOOP_EDGE_DEVICE_TOKENS repeats label {label}")
        value = config_module.secret(ref.strip())
        if not strong(value):
            _die(f"device token {label} must resolve to 32-256 printable ASCII characters, at least 12 distinct")
        if value in reserved or value in tokens.values():
            _die(f"device token {label} must differ from every other secret and device token")
        tokens[label] = value
    return device_auth.DeviceTokens(tokens)


def build(args, env):
    """(Config, OIDC, DeviceTokens) or SystemExit.  No network, no bind."""
    if not valid_public_origin(args.public_origin):
        _die("--public-origin must be an exact lowercase https://host[:port] origin (no path, no :443)")
    if not re.fullmatch(r"https://[A-Za-z0-9.-]+(:[0-9]{1,5})?(/[A-Za-z0-9._~/-]*)?", args.issuer or ""):
        _die("--issuer must be an https:// URL")
    if not args.client_id:
        _die("--client-id is required")
    if not 0 <= args.listen_port <= 65535 or not 1 <= args.backend_port <= 65535 or args.listen_port == args.backend_port:
        _die("--listen-port/--backend-port must be distinct loopback ports")
    if not 1 <= args.session_hours <= MAX_SESSION_HOURS:
        _die(f"--session-hours must be 1..{MAX_SESSION_HOURS}")
    subs = frozenset(s.strip() for s in env.get("LMLOOP_EDGE_OPERATOR_SUBS", "").split(",") if s.strip())
    if not subs:
        _die("LMLOOP_EDGE_OPERATOR_SUBS must name at least one operator subject")
    client_secret = config_module.secret(env.get("LMLOOP_EDGE_OIDC_CLIENT_SECRET", ""))
    session_secret = config_module.secret(env.get("LMLOOP_EDGE_SESSION_SECRET", ""))
    if not client_secret:
        _die("LMLOOP_EDGE_OIDC_CLIENT_SECRET is missing or unresolvable")
    if not strong(session_secret):
        _die("LMLOOP_EDGE_SESSION_SECRET must resolve to 32-256 printable ASCII characters, at least 12 distinct")
    if session_secret == client_secret:
        _die("LMLOOP_EDGE_SESSION_SECRET must differ from the OIDC client secret")
    cfg = Config(args.public_origin, args.backend_port, read_basic(args.auth_file), subs)
    key = derive_key(session_secret)
    oidc = web_auth.build({"mode": "oidc", "oidc_issuer": args.issuer, "oidc_client_id": args.client_id,
                           "oidc_client_secret": client_secret, "public_url": args.public_origin,
                           "session_secret": key, "session_hours": args.session_hours})
    if not isinstance(oidc, web_auth.OIDC) or not oidc.enabled:
        _die("OIDC is not usable")
    plain = base64.b64decode(cfg.basic[len("Basic "):]).decode()
    reserved = {client_secret, session_secret, key, plain, plain.partition(":")[2]}
    return cfg, oidc, parse_devices(env.get("LMLOOP_EDGE_DEVICE_TOKENS", ""), reserved)


class UpstreamError(Exception):
    pass


def _no_dup_keys(pairs):
    out = {}
    for k, v in pairs:
        if k in out:
            raise ValueError("duplicate key")
        out[k] = v
    return out


def parse_write(kind, raw):
    """The re-serialised, validated body for a write route, or None."""
    try:
        data = json.loads(raw.decode("utf-8"), object_pairs_hook=_no_dup_keys)
    except (ValueError, UnicodeDecodeError, RecursionError):
        return None
    if not isinstance(data, dict):
        return None
    if kind == "control":
        ok = set(data) == {"control"} and isinstance(data["control"], str) and data["control"] in CONTROLS
    else:
        note = data.get("note", "")
        try:
            size = len(note.encode()) if isinstance(note, str) else 1 << 20   # a lone surrogate ("\ud800") cannot be UTF-8
        except UnicodeEncodeError:
            return None
        ok = ("decision" in data and set(data) <= {"decision", "note"} and isinstance(data["decision"], str)
              and data["decision"] in DECISIONS and size <= 2000)
    return json.dumps(data, separators=(",", ":")).encode() if ok else None


def parse_launch(raw):
    """The re-serialised launch body: exactly project, objective, revision (nothing defaulted or added), or None.

    ensure_ascii=False keeps it no larger than what was received, so Rust's own 8 KiB cap is never newly hit."""
    try:
        data = json.loads(raw.decode("utf-8"), object_pairs_hook=_no_dup_keys)
        if not isinstance(data, dict) or set(data) != LAUNCH_KEYS or not all(isinstance(v, str) for v in data.values()):
            return None
        if not RUN_ID.fullmatch(data["project"]) or not REVISION.fullmatch(data["revision"]):
            return None
        return json.dumps(data, separators=(",", ":"), ensure_ascii=False).encode("utf-8")   # a lone surrogate raises
    except (ValueError, UnicodeError, RecursionError):
        return None


def launch_reply(status, data):
    """(status, body) for the browser from the backend's launch answer: 202 {id} or a fixed-text typed error.

    Anything unrecognised is the uncertain 502 upstream_error: the run may exist.  Never raises."""
    uncertain = (502, {"error": {"kind": "upstream_error", "message": LAUNCH_MSG["upstream_error"]}})
    try:
        d = json.loads(data)
    except (ValueError, RecursionError):
        return uncertain
    if status == 202:
        ok = isinstance(d, dict) and set(d) == {"id"} and isinstance(d["id"], str) and RUN_ID.fullmatch(d["id"])
        return (202, {"id": d["id"]}) if ok else uncertain
    e = d.get("error") if isinstance(d, dict) else None
    kind = e.get("kind") if isinstance(e, dict) else None
    if not isinstance(kind, str) or (status, kind) not in LAUNCH_REFUSALS:
        return uncertain
    reason = e.get("message")
    msg = reason if kind == "ineligible" and isinstance(reason, str) and reason in REASONS else LAUNCH_MSG[kind]
    return status, {"error": {"kind": kind, "message": msg}}


def clean_options(raw):
    """Projection of Rust's /api/launch-options onto its known fields and types, or None for any other shape.

    Policy (label, scopes, budget values) is Rust's to validate; scopes pass through untouched.  Never raises."""
    try:
        d = json.loads(raw)
    except (ValueError, RecursionError):
        return None
    projects = d.get("projects") if isinstance(d, dict) else None
    if not isinstance(projects, list):
        return None
    if d.get("enabled") is False:
        return {"enabled": False, "projects": []}
    cap = d.get("objective_max_bytes")
    if d.get("enabled") is not True or type(cap) is not int:
        return None
    out = []
    for p in projects:
        if not (isinstance(p, dict) and isinstance(p.get("id"), str) and RUN_ID.fullmatch(p["id"])
                and isinstance(p.get("label"), str) and isinstance(p.get("eligible"), bool)):
            return None
        q = {"id": p["id"], "label": p["label"], "eligible": p["eligible"]}
        reason = p.get("reason")
        if isinstance(reason, str):
            q["reason"] = reason if reason in REASONS else LAUNCH_MSG["ineligible"]
        if "worktree" in p:
            if type(p["worktree"]) is not bool:
                return None
            q["worktree"] = p["worktree"]
        if "revision" in p:
            sc, bd, rev = p.get("scopes"), p.get("budget"), p["revision"]
            if not (isinstance(sc, list) and all(isinstance(x, str) for x in sc) and isinstance(bd, dict)
                    and set(bd) == BUDGET_KEYS and all(v is None or type(v) is int for v in bd.values())
                    and isinstance(rev, str) and REVISION.fullmatch(rev)):
                return None
            q.update(scopes=sc, budget=bd, revision=rev)
        elif q["eligible"]:
            return None
        out.append(q)
    return {"enabled": True, "objective_max_bytes": cap, "projects": out}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"   # one request per connection: no pipelining or smuggling surface
    timeout = IO_TIMEOUT
    server_version, sys_version = "lmloop-edge", ""

    def setup(self):
        super().setup()
        self.t0 = time.monotonic()

    def remaining(self):
        return max(0.1, self.cfg.deadline - (time.monotonic() - self.t0))

    def log_message(self, *args):   # stdlib would log the request line; targets carry OIDC codes
        pass

    def audit(self, session, kind, path, status, clean):
        """One stderr line per forwarded write.  Only allowlisted or hashed values: no note text, cookie, token or raw sub."""
        try:
            body = json.loads(clean)
            actor = hashlib.sha256(str(session["sub"]).encode("utf-8", "replace")).hexdigest()[:12]
            if kind == "launch":   # project id and outcome only: never the objective
                print(f"lmloop edge: audit actor={actor} launch project={body['project']} status={status}",
                      file=sys.stderr, flush=True)
                return
            extra = f" note_bytes={len(body['note'].encode())}" if "note" in body else ""
            print(f"lmloop edge: audit actor={actor} run={path.split('/')[3]} {kind}={body[kind]}{extra} status={status}",
                  file=sys.stderr, flush=True)
        except (OSError, ValueError, KeyError):
            pass

    # -- output ---------------------------------------------------------
    def send(self, status, body=b"", ctype=JSON, extra=()):
        if not isinstance(body, bytes):
            body = json.dumps(body, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for pair in SECURITY + tuple(extra):
            self.send_header(*pair)
        self.end_headers()
        self.wfile.write(body)

    def err(self, status, code, extra=()):
        self.send(status, {"error": code}, extra=extra)

    def lerr(self, status, code, extra=()):   # launch routes: the typed {error:{kind,message}} shape the page parses
        self.send(status, {"error": {"kind": code, "message": LAUNCH_MSG[code]}}, extra=extra)

    def send_error(self, code, message=None, explain=None):   # stdlib's HTML error pages leak nothing, but stay generic
        self.close_connection = True
        self.err(code, "bad_request")

    def redirect(self, location, cookies=()):
        self.send(302, b"", extra=[("Location", location)] + [("Set-Cookie", c) for c in cookies])

    # -- dispatch -------------------------------------------------------
    def do_GET(self):
        self.guarded("GET")

    def do_POST(self):
        self.guarded("POST")

    def refuse(self):
        self.err(405, "method_not_allowed", extra=[("Allow", "GET, POST")])

    do_HEAD = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = refuse

    def guarded(self, method):
        try:
            self.handle_request(method)
        except (BrokenPipeError, ConnectionError, TimeoutError):
            self.close_connection = True
        except Exception as e:  # noqa: BLE001 - nothing but the type may reach logs or clients
            print(f"lmloop edge: handler failed: {type(e).__name__}", file=sys.stderr)
            try:
                self.err(500, "internal")
            except OSError:
                pass

    @property
    def cfg(self):
        return self.server.cfg

    def session(self):
        """The OIDC session for the preview cookie, only if its subject is allowlisted (checked every request)."""
        try:
            jar = SimpleCookie(self.headers.get("Cookie", ""))
            morsel = jar.get(SESSION_COOKIE)
            s = self.server.oidc.session(morsel.value) if morsel else None
        except Exception:  # noqa: BLE001 - a malformed cookie is simply no session
            return None
        exp = s.get("exp") if isinstance(s, dict) else None
        # a validly signed cookie still may not outlive the configured session length
        if (isinstance(exp, (int, float)) and exp - time.time() <= self.server.oidc.session_seconds + 5
                and s.get("sub") in self.cfg.subs and isinstance(s.get("csrf"), str)):
            return s
        return None

    def handle_request(self, method):
        h = self.headers
        if any(len(h.get_all(n) or ()) > 1 for n in SINGLETONS) or "Transfer-Encoding" in h:
            return self.err(400, "bad_request")
        raw_len = h.get("Content-Length")
        if raw_len is not None and not re.fullmatch(r"[0-9]{1,6}", raw_len):
            return self.err(400, "bad_request")
        length = int(raw_len or 0)
        if h.get("Host") != self.cfg.authority:
            return self.err(421, "bad_host")
        if method == "GET" and length:
            return self.err(400, "bad_request")
        if length > MAX_BODY:   # the same cap everywhere; only the launch page gets a message it can show
            return (self.lerr if self.requestline.split(" ")[:2] == ["POST", "/api/runs"] else self.err)(413, "too_large")
        body = self.rfile.read(length) if length else b""   # drained first so error replies are not cut by a reset
        if len(body) != length:
            return self.err(400, "bad_request")

        parts = self.requestline.split(" ")   # stdlib rewrites a leading "//" in self.path; judge what was sent
        if len(parts) != 3:
            return self.err(400, "bad_request")
        target = parts[1]
        path, qmark, query = target.partition("?")
        query = query if query else ("?" if qmark else "")   # a bare trailing "?" is a query, and no route takes an empty one
        if len(target) > 2048 or not re.fullmatch(r"/[\x21-\x7e]*", target) or "#" in target or "\\" in path or "%" in path:
            return self.err(404, "not_found")
        kind = next((k for m, rx, k in ROUTES if m == method and rx.fullmatch(path)), None)
        if kind is None:
            return self.err(404, "not_found")
        if kind == "health":
            if query:
                return self.err(404, "not_found")
            return self.send(200, {"status": "ok", "auth": "oidc", "oidc": True, "read_only": True})
        if kind == "login":
            return self.login() if not query else self.err(404, "not_found")
        if kind in ("signin", "logout"):
            return self.signin(kind == "logout") if not query else self.err(404, "not_found")
        if kind == "public":
            return self.send(200, PUBLIC[path][1], PUBLIC[path][0]) if not query else self.err(404, "not_found")
        if kind == "callback":
            return self.callback(query)
        if kind in ("control", "decision", "launch"):
            return self.write(kind, path, body) if not query else (self.lerr if kind == "launch" else self.err)(404, "not_found")

        # reads
        identity = None
        if kind == "android":   # Android bearer first (same order as web/server.py), then the browser session
            auth = h.get("Authorization", "")   # printable ASCII only: compare_digest raises on anything else
            label = self.server.devices.match(auth) if BEARER.fullmatch(auth) else None
            if label:
                identity = {"name": f"device:{label}", "csrf": "disabled", "bearer": True}
        if kind == "launch_options":   # session only, never the bearer; Origin is optional on a GET but must match if sent
            if h.get("Origin") not in (None, self.cfg.public_origin):
                return self.lerr(403, "bad_origin")
            identity = self.session()
            if identity is None:
                return self.lerr(401, "authentication_required")
        if identity is None:
            identity = self.session()
        if identity is None:
            if path == "/" and not query:   # the sign-in page, never straight to the issuer: SSO would undo a logout
                return self.redirect("/login")
            return self.err(401, "authentication_required")
        if kind == "log":
            m = re.fullmatch(r"|bytes=([0-9]{1,6})", query)
            if not m or (m.group(1) and not 1 <= int(m.group(1)) <= MAX_LOG_BYTES):
                return self.err(404, "not_found")
            target = path + (f"?bytes={int(m.group(1))}" if m.group(1) else "")
        elif query:
            return (self.lerr if kind == "launch_options" else self.err)(404, "not_found")
        expect = ASSETS.get(path, JSON)
        status, data = self.forward("GET", target, "*/*" if path in ASSETS else JSON, expect)
        if kind == "launch_options":   # only the re-typed allowlisted fields ever reach the browser
            opts = clean_options(data) if status == 200 else None
            if opts is None:
                return self.lerr(502, "upstream_error")
            return self.send(200, opts)
        if path == "/api/config" and status == 200:
            try:
                cfg = json.loads(data)
                assert isinstance(cfg, dict)
            except (ValueError, AssertionError):
                return self.err(502, "upstream_error")
            cfg.update(user=identity["name"], csrf=identity["csrf"], auth="oidc", oidc=True,
                       read_only=bool(identity.get("bearer")))
            data = json.dumps(cfg, separators=(",", ":")).encode()
        self.send(status, data, expect)

    # -- login ----------------------------------------------------------
    def cookie(self, name, value, path, max_age):
        return f"{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; Secure; SameSite=Lax"

    def signin(self, clear):
        """The sign-in page (200, no redirect).  /logout also expires every cookie this edge or the legacy dashboard set."""
        cookies = ()
        if clear:
            cookies = [("Set-Cookie", self.cookie(SESSION_COOKIE, "", "/", 0)),
                       ("Set-Cookie", self.cookie(LEGACY_COOKIE, "", "/", 0)),
                       ("Set-Cookie", self.cookie(OIDC_COOKIE, "", "/oauth/callback", 0))]
        self.send(200, SIGNED_OUT if clear else SIGN_IN, HTML, extra=cookies)

    def login(self):
        try:
            location, tx = self.server.oidc.begin()
        except Exception as e:  # noqa: BLE001
            print(f"lmloop edge: login unavailable: {type(e).__name__}", file=sys.stderr)
            return self.err(502, "login_unavailable")
        self.redirect(location, [self.cookie(OIDC_COOKIE, tx, "/oauth/callback", 600)])

    def callback(self, query):
        try:
            pairs = parse_qsl(query, keep_blank_values=True, strict_parsing=True, max_num_fields=8)
        except ValueError:
            return self.err(400, "login_failed")
        q = dict(pairs)
        if len(q) != len(pairs) or not set(q) <= CALLBACK_KEYS or not q.get("code") or not q.get("state") or "error" in q:
            return self.err(400, "login_failed")
        oidc = self.server.oidc
        try:
            jar = SimpleCookie(self.headers.get("Cookie", ""))
            tx = jar[OIDC_COOKIE].value if OIDC_COOKIE in jar else ""
            cookie = oidc.callback(q["code"], q["state"], tx)
        except Exception as e:  # noqa: BLE001
            print(f"lmloop edge: callback failed: {type(e).__name__}", file=sys.stderr)
            return self.err(400, "login_failed")
        claims = oidc.unsign(cookie)
        if not claims or claims.get("sub") not in self.cfg.subs:   # authenticated, but not an operator: no cookie
            return self.err(403, "not_authorized")
        self.redirect("/", [self.cookie(SESSION_COOKIE, cookie, "/", oidc.session_seconds),
                            self.cookie(OIDC_COOKIE, "", "/oauth/callback", 0)])

    # -- writes ---------------------------------------------------------
    def write(self, kind, path, body):
        h = self.headers
        launch = kind == "launch"
        err = self.lerr if launch else self.err
        if h.get("Authorization") is not None:   # bearer tokens (device_auth) can never reach a write
            return err(401, "authentication_required")
        session = self.session()
        if session is None:
            return err(401, "authentication_required")
        if h.get("Origin") != self.cfg.public_origin:
            return err(403, "bad_origin")
        if h.get("Sec-Fetch-Site") not in (None, "same-origin"):
            return err(403, "bad_origin")
        if not hmac.compare_digest(h.get("X-CSRF-Token", "").encode("utf-8", "replace"), session["csrf"].encode()):
            return err(403, "bad_csrf")
        if (h.get("Content-Type") or "").split(";")[0].strip().lower() != JSON:
            return err(415, "bad_content_type")
        clean = parse_launch(body) if launch else parse_write(kind, body)
        if clean is None:
            return err(400, "bad_request")
        if launch:   # Rust answers within its 10 s launch wait; a slower or odd answer is the uncertain 502
            status, data = self.forward("POST", path, JSON, JSON, clean, LAUNCH_STATUSES, LAUNCH_TIMEOUT)
            status, out = launch_reply(status, data)
            self.audit(session, kind, path, status, clean)
            return self.send(status, out)
        status, data = self.forward("POST", path, JSON, JSON, clean)
        self.audit(session, kind, path, status, clean)
        self.send(status, data)

    # -- upstream -------------------------------------------------------
    def forward(self, method, target, accept, expect, body=None, statuses=(200, 400, 404, 409), timeout=IO_TIMEOUT):
        """Rebuild and send one upstream request: (status, bytes), or (502, generic body) on any surprise.

        http.client never follows redirects; a redirect is a non-allowed status here."""
        c = self.cfg
        headers = {"Host": c.backend_authority, "Authorization": c.basic, "Accept": accept}
        if body is not None:
            headers.update({"Origin": f"http://{c.backend_authority}", "Content-Type": JSON})
        left = self.remaining()
        conn = http.client.HTTPConnection("127.0.0.1", c.backend_port, timeout=min(timeout, left))
        try:
            conn.request(method, target, body=body, headers=headers)
            r = conn.getresponse()
            ok_type = (r.getheader("Content-Type") or "").split(";")[0].strip().lower() == expect.split(";")[0]
            if r.status not in ((200,) if expect != JSON else statuses) or not ok_type:
                raise UpstreamError
            data, deadline = b"", time.monotonic() + min(UPSTREAM_DEADLINE, self.remaining())
            while len(data) <= MAX_UPSTREAM:
                chunk = r.read(65536)
                if not chunk:
                    break
                data += chunk
                if time.monotonic() > deadline:
                    raise UpstreamError
            if len(data) > MAX_UPSTREAM:
                raise UpstreamError
            return r.status, data
        except (UpstreamError, OSError, http.client.HTTPException) as e:
            print(f"lmloop edge: upstream failed: {type(e).__name__}", file=sys.stderr)
            return 502, json.dumps({"error": "upstream_error"}).encode()
        finally:
            conn.close()


class Server(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 32

    def __init__(self, address, cfg, oidc, devices):
        super().__init__(address, Handler)
        self.cfg, self.oidc, self.devices = cfg, oidc, devices
        self.slots = threading.BoundedSemaphore(cfg.max_inflight)

    def process_request(self, request, client_address):
        if not self.slots.acquire(blocking=False):
            body = b'{"error":"busy"}'
            try:
                request.settimeout(1)
                request.sendall(b"HTTP/1.0 503 Service Unavailable\r\nContent-Type: application/json\r\n"
                                b"Cache-Control: no-store\r\nContent-Length: %d\r\n\r\n%s" % (len(body), body))
            except OSError:
                pass
            return self.shutdown_request(request)
        super().process_request(request, client_address)

    def process_request_thread(self, request, client_address):
        def expire():   # unblocks any recv/send still stuck on this connection
            try:
                request.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        watchdog = threading.Timer(self.cfg.deadline, expire)
        watchdog.daemon = True
        watchdog.start()
        try:
            super().process_request_thread(request, client_address)
        finally:
            watchdog.cancel()
            self.slots.release()

    def handle_error(self, request, client_address):   # stdlib prints a full traceback and the peer
        print(f"lmloop edge: connection failed: {type(sys.exc_info()[1]).__name__}", file=sys.stderr)


def main(argv=None, env=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--public-origin", required=True, help="exact https://host[:port] the browser uses")
    p.add_argument("--issuer", required=True, help="OIDC issuer, https://")
    p.add_argument("--client-id", required=True)
    p.add_argument("--listen-port", type=int, required=True, help="edge port on 127.0.0.1 (0 = pick one, printed)")
    p.add_argument("--backend-port", type=int, required=True, help="Rust `lmloop web --port` on 127.0.0.1")
    p.add_argument("--auth-file", required=True, help="the Rust backend's 0600 username:password file")
    p.add_argument("--session-hours", type=int, default=12, help=f"1..{MAX_SESSION_HOURS}")
    p.add_argument("--check", action="store_true", help="validate configuration and exit; no bind, no network")
    args = p.parse_args(argv)
    cfg, oidc, devices = build(args, os.environ if env is None else env)
    if args.check:
        print(f"config ok: origin={cfg.public_origin} operators={len(cfg.subs)} devices={len(devices._tokens)}")
        return 0
    server = Server(("127.0.0.1", args.listen_port), cfg, oidc, devices)
    print(f"listening on http://127.0.0.1:{server.server_port}/", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
