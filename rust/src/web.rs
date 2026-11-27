//! Local web adapter over `operator::Operator` (M3): the same read, control and
//! decision operations the TUI uses, as JSON over loopback HTTP. It owns no run
//! logic, launches no worker except the existing fixture `resume` and, only with
//! `--launch-registry`, a one-task pilot from an operator-owned template (`project.rs`:
//! a client picks a project id and objective text, never argv, path, scope or timeout).
//! FIXTURE EVIDENCE ONLY: production auth, PWA and Android are separate work.
//!
//! Boundary (every request, in `guard`):
//! * Binds `127.0.0.1` only; there is no host option. `Host` must equal the bound
//!   `ip:port` exactly (stops DNS rebinding). `Forwarded`/`X-Forwarded-*` are ignored.
//! * Behind `--public-origin`, `GET` may omit `Origin` (top-level navigation does);
//!   any present `Origin` must still equal it. Basic auth is checked on every route
//!   except `GET /health`, which Android's setup probe calls without credentials; it
//!   still needs the exact Host/Origin and answers only fixed readiness fields.
//! * Android read contract (additive; bearer/OIDC is not part of it): `GET /api/config`,
//!   `GET /api/runs/lmloop/<id>`, and Android aliases on `/api/runs` rows (see `with_android`).
//! * `POST` also needs `Origin: http://<ip:port>` exactly, `Content-Type:
//!   application/json`, and, when sent, `Sec-Fetch-Site: same-origin`. A browser on
//!   another origin cannot satisfy that; there is no CORS, so it cannot read replies.
//!   curl must therefore send both headers, for example:
//!   `curl -sS -X POST -H 'Origin: http://127.0.0.1:PORT' -H 'Content-Type: application/json' -d '{}' http://127.0.0.1:PORT/api/runs/ID/resume`
//! * Every response is `no-store`, `nosniff`, framed-denied, with a strict CSP.
//! * At most `MAX_INFLIGHT` requests run at once (503 `busy` beyond), bodies are
//!   capped at `MAX_BODY`, log answers by the Operator's 64 KiB cap, lists at 100.
//! * Operator I/O runs on the blocking pool. `resume` blocks for the whole resumed
//!   run, so at most one runs per process (`RESUMING`; the engine's signal scope is
//!   process-global too) and the rest of the API stays responsive meanwhile.
//! * Errors reaching a client carry the Operator's own refusal reason; store/IO
//!   failures are reported as a generic 500 and detailed only on stderr.
//!
//! Signals: the server installs no handler, so SIGINT/SIGTERM end an idle server
//! at once (nothing in flight owns anything the OS does not release). While a
//! resume runs, the engine's handlers are installed instead: the signal tears down
//! the worker and the run ends, but the server itself keeps serving; stop it again
//! afterwards (or SIGKILL it).
//!
//! shortcut: no per-connection header timeout or connection cap (loopback only);
//! upgrade before any non-loopback exposure. View `warnings`/`problem` text may
//! name state-root paths.

// An early-return `Response` is the natural error here; boxing it buys nothing.
#![allow(clippy::result_large_err)]

use crate::lease::LeaseState;
use crate::operator::{Control, Decision, Operator, RunKind, RunSummary};
use crate::project::Registry;
use crate::state::{Error, Result as StateResult};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::Read;
use std::net::Ipv4Addr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Default for `lmloop web --port` (the Python dashboard keeps 8082).
pub const DEFAULT_PORT: u16 = 8083;
const MAX_INFLIGHT: usize = 32;
const MAX_BODY: usize = 8 * 1024;
const DEFAULT_LOG_BYTES: usize = 16 * 1024;
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");

/// One resume per process: the engine's signal scope is process-global.
static RESUMING: AtomicBool = AtomicBool::new(false);

struct Ctx {
    root: PathBuf,
    authority: String,
    origin: String,
    public_origin: Option<String>,
    auth: Option<(String, String)>,
    inflight: AtomicUsize,
    launch: Option<Registry>,
}

struct Slot<'a>(&'a AtomicUsize);

impl<'a> Slot<'a> {
    fn take(n: &'a AtomicUsize) -> Option<Self> {
        if n.fetch_add(1, Ordering::SeqCst) >= MAX_INFLIGHT {
            n.fetch_sub(1, Ordering::SeqCst);
            None
        } else {
            Some(Self(n))
        }
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serve `root` (a resolved state root) on 127.0.0.1:`port` until killed. Prints
/// `listening on http://127.0.0.1:<port>/` once bound (port 0 picks a free one).
pub fn run(
    root: &FsPath,
    port: u16,
    auth_file: Option<&FsPath>,
    public_origin: Option<&str>,
    launch_registry: Option<&FsPath>,
) -> StateResult<()> {
    let auth = auth_file.map(read_auth).transpose()?;
    let launch = launch_registry.map(Registry::load).transpose()?;
    if launch.is_some() && auth.is_none() {
        return Err(Error::new(
            "usage",
            "--launch-registry requires --auth-file",
        ));
    }
    if public_origin.is_some() && auth.is_none() {
        return Err(Error::new("usage", "--public-origin requires --auth-file"));
    }
    if public_origin.is_some_and(|s| !valid_public_origin(s)) {
        return Err(Error::new(
            "usage",
            "--public-origin must be an exact https://host[:port] origin",
        ));
    }
    let io = |e: std::io::Error| Error::new("io", format!("web: {e}"));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io)?;
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(io)?;
        let addr = listener.local_addr().map_err(io)?;
        assert!(addr.ip().is_loopback(), "web must stay on loopback");
        println!("listening on http://{addr}/");
        let ctx = Arc::new(Ctx {
            root: root.to_path_buf(),
            authority: addr.to_string(),
            origin: format!("http://{addr}"),
            public_origin: public_origin.map(str::to_owned),
            auth,
            inflight: AtomicUsize::new(0),
            launch,
        });
        axum::serve(listener, router(ctx)).await.map_err(io)
    })
}

fn read_auth(path: &FsPath) -> StateResult<(String, String)> {
    let io = |e: std::io::Error| Error::new("usage", format!("invalid auth file: {e}"));
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(io)?;
    let m = f.metadata().map_err(io)?;
    // Test credentials only; never accept credentials readable by another user.
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o777 != 0o600
        || m.len() > 1024
    {
        return Err(Error::new(
            "usage",
            "auth file must be a regular file owned by this user, mode 0600, and at most 1024 bytes",
        ));
    }
    let mut text = String::new();
    f.take(1025).read_to_string(&mut text).map_err(io)?;
    if text.len() > 1024 {
        return Err(Error::new("usage", "auth file is too large"));
    }
    let line = text.strip_suffix('\n').unwrap_or(&text);
    let (user, password) = line
        .split_once(':')
        .ok_or_else(|| Error::new("usage", "auth file must contain username:password"))?;
    if user.is_empty()
        || password.is_empty()
        || user.contains([':', '\r', '\n'])
        || password.contains(['\r', '\n'])
    {
        return Err(Error::new(
            "usage",
            "auth file must contain non-empty username and password on one line",
        ));
    }
    Ok((user.to_owned(), password.to_owned()))
}

fn valid_public_origin(s: &str) -> bool {
    let Some(authority) = s.strip_prefix("https://") else {
        return false;
    };
    if authority.is_empty() || authority.contains(['/', '?', '#', '@', '\\']) {
        return false;
    }
    let Ok(authority) = authority.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    if authority.as_str().ends_with(':')
        || authority
            .port()
            .is_some_and(|p| p.as_str().parse::<u16>().is_err())
    {
        return false;
    }
    let host = authority.host();
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
        })
}

fn basic_value(credentials: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::from("Basic ");
    for chunk in credentials.as_bytes().chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }),
        )
        .route(
            "/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route(
            "/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route("/health", get(health))
        .route(
            "/api/config",
            get(|| async { axum::Json(json!({ "poll_seconds": 5.0, "read_only": false })) }),
        )
        .route("/api/runs", get(list).post(launch))
        .route("/api/launch-options", get(launch_options))
        .route("/api/runs/{project}/{id}", get(android_view))
        .route("/api/runs/{id}", get(view))
        .route("/api/runs/{id}/logs/{name}", get(log))
        .route("/api/runs/{id}/control", post(control))
        .route("/api/runs/{id}/resume", post(resume))
        .route("/api/runs/{id}/decision", post(decision))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(ctx.clone(), guard))
        .with_state(ctx)
}

fn asset(kind: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, kind)], body).into_response()
}

/// The single header value, or `None` if absent, repeated or not text.
fn one(h: &HeaderMap, name: HeaderName) -> Option<&str> {
    let mut v = h.get_all(name).iter();
    let first = v.next()?;
    if v.next().is_some() {
        return None;
    }
    first.to_str().ok()
}

fn admit(c: &Ctx, req: &Request) -> Result<(), Response> {
    let h = req.headers();
    let host = one(h, header::HOST);
    let request_origin = one(h, header::ORIGIN);
    let local = host == Some(c.authority.as_str());
    // Browsers omit Origin on top-level GET navigation; a present Origin must still match.
    let origin_ok = if request_origin.is_none() {
        req.method() == Method::GET && !h.contains_key(header::ORIGIN)
    } else {
        c.public_origin.as_deref() == request_origin
    };
    let public = c.auth.is_some()
        && origin_ok
        && host
            == c.public_origin
                .as_deref()
                .and_then(|o| o.strip_prefix("https://"));
    if !local && !public {
        return Err(problem(
            StatusCode::FORBIDDEN,
            "bad_host",
            "Host and Origin must match the configured server origin",
        ));
    }
    // The Android client's setup probe sends no credentials by design; the exact-host
    // check above still applies and the reply is only `health`'s fixed readiness fields.
    let health = req.method() == Method::GET && req.uri().path() == "/health";
    if let Some((user, password)) = c.auth.as_ref().filter(|_| !health) {
        let auth = one(h, header::AUTHORIZATION);
        let expected = basic_value(&format!("{user}:{password}"));
        if auth != Some(expected.as_str()) {
            let mut r = problem(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "authentication required",
            );
            r.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"lmloop\", charset=\"UTF-8\""),
            );
            return Err(r);
        }
    }
    if req.method() != Method::POST {
        return Ok(());
    }
    let expected_origin = if local {
        c.origin.as_str()
    } else {
        c.public_origin.as_deref().unwrap_or("")
    };
    let same_site =
        one(h, HeaderName::from_static("sec-fetch-site")).is_none_or(|v| v == "same-origin");
    if request_origin != Some(expected_origin) || !same_site {
        return Err(problem(
            StatusCode::FORBIDDEN,
            "bad_origin",
            "POST needs Origin equal to this server's own origin",
        ));
    }
    let json = one(h, header::CONTENT_TYPE).is_some_and(|ct| {
        ct.split(';')
            .next()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
    });
    if !json {
        return Err(problem(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "bad_content_type",
            "POST needs Content-Type: application/json",
        ));
    }
    Ok(())
}

async fn guard(State(c): State<Arc<Ctx>>, req: Request, next: Next) -> Response {
    let mut resp = 'done: {
        let Some(_slot) = Slot::take(&c.inflight) else {
            break 'done problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                "too many requests in flight",
            );
        };
        if let Err(r) = admit(&c, &req) {
            break 'done r;
        }
        next.run(req).await
    };
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    resp
}

fn problem(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = json!({ "error": { "kind": kind, "message": message } });
    (status, axum::Json(body)).into_response()
}

fn fail(e: Error) -> Response {
    let status = match e.kind {
        "usage" | "note_required" => StatusCode::BAD_REQUEST,
        "no_run" | "no_log" => StatusCode::NOT_FOUND,
        "unavailable" | "ineligible" | "stale_revision" | "busy" => StatusCode::CONFLICT,
        "no_project" => StatusCode::NOT_FOUND,
        _ => {
            let line: String = e
                .to_string()
                .chars()
                .map(|c| if c.is_control() { '?' } else { c })
                .take(300)
                .collect();
            eprintln!("lmloop web: {line}");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.kind,
                "the run store reported an error; details are in the server log",
            );
        }
    };
    problem(status, e.kind, &e.message)
}

/// Run Operator I/O off the async threads and answer with its JSON or refusal.
async fn blocking<T: Serialize + Send + 'static>(
    f: impl FnOnce() -> StateResult<T> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => axum::Json(v).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "handler failed",
        ),
    }
}

/// An empty body means `{}`; unknown fields are refused.
fn parse<T: DeserializeOwned>(body: &Bytes) -> Result<T, Response> {
    let raw: &[u8] = if body.is_empty() { b"{}" } else { body };
    // serde would also take a JSON array for a struct; only an object is the contract.
    let object = raw.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
    match serde_json::from_slice(raw) {
        Ok(v) if object => Ok(v),
        _ => Err(problem(
            StatusCode::BAD_REQUEST,
            "usage",
            "body is not the expected JSON object",
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlBody {
    control: Control,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionBody {
    decision: Decision,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogQuery {
    bytes: Option<usize>,
}

/// The Rust store has no per-run project; Android's `/api/runs/<project>/<id>` gets this one.
const PROJECT: &str = "lmloop";

/// `summary` as the Android client's `RunSummary` reads it: every Rust field kept, plus
/// `route_id`, `project`, `state`, `title`, `paused`, `stopping`. Fields the Operator
/// does not know (iteration, plan, model, agent, commits) are left out, never invented.
/// `title` is the run id (the Python dashboard's own last fallback). Android's `state`
/// vocabulary is coarser: `failed`/`interrupted` and a diagnostic `gate_finished` read
/// `stopped` (a gate probe never completes an objective); `completed` is only a serial run
/// whose tasks are all done without a failure. The original `phase`/`failure` stay beside it.
fn android_state(s: &RunSummary) -> &'static str {
    let held = s
        .lease
        .as_ref()
        .is_some_and(|l| l.state == LeaseState::Held);
    let stop = s.pending_controls.iter().any(|c| c != "PAUSE");
    match (s.kind, s.phase.as_str()) {
        (RunKind::Invalid, _) => "unknown",
        (RunKind::SerialFixture | RunKind::SerialPilot, "complete")
            if s.tasks_done == s.tasks_total && s.failure.is_none() =>
        {
            "completed"
        }
        // An unverifiable `complete` is not terminal for Android.
        (RunKind::SerialFixture | RunKind::SerialPilot, "complete") => "unknown",
        (_, "stopped" | "interrupted" | "failed" | "gate_finished") => "stopped",
        (_, "paused") => "paused",
        _ if held && stop => "stopping",
        _ if held => "running",
        _ => "stale",
    }
}

fn with_android(s: &RunSummary, mut v: Value) -> Value {
    let state = android_state(s);
    v["route_id"] = json!(s.run_id);
    v["project"] = json!(PROJECT);
    v["state"] = json!(state);
    v["title"] = json!(s.run_id);
    v["paused"] = json!(state == "paused");
    v["stopping"] = json!(state == "stopping");
    v
}

/// Android's setup probe: fixed, non-sensitive readiness (`read_only` is false because the
/// Rust console does have mutations; they stay behind `admit`). `auth` is kept because
/// Android's `HealthResponse` requires `auth`/`oidc`/`read_only` with no defaults, so
/// dropping it fails setup; it only says Basic is on, and the 401s on every other route
/// already reveal that.
async fn health(State(c): State<Arc<Ctx>>) -> Response {
    let auth = if c.auth.is_some() { "basic" } else { "none" };
    axum::Json(json!({ "status": "ok", "auth": auth, "oidc": false, "read_only": false }))
        .into_response()
}

async fn list(State(c): State<Arc<Ctx>>) -> Response {
    blocking(move || {
        Operator::new(&c.root).list_runs().map(|runs| {
            let runs: Vec<Value> = runs.iter().map(|r| with_android(r, json!(r))).collect();
            json!({ "runs": runs })
        })
    })
    .await
}

async fn android_view(
    State(c): State<Arc<Ctx>>,
    Path((project, id)): Path<(String, String)>,
) -> Response {
    if project != PROJECT {
        return fail(Error::new("no_run", format!("no such project: {project}")));
    }
    blocking(move || {
        let v = Operator::new(&c.root).view(&id)?;
        Ok(with_android(&v.summary, json!(v)))
    })
    .await
}

async fn view(State(c): State<Arc<Ctx>>, Path(id): Path<String>) -> Response {
    blocking(move || Operator::new(&c.root).view(&id)).await
}

async fn log(
    State(c): State<Arc<Ctx>>,
    Path((id, name)): Path<(String, String)>,
    Query(q): Query<LogQuery>,
) -> Response {
    let bytes = q.bytes.unwrap_or(DEFAULT_LOG_BYTES);
    blocking(move || Operator::new(&c.root).read_log(&id, &name, bytes)).await
}

async fn control(State(c): State<Arc<Ctx>>, Path(id): Path<String>, body: Bytes) -> Response {
    let b: ControlBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    blocking(move || Operator::new(&c.root).control(&id, b.control)).await
}

async fn decision(State(c): State<Arc<Ctx>>, Path(id): Path<String>, body: Bytes) -> Response {
    let b: DecisionBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    blocking(move || Operator::new(&c.root).decide(&id, b.decision, &b.note)).await
}

/// Frees `RESUMING` when the resume thread ends, even if the client has gone.
struct ResumeFlag;

impl Drop for ResumeFlag {
    fn drop(&mut self) {
        RESUMING.store(false, Ordering::SeqCst);
    }
}

async fn resume(State(c): State<Arc<Ctx>>, Path(id): Path<String>, body: Bytes) -> Response {
    if let Err(r) = parse::<Empty>(&body) {
        return r;
    }
    if RESUMING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return problem(
            StatusCode::CONFLICT,
            "busy",
            "a resume is already running in this server",
        );
    }
    let flag = ResumeFlag;
    blocking(move || {
        let _flag = flag;
        let o = Operator::new(&c.root).resume(&id)?;
        if o.outcome == "checkout_busy" {
            return Err(Error::new("busy", o.message));
        }
        Ok(o)
    })
    .await
}

/// Review data for "New run": project, allowed scopes and budget only (no argv, paths, prompt).
async fn launch_options(State(c): State<Arc<Ctx>>) -> Response {
    blocking(move || {
        Ok(match &c.launch {
            Some(r) => r.options(&c.root),
            None => json!({ "enabled": false, "projects": [] }),
        })
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchBody {
    project: String,
    objective: String,
    revision: String,
}

/// How long a launch may take to create its run before the client is told the outcome is unknown
/// (the launch may still create it later).
const LAUNCH_VISIBLE: std::time::Duration = std::time::Duration::from_secs(10);

/// Start one registry-template pilot through `serial::start_pilot`. Shares `RESUMING`, so at
/// most one launch or resume runs per process. Answers 202 `{id}` once the run exists at
/// `/api/runs/<id>`; the pilot then runs detached from the request, and only STOP-NOW
/// (`/api/runs/<id>/control`) controls it.
async fn launch(State(c): State<Arc<Ctx>>, body: Bytes) -> Response {
    if c.launch.is_none() {
        return problem(
            StatusCode::NOT_FOUND,
            "launch_disabled",
            "this server has no launch registry",
        );
    }
    let b: LaunchBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if RESUMING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return problem(
            StatusCode::CONFLICT,
            "busy",
            "a launch or resume is already running in this server",
        );
    }
    let flag = ResumeFlag; // dropped with the closure if the launch is refused before it starts
    let id = new_run_id();
    let (c2, id2) = (c.clone(), id.clone());
    // One deadline covers registry/Git preparation and the wait for run.json. On timeout the
    // blocking task is not cancelled: it may still start the pilot (the slot stays held until
    // that thread ends), so the 504 below is "unknown, possibly a run later", never "refused".
    let t0 = std::time::Instant::now();
    let started = tokio::time::timeout(
        LAUNCH_VISIBLE,
        tokio::task::spawn_blocking(move || {
            let registry = c2.launch.as_ref().expect("checked");
            let (policy, shot) =
                registry.prepare(&c2.root, &b.project, &b.objective, &b.revision)?;
            Ok(std::thread::spawn(move || {
                let _flag = flag;
                Operator::new(&c2.root).launch_pilot_guarded(&id2, policy, |p| shot.claim(p))
            }))
        }),
    )
    .await;
    let thread = match started {
        Ok(Ok(Ok(t))) => t,
        Ok(Ok(Err(e))) => return fail(e),
        Ok(Err(_)) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "handler failed",
            );
        }
        Err(_) => return launch_pending(),
    };
    let run_json = || {
        crate::state::Store::new(&c.root)
            .run_dir(&id, false)
            .ok()
            .flatten()
            .is_some_and(|d| d.join("run.json").exists())
    };
    while t0.elapsed() < LAUNCH_VISIBLE {
        // Read `finished` before the check: once the thread is done, run.json is final, so a run
        // published just before an early error or reconciliation is still accepted.
        let finished = thread.is_finished();
        if run_json() {
            return accepted(&id);
        }
        if finished {
            // Ended without ever publishing run.json: a true pre-publication refusal.
            return match thread.join() {
                Ok(Ok(o)) if o.outcome == "checkout_busy" => problem(
                    StatusCode::CONFLICT,
                    "busy",
                    "another run holds this checkout; nothing was started",
                ),
                Ok(Ok(o)) => problem(
                    StatusCode::CONFLICT,
                    o.outcome,
                    "the launch was refused before a run was created",
                ),
                Ok(Err(e)) => fail(e),
                Err(_) => problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "launch failed",
                ),
            };
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    launch_pending()
}

fn launch_pending() -> Response {
    problem(
        StatusCode::GATEWAY_TIMEOUT,
        "launch_pending",
        "the launch has not created its run yet; check the run list before retrying",
    )
}

fn accepted(id: &str) -> Response {
    (StatusCode::ACCEPTED, axum::Json(json!({ "id": id }))).into_response()
}

/// `web-<unix millis>-<pid>-<n>`: simple, sortable, never client-chosen, unique per process by
/// the counter. `launch` accepts on any published run.json for the id, so it relies on that
/// uniqueness rather than on the engine's duplicate refusal (which is a 409 only if nothing
/// was published, never reached for a fresh id).
fn new_run_id() -> String {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!(
        "web-{ms}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::LeaseInfo;

    fn sum(
        kind: RunKind,
        phase: &str,
        done: usize,
        failure: Option<&str>,
        held: bool,
        ctl: &[&str],
    ) -> RunSummary {
        RunSummary {
            run_id: "r".into(),
            kind,
            phase: phase.into(),
            lease: Some(LeaseInfo {
                state: if held {
                    LeaseState::Held
                } else {
                    LeaseState::Free
                },
                pid: None,
                command: None,
                started_at_unix: None,
            }),
            owner_lost: false,
            tasks_done: done,
            tasks_total: 2,
            failure: failure.map(Into::into),
            pending_controls: ctl.iter().map(|c| c.to_string()).collect(),
            problem: None,
            repo_label: None,
            active_task: None,
            updated_at_unix: None,
        }
    }

    #[test]
    fn android_state_never_fabricates_completion() {
        use RunKind::*;
        let cases = [
            (
                sum(SerialFixture, "complete", 2, None, false, &[]),
                "completed",
            ),
            (
                sum(SerialPilot, "complete", 2, None, false, &[]),
                "completed",
            ),
            (
                sum(SerialFixture, "complete", 1, None, false, &[]),
                "unknown",
            ),
            (
                sum(SerialFixture, "complete", 2, Some("t: x (y)"), false, &[]),
                "unknown",
            ),
            (
                sum(SerialFixture, "failed", 1, Some("t: x (y)"), false, &[]),
                "stopped",
            ),
            (
                sum(SerialFixture, "stopped", 1, None, false, &[]),
                "stopped",
            ),
            (sum(SerialFixture, "paused", 1, None, false, &[]), "paused"),
            (
                sum(SerialFixture, "worker", 1, None, true, &["STOP"]),
                "stopping",
            ),
            (
                sum(SerialFixture, "worker", 1, None, true, &["PAUSE"]),
                "running",
            ),
            (sum(SerialFixture, "worker", 1, None, false, &[]), "stale"),
            (
                sum(DiagnosticGate, "gate_finished", 0, None, false, &[]),
                "stopped",
            ),
            (
                sum(DiagnosticGate, "interrupted", 0, None, false, &[]),
                "stopped",
            ),
            (
                sum(DiagnosticGate, "gate_running", 0, None, true, &[]),
                "running",
            ),
            (
                sum(DiagnosticGate, "gate_running", 0, None, true, &["STOP-NOW"]),
                "stopping",
            ),
            (sum(Invalid, "invalid", 0, None, false, &[]), "unknown"),
            (sum(Invalid, "complete", 2, None, false, &[]), "unknown"),
        ];
        for (s, want) in cases {
            assert_eq!(android_state(&s), want, "{:?} {}", s.kind, s.phase);
        }
    }
}
