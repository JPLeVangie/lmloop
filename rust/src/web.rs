//! Local web adapter over `operator::Operator` (M3): the same read, control and
//! decision operations the TUI uses, as JSON over loopback HTTP. It owns no run
//! logic, launches no worker (only the existing fixture `resume`), and takes no
//! argv, path or import from a client. FIXTURE EVIDENCE ONLY: production auth,
//! PWA, Android and the pilot are separate work.
//!
//! Boundary (every request, in `guard`):
//! * Binds `127.0.0.1` only; there is no host option. `Host` must equal the bound
//!   `ip:port` exactly (stops DNS rebinding). `Forwarded`/`X-Forwarded-*` are ignored.
//! * Behind `--public-origin`, `GET` may omit `Origin` (top-level navigation does);
//!   any present `Origin` must still equal it. Auth is checked on every route.
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

use crate::operator::{Control, Decision, Operator};
use crate::state::{Error, Result as StateResult};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
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
) -> StateResult<()> {
    let auth = auth_file.map(read_auth).transpose()?;
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
        .route("/api/runs", get(list))
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
    if let Some((user, password)) = &c.auth {
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
        "unavailable" => StatusCode::CONFLICT,
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

async fn list(State(c): State<Arc<Ctx>>) -> Response {
    blocking(move || {
        Operator::new(&c.root)
            .list_runs()
            .map(|runs| json!({ "runs": runs }))
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
        Operator::new(&c.root).resume(&id)
    })
    .await
}
