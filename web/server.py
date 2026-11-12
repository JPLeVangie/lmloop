"""The lmloop dashboard: start, watch, pause and stop runs from a browser.

Two things make this much smaller than the predecessor dashboard it replaces.

**Runs are controlled by files, not by this process.**  Pausing is `touch PAUSE`
and stopping is `touch STOP`, which the loop notices on its own poll.  The predecessor dashboard
had to own a supervisor: track pids, reconcile them against processes it did not
start, signal process trees, and persist all of that so a restart did not orphan
a run.  Here the web app can crash, be restarted, or be replaced mid-run and
every control still works, because none of them were ever its to hold.

**The present is a file too.**  `status.json` is rewritten every couple of
seconds, so the dashboard reads state instead of replaying an append-only log to
its end.

The safety rule that follows from being able to start and stop agents: without
OIDC configured this server binds loopback only.  An unauthenticated dashboard
with a launch button does not belong on a network, and defaulting to "convenient"
is how that becomes someone's incident.
"""

from __future__ import annotations

import hmac
import json
import mimetypes
import os
import subprocess
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlencode, urlparse

import config as config_module
import harness
import policy
import runrecord
from web import runs as runs_module
from web import service
from web import auth as auth_module
from web import device_auth
from web import push as push_module
from web import push_store

BASE = Path(__file__).resolve().parent
STATIC = BASE / "static"
LMLOOP = str(Path(__file__).resolve().parent.parent / "lmloop.py")
MAX_BODY_BYTES = 1_048_576
MAX_ITERATIONS = service.MAX_ITERATIONS
MAX_ARCHIVE_ENTRIES = 100_000
MAX_ARCHIVE_BYTES = 1_073_741_824

# Served pages may load only their own assets.  The dashboard has no third-party
# anything, so the strictest useful policy is also the one that costs nothing.
CSP = (
    "default-src 'self'; style-src 'self'; script-src 'self'; "
    "worker-src 'self'; img-src 'self' data:; connect-src 'self'"
)


def load_env(path: Path) -> None:
    """Read KEY=value lines without overriding the real environment."""
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return
    for raw in lines:
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        os.environ.setdefault(key.strip(), value.strip())


def _flag(name: str, default: bool) -> bool:
    return os.environ.get(name, str(default)).strip().lower() in ("1", "true", "yes", "on")


def configure() -> dict:
    roots = [
        Path(os.path.expandvars(item)).expanduser()
        for item in os.environ.get("LMLOOP_WEB_ROOTS", str(Path.home() / "git")).split(":")
        if item.strip()
    ]
    return {
        "roots": roots,
        "host": os.environ.get("LMLOOP_WEB_HOST", "127.0.0.1"),
        "port": int(os.environ.get("LMLOOP_WEB_PORT", "8082")),
        "poll_seconds": float(os.environ.get("LMLOOP_WEB_POLL_SECONDS", "3")),
        "hidden_poll_seconds": float(os.environ.get("LMLOOP_WEB_HIDDEN_POLL_SECONDS", "30")),
        "read_only": _flag("LMLOOP_WEB_READ_ONLY", False),
        # No default: see `_fallback_models`.  Empty means the dashboard offers
        # what the agent lists, and nothing of its own invention.
        "default_model": os.environ.get("LMLOOP_WEB_DEFAULT_MODEL", ""),
        "harness": os.environ.get("LMLOOP_WEB_HARNESS", "pi"),
        "default_max_iterations": int(os.environ.get("LMLOOP_WEB_DEFAULT_MAX_ITERATIONS", "20")),
        "default_thinking": os.environ.get("LMLOOP_WEB_DEFAULT_THINKING", "low"),
        "python": os.environ.get("LMLOOP_WEB_PYTHON", "python3"),
    }


def build_auth():
    """The auth for this deployment; see `web.auth.build` for the modes."""
    return auth_module.build({
        "mode": os.environ.get("LMLOOP_WEB_AUTH_MODE", ""),
        "oidc_issuer": os.environ.get("LMLOOP_WEB_OIDC_ISSUER", ""),
        "oidc_client_id": os.environ.get("LMLOOP_WEB_OIDC_CLIENT_ID", ""),
        "oidc_client_secret": config_module.secret(
            os.environ.get("LMLOOP_WEB_OIDC_CLIENT_SECRET", "")),
        "public_url": os.environ.get("LMLOOP_WEB_PUBLIC_URL", ""),
        "session_secret": config_module.secret(
            os.environ.get("LMLOOP_WEB_SESSION_SECRET", "")),
        "session_hours": os.environ.get("LMLOOP_WEB_SESSION_HOURS", "12"),
        "proxy_header": os.environ.get("LMLOOP_WEB_PROXY_HEADER", ""),
        "proxy_display_header": os.environ.get("LMLOOP_WEB_PROXY_NAME_HEADER", ""),
        "trusted_proxies": [
            item.strip()
            for item in os.environ.get("LMLOOP_WEB_TRUSTED_PROXIES", "").split(",")
            if item.strip()
        ],
    })


def build_device_tokens() -> device_auth.DeviceTokens:
    """Read-only bearer tokens for clients with no cookie jar of their own --
    a background service or a periodic job, not a browser. See
    `web.device_auth` for why these can never grant write access."""
    return device_auth.build(os.environ.get("LMLOOP_WEB_DEVICE_TOKENS", ""))


def build_push() -> push_module.WebPush:
    """This deployment's Web Push configuration; see `web.push`."""
    return push_module.build(
        config_module.secret(os.environ.get("LMLOOP_WEB_VAPID_PRIVATE_KEY", "")),
        config_module.reference(os.environ.get("LMLOOP_WEB_VAPID_CONTACT", "")),
        push_store.default_path(),
    )


# `pi --list-models` costs ~2.6s: it starts node, loads every extension, and
# asks llama-swap for its catalogue.  That is fine once and intolerable on every
# page load, so the answer is cached and the endpoint that needs it is separate
# from the one first paint waits on.  Models change when someone installs one.
_MODEL_CACHE: dict = {"at": 0.0, "value": None, "harness": None}
MODEL_CACHE_SECONDS = 300

def _fallback_models(config: dict) -> list[str]:
    """What to offer when the agent cannot be asked.

    Only what the operator configured, if anything.  This used to fall back to
    a hardcoded `llama-swap/local-fast`, which is one person's model name on
    one person's server -- offering it to somebody else produces a run that
    dies on its first request, minutes later, for a reason nothing here
    explains.
    """
    return [config["default_model"]] if config["default_model"] else []


def available_models(config: dict, force: bool = False) -> dict:
    """Model ids the configured agent will accept, asked of it rather than
    guessed.

    A dashboard that offers a model the agent cannot resolve produces a run that
    dies on its first request, minutes later, for a reason nobody can see from
    here.

    The asking and the parsing both belong to the adapter, because the answer
    is agent-shaped: this function once ran every agent's output through pi's
    column parser, and omp's box-drawing table survived it as two models named
    after its provider counts.  What is left here is the part that really is
    the dashboard's -- which failure this was, so the sheet can say so.
    """
    fresh = time.monotonic() - _MODEL_CACHE["at"] < MODEL_CACHE_SECONDS
    if _MODEL_CACHE["value"] and fresh and not force and _MODEL_CACHE.get("harness") == config["harness"]:
        return _MODEL_CACHE["value"]
    agent = config["harness"]
    try:
        adapter = harness.get(agent)
    except SystemExit:
        return {"models": _fallback_models(config), "model_source": "unknown agent"}
    if not adapter.list_models_argv():
        return {"models": _fallback_models(config), "model_source": f"{agent} cannot list"}
    try:
        models = adapter.catalogue()
    # `ValueError` belongs here with the other two: an agent whose catalogue
    # comes back as something other than what it documents has not answered,
    # whether it failed to start or failed to make sense.
    except (OSError, ValueError, subprocess.SubprocessError):
        return {"models": _fallback_models(config), "model_source": "unavailable"}
    result = {
        "models": models or _fallback_models(config),
        "model_source": agent if models else "fallback",
    }
    _MODEL_CACHE.update(at=time.monotonic(), value=result, harness=agent)
    return result


class Handler(BaseHTTPRequestHandler):
    server_version = "lmloop-web"
    config: dict = {}
    auth: OIDC = None
    device_tokens: device_auth.DeviceTokens = None
    push: push_module.WebPush = None

    def log_message(self, fmt, *args):
        print(f"{self.client_address[0]} {fmt % args}")

    # -- plumbing ---------------------------------------------------------

    def _headers(self, status, content_type, length):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(length))
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header("Content-Security-Policy", CSP)

    def json(self, payload, status=200):
        body = json.dumps(payload).encode()
        self._headers(status, "application/json; charset=utf-8", len(body))
        self.send_header("Cache-Control", "no-store")
        BaseHTTPRequestHandler.end_headers(self)
        self.wfile.write(body)

    def static(self, name):
        target = (STATIC / name).resolve()
        if not target.is_relative_to(STATIC.resolve()) or not target.is_file():
            return self.json({"error": "not found"}, 404)
        body = target.read_bytes()
        kind = mimetypes.guess_type(target.name)[0] or "application/octet-stream"
        self._headers(200, f"{kind}; charset=utf-8" if kind.startswith("text") else kind, len(body))
        self.send_header("Cache-Control", "no-cache")
        BaseHTTPRequestHandler.end_headers(self)
        self.wfile.write(body)

    def _content_length(self):
        raw = self.headers.get("Content-Length")
        if raw is None:
            return 0
        try:
            length = int(raw)
        except (TypeError, ValueError):
            return None
        return length if 0 <= length <= MAX_BODY_BYTES else None

    def body(self):
        length = self._content_length()
        if length is None:
            self.json({"error": f"Content-Length must be between 0 and {MAX_BODY_BYTES}"}, 413)
            return None
        try:
            payload = json.loads(self.rfile.read(length) or b"{}")
        except (ValueError, UnicodeDecodeError):
            self.json({"error": "invalid JSON"}, 400)
            return None
        if not isinstance(payload, dict):
            self.json({"error": "JSON object required"}, 400)
            return None
        return payload

    def redirect(self, location, cookies=()):
        self.send_response(302)
        self.send_header("Location", location)
        for cookie in cookies:
            self.send_header("Set-Cookie", cookie)
        self.end_headers()

    def cookies(self):
        from http.cookies import SimpleCookie

        jar = SimpleCookie(self.headers.get("Cookie", ""))
        return {key: morsel.value for key, morsel in jar.items()}

    def session(self):
        """Whoever is asking, however this deployment establishes that.

        Each mode answers for itself -- see `web/auth.py`.  It used to be a
        branch on "is OIDC configured", which had only two answers and so could
        not express the common deployment: an ingress that has already
        authenticated the request.
        """
        return self.auth.session_for(self)

    def require_auth(self, api=False):
        session = self.session()
        if session:
            return session
        if api:
            self.json({"error": "authentication required"}, 401)
        else:
            self.redirect("/login")
        return None

    def device_session(self):
        """A read-only session for a device token, or None.

        Consulted only from `do_GET`, for `/api/*`. **Never call this from
        `do_POST`** -- see `web.device_auth`'s module docstring for why that
        invariant is what keeps a device token from ever being able to
        mutate a run, however it is configured.
        """
        label = self.device_tokens.match(self.headers.get("Authorization", "")) if self.device_tokens else None
        if not label:
            return None
        return {"name": f"device:{label}", "csrf": "disabled"}

    # -- run lookup -------------------------------------------------------

    def _resolve(self, project_id, run_id):
        for project in runs_module.projects(self.config["roots"]):
            if project["id"] != project_id:
                continue
            project_path = Path(project["path"])
            for run_dir in runs_module.run_dirs(project_path):
                if runs_module.route_id(project_path, run_dir) == run_id:
                    owning = runs_module.owner(project_path, run_dir)
                    return dict(project, path=str(owning)), run_dir
            # Archived runs have no worktree to walk.  Checked last, so a
            # re-used run id always resolves to the live run rather than the
            # archived copy of whatever held the name before it.
            target = runs_module.archive_target(project_id, run_id)
            if target.is_dir():
                return project, target
        return None, None

    # -- GET --------------------------------------------------------------

    def do_GET(self):
        parsed = urlparse(self.path)
        path = parsed.path

        if path == "/health":
            return self.json({"status": "ok", "auth": self.auth.mode,
                              "oidc": self.auth.enabled,
                              "read_only": self.config["read_only"]})
        if path == "/login":
            return self.static("login.html") if self.auth.interactive else self.redirect("/")
        if path == "/login/start":
            if not self.auth.interactive:
                return self.redirect("/")
            try:
                location, transaction = self.auth.begin()
            except Exception as error:  # noqa: BLE001 - surfaced to the user
                return self.redirect("/login?" + urlencode({"error": str(error)}))
            return self.redirect(location, [
                f"lmloop_oidc={transaction}; Path=/oauth/callback; Max-Age=600; HttpOnly; Secure; SameSite=Lax"
            ])
        if path == "/oauth/callback":
            query = parse_qs(parsed.query)
            try:
                cookie = self.auth.callback(
                    query.get("code", [""])[0],
                    query.get("state", [""])[0],
                    self.cookies().get("lmloop_oidc", ""),
                )
            except Exception as error:  # noqa: BLE001
                return self.json({"error": f"OIDC callback failed: {error}"}, 400)
            # `Lax`, not `Strict`, and the difference is a whole extra click.
            # This response 302s to `/`, but that request is the tail of a
            # navigation chain that started at the identity provider, so the
            # browser still counts it as cross-site and withholds a `Strict`
            # cookie.  `/` then saw no session, bounced to `/login`, and the
            # sign-in button had to be pressed a second time to land anywhere.
            # `Lax` is sent on exactly this case -- a top-level GET navigation
            # -- and still withheld from cross-site POSTs and subresources;
            # state-changing calls are guarded by the CSRF token regardless.
            return self.redirect("/", [
                f"lmloop_session={cookie}; Path=/; Max-Age={self.auth.session_seconds}; HttpOnly; Secure; SameSite=Lax",
                "lmloop_oidc=; Path=/oauth/callback; Max-Age=0; HttpOnly; Secure; SameSite=Lax",
            ])
        if path == "/logout":
            location = "/"
            if self.auth.interactive:
                location = self.auth.logout_url() + "?" + urlencode(
                    {"post_logout_redirect_uri": self.auth.public_url, "client_id": self.auth.client_id}
                )
            return self.redirect(location, [
                "lmloop_session=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax"
            ])
        if path in ("/sw.js", "/manifest.json"):
            return self.static(path.lstrip("/"))

        if not path.startswith("/static/"):
            session = None
            if path.startswith("/api/"):
                session = self.device_session()
            if not session:
                session = self.require_auth(path.startswith("/api/"))
            if not session:
                return

        if path == "/" or path == "/index.html":
            return self.static("index.html")
        if path.startswith("/static/"):
            return self.static(path[len("/static/"):])

        if path == "/api/config":
            return self.json({
                "poll_seconds": self.config["poll_seconds"],
                "hidden_poll_seconds": self.config["hidden_poll_seconds"],
                "read_only": self.config["read_only"],
                "default_model": self.config["default_model"],
                "default_harness": self.config.get("harness", "pi"),
                "harnesses": sorted(harness._HARNESSES),
                "verifying_harnesses": sorted(
                    name for name in harness._HARNESSES if harness.get(name).verifies_required_tools),
                "default_max_iterations": self.config["default_max_iterations"],
                "default_thinking": self.config["default_thinking"],
                "user": session["name"],
                "csrf": session["csrf"],
                "auth": self.auth.mode,
                "oidc": self.auth.enabled,
                "push_public_key": self.push.public_key if self.push else "",
                # The dashboard's context gauge used to carry its own `0.75`,
                # which agreed with `policy.CONTEXT_PRESSURE` by coincidence.
                # Served rather than duplicated: the number was measured from
                # real overflows and belongs to one file.
                "context_pressure": policy.CONTEXT_PRESSURE,
            })
        if path == "/api/models":
            # Fetched only when the new-run form opens, so first paint never
            # waits on a node process starting up.
            selected = parse_qs(parsed.query).get("harness", [self.config["harness"]])[0]
            if selected not in harness._HARNESSES:
                return self.json({"error": "unknown harness"}, 400)
            return self.json(available_models(
                dict(self.config, harness=selected, default_model=(
                    self.config["default_model"] if selected == self.config["harness"] else "")),
                force="refresh" in parsed.query))
        if path.startswith("/api/projects/") and path.endswith("/agent"):
            project_id = path[len("/api/projects/"): -len("/agent")]
            match = [p for p in runs_module.projects(self.config["roots"]) if p["id"] == project_id]
            if not match:
                return self.json({"error": "no such project"}, 404)
            try:
                agent = config_module.load(Path(match[0]["path"]))["agent"]
            except SystemExit as error:
                return self.json({"error": str(error)}, 400)
            return self.json({key: agent[key] for key in ("harness", "model", "thinking", "required_tools")})
        if path == "/api/projects":
            return self.json({"projects": runs_module.projects(self.config["roots"])})
        if path == "/api/system-prompt":
            try:
                text = config_module.SYSTEM_PROMPT.read_text()
            except OSError:
                text = ""
            return self.json({"text": text, "path": str(config_module.SYSTEM_PROMPT)})
        if path.startswith("/api/projects/") and path.endswith("/beads"):
            project_id = path[len("/api/projects/"): -len("/beads")]
            match = [p for p in runs_module.projects(self.config["roots"]) if p["id"] == project_id]
            if not match:
                return self.json({"error": "no such project"}, 404)
            issues = runs_module.beads_issues(match[0]["path"])
            return self.json({"issues": issues})
        if path == "/api/runs":
            return self.json({"runs": runs_module.all_runs(self.config["roots"])})
        if path.startswith("/api/runs/"):
            parts = path[len("/api/runs/"):].split("/")
            if len(parts) == 2:
                project, run_dir = self._resolve(*parts)
                if not run_dir:
                    return self.json({"error": "no such run"}, 404)
                return self.json(runs_module.detail(project, run_dir))
            if len(parts) == 3 and parts[2] == "preview":
                project, run_dir = self._resolve(parts[0], parts[1])
                if not run_dir:
                    return self.json({"error": "no such run"}, 404)
                status, reply = service.preview_status(project, run_dir)
                return self.json(reply, status)

        return self.json({"error": "not found"}, 404)

    # -- POST -------------------------------------------------------------

    def do_POST(self):
        path = urlparse(self.path).path
        session = self.require_auth(api=True)
        if not session:
            return
        # Push-subscription management is dashboard preference, not a run
        # mutation -- read-only mode exists to guard runs, not to stop a
        # browser from asking to be notified about them.
        if self.config["read_only"] and not path.startswith("/api/push/"):
            return self.json({"error": "read-only mode"}, 403)
        if self.auth.mode == "proxy" and not self.auth.same_origin(self):
            return self.json({"error": "same-origin request required"}, 403)
        # Only a cookie session needs one: CSRF is about a browser attaching
        # an ambient credential to somebody else's request, and the other modes
        # have no ambient credential to attach.
        if self.auth.interactive and not hmac.compare_digest(
            self.headers.get("X-CSRF-Token", ""), session.get("csrf", "")
        ):
            return self.json({"error": "bad CSRF token"}, 403)

        if path == "/api/projects":
            return self.create_project(self.body())
        if path == "/api/runs":
            return self.start_run(self.body())
        if path == "/api/system-prompt":
            payload = self.body()
            if payload is None:
                return
            text = payload.get("text", "")
            if not isinstance(text, str) or len(text) > 20_000:
                return self.json({"error": "text must be a string of at most 20000 characters"}, 400)
            target = config_module.SYSTEM_PROMPT
            target.parent.mkdir(parents=True, exist_ok=True)
            tmp = target.with_suffix(".tmp")
            tmp.write_text(text)
            tmp.replace(target)
            return self.json({"saved": True})
        if path == "/api/push/subscribe":
            return self.push_subscribe(self.body())
        if path == "/api/push/unsubscribe":
            return self.push_unsubscribe(self.body())
        if path.startswith("/api/runs/"):
            parts = path[len("/api/runs/"):].split("/")
            if len(parts) == 3 and parts[2] == "preview":
                project, run_dir = self._resolve(parts[0], parts[1])
                if not run_dir or not project:
                    return self.json({"error": "no such run"}, 404)
                payload = self.body() or {}
                action = (payload.get("action") or "").strip()
                status, reply = service.preview_control(project, run_dir, action)
                return self.json(reply, status)
            if len(parts) == 4 and parts[2] == "preview":
                project, run_dir = self._resolve(parts[0], parts[1])
                if not run_dir or not project:
                    return self.json({"error": "no such run"}, 404)
                status, reply = service.preview_control(project, run_dir, parts[3])
                return self.json(reply, status)
            if len(parts) == 3:
                project, run_dir = self._resolve(parts[0], parts[1])
                if not run_dir:
                    return self.json({"error": "no such run"}, 404)
                return self.control(project, run_dir, parts[2], self.body())
        return self.json({"error": "not found"}, 404)

    def create_project(self, payload):
        status, reply = service.create_project(payload, self.config)
        return self.json(reply, status)

    def start_run(self, payload):
        status, reply = service.start_run(payload, self.config, LMLOOP)
        return self.json(reply, status)

    def push_subscribe(self, payload):
        if not self.push or not self.push.enabled:
            return self.json({"error": "push not configured"}, 400)
        if not payload.get("endpoint"):
            return self.json({"error": "missing endpoint"}, 400)
        push_store.add(self.push.store_path, payload)
        return self.json({"ok": True})

    def push_unsubscribe(self, payload):
        if self.push:
            push_store.remove(self.push.store_path, payload.get("endpoint", ""))
        return self.json({"ok": True})

    def control(self, project, run_dir, action, payload):
        """Route one control action.

        The five that are an operation live in `service.control`; the three
        that are a destination are dispatched here, because routing is what
        this class is for.
        """
        answered = service.control(
            project, run_dir, action, payload, self.config, LMLOOP)
        if answered is not None:
            status, reply = answered
            return self.json(reply, status)
        if action == "archive":
            return self.archive_run(project, run_dir)
        if action == "delete":
            return self.delete_run(project, run_dir, payload)
        if action == "merge":
            return self.merge_local(project, run_dir, payload)
        if action == "approval":
            status, reply = service.approval(project, run_dir, payload or {}, self.config, LMLOOP)
            return self.json(reply, status)
        if action == "pr":
            return self.open_pr(project, run_dir, payload)
        return self.json({"error": f"unknown action {action}"}, 400)

    # -- archive, delete, PR ----------------------------------------------
    #
    # CLAUDE.md's first invariant is that nothing is ever discarded, and these
    # three are the only code in the project that removes anything.  They are
    # built so that no single action destroys evidence:
    #
    #   archive  copies the run out, *verifies the copy*, then asks git to remove
    #            only a clean worktree -- never with --force -- and leaves the
    #            branch alone.  The run stays readable in the dashboard.
    #   delete   refuses to run on anything that has not been archived first,
    #            so the destructive step is always the second one.

    def archive_run(self, project, run_dir):
        status, reply = service.archive_run(project, run_dir)
        return self.json(reply, status)

    def delete_run(self, project, run_dir, payload):
        status, reply = service.delete_run(project, run_dir, payload)
        return self.json(reply, status)

    def merge_local(self, project, run_dir, payload):
        status, reply = service.merge_local(project, run_dir, payload)
        return self.json(reply, status)

    def open_pr(self, project, run_dir, payload):
        status, body = service.open_pr(project, run_dir, payload)
        return self.json(body, status)

def serve(config: dict | None = None) -> int:
    config = config or configure()
    auth = build_auth()

    if not auth.trusted and config["host"] not in auth_module.LOOPBACK:
        raise SystemExit(
            f"lmloop web: refusing to bind {config['host']} with auth mode"
            f" `{auth.mode}`.\n"
            "  This dashboard starts and stops agents, deletes archives and opens\n"
            "  pull requests; unauthenticated, that does not belong on a network.\n"
            "  Either:\n"
            "    LMLOOP_WEB_AUTH_MODE=proxy  behind an ingress that authenticates,\n"
            "                                with LMLOOP_WEB_TRUSTED_PROXIES set\n"
            "    LMLOOP_WEB_AUTH_MODE=oidc   with LMLOOP_WEB_OIDC_* configured\n"
            "    LMLOOP_WEB_HOST=127.0.0.1   and reach it over an SSH tunnel"
        )

    Handler.config = config
    Handler.auth = auth
    Handler.device_tokens = build_device_tokens()
    Handler.push = build_push()
    httpd = ThreadingHTTPServer((config["host"], config["port"]), Handler)
    print(f"lmloop web on http://{config['host']}:{config['port']}")
    print(f"  roots     {', '.join(str(root) for root in config['roots'])}")
    print(f"  auth      {auth.mode}"
          + ("" if auth.trusted else " (loopback only)"))
    print(f"  mode      {'read-only' if config['read_only'] else 'read-write'}")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\nstopped")
    return 0
