//! Web adapter: the real `lmloop web` binary on an ephemeral loopback port, a raw
//! TCP client, and disposable fixture runs. No browser, no model, no network.

use lmloop::operator::Operator;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let base = std::env::var_os("LMLOOP_TEST_TMP")
            .map_or_else(|| PathBuf::from("/tmp/opencode"), PathBuf::from);
        let dir = base.join(format!("lmloop-web-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        fs::create_dir_all(dir.join("state")).unwrap();
        let s = Self(dir);
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "t"],
            &["config", "user.email", "t@example.com"],
        ] {
            s.git(args);
        }
        fs::write(s.0.join("repo/README"), "base\n").unwrap();
        s.git(&["add", "-A"]);
        s.git(&["commit", "-q", "-m", "base"]);
        s
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(self.0.join("repo"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn state(&self) -> PathBuf {
        self.0.join("state")
    }

    /// Two tasks; the first drops PAUSE, so the run ends `paused` after task one.
    /// `second` is the second task's worker prompt.
    fn start_paused(&self, id: &str, second: &str) {
        let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let pause = self.state().join(format!("v1/runs/{id}/PAUSE"));
        let manifest = json!({
            "manifest_version": 1,
            "repo": self.0.join("repo"),
            "worker": { "argv": [fx.join("fake-opencode")], "timeout_secs": 30 },
            "gate": { "argv": [fx.join("gate-no-fail")], "timeout_secs": 30 },
            "tasks": [
                { "id": "t1", "prompt": format!("ctl|src/one.txt|{}", pause.display()), "scopes": ["src/one.txt"] },
                { "id": "t2", "prompt": second, "scopes": ["docs/two.txt"] },
            ],
        });
        let path = self.0.join(format!("manifest-{id}.json"));
        fs::write(&path, manifest.to_string()).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_lmloop"))
            .arg("--state-root")
            .arg(self.state())
            .args(["fixture-run", id, "--manifest"])
            .arg(&path)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(12), "{out:?}");
    }

    fn serve(&self) -> Server {
        self.serve_args(&[])
    }

    fn auth_file(&self) -> PathBuf {
        let file = self.0.join("auth");
        use std::os::unix::fs::PermissionsExt;
        fs::write(&file, "test-user:correct-horse-battery\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        file
    }

    fn serve_auth(&self) -> Server {
        let file = self.auth_file();
        self.serve_args(&[
            "--auth-file",
            file.to_str().unwrap(),
            "--public-origin",
            "https://loop.example",
        ])
    }

    fn serve_args(&self, extra: &[&str]) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lmloop"))
            .arg("--state-root")
            .arg(self.state())
            .args(["web", "--port", "0"])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line
            .trim()
            .strip_prefix("listening on http://")
            .and_then(|a| a.strip_suffix('/'))
            .unwrap_or_else(|| panic!("no address in {line:?}"))
            .to_string();
        Server { child, addr }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    addr: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
    fn kind(&self) -> String {
        self.json()["error"]["kind"].as_str().unwrap_or("").into()
    }
}

/// One HTTP/1.1 exchange with exactly the headers given.
fn raw(addr: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    for (k, v) in headers {
        req += &format!("{k}: {v}\r\n");
    }
    req += &format!("Content-Length: {}\r\n\r\n", body.len());
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {text:?}"));
    let headers = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Reply {
        status,
        headers,
        body: body.to_string(),
    }
}

/// The credentials `Scratch::auth_file` writes; a server started without `--auth-file` ignores them.
const BASIC: &str = "Basic dGVzdC11c2VyOmNvcnJlY3QtaG9yc2UtYmF0dGVyeQ==";

fn get(addr: &str, path: &str) -> Reply {
    raw(
        addr,
        "GET",
        path,
        &[("Host", addr), ("Authorization", BASIC)],
        b"",
    )
}

fn post(addr: &str, path: &str, body: Value) -> Reply {
    let origin = format!("http://{addr}");
    raw(
        addr,
        "POST",
        path,
        &[
            ("Host", addr),
            ("Origin", &origin),
            ("Authorization", BASIC),
            ("Content-Type", "application/json"),
        ],
        body.to_string().as_bytes(),
    )
}

#[test]
fn reads_actions_decisions_and_headers_on_a_fixture() {
    let s = Scratch::new("main");
    s.start_paused("r1", "ok|docs/two.txt|second");
    let srv = s.serve();
    let a = srv.addr.as_str();
    assert!(a.starts_with("127.0.0.1:") && !a.ends_with(":0"), "{a}");

    // Reads, and the same answer the Operator gives directly.
    let list = get(a, "/api/runs");
    assert_eq!(list.status, 200);
    assert_eq!(list.json()["runs"][0]["run_id"], "r1");
    assert_eq!(list.json()["runs"][0]["phase"], "paused");
    let view = get(a, "/api/runs/r1");
    assert_eq!(view.status, 200);
    let op = Operator::new(&s.state());
    assert_eq!(
        view.json(),
        serde_json::to_value(op.view("r1").unwrap()).unwrap()
    );
    let v = view.json();
    assert_eq!(v["actions"]["resume"]["available"], true);
    assert_eq!(v["actions"]["approve"]["available"], false);
    assert_eq!(v["evidence"], "fixture_runner_not_production_evidence");
    let gate_log = v["tasks"][0]["gate_log"].as_str().unwrap().to_string();
    let log = get(a, &format!("/api/runs/r1/{gate_log}"));
    assert_eq!(log.status, 200); // a task's `gate_log` (`logs/<name>`) is directly a URL tail
    let name = gate_log.strip_prefix("logs/").unwrap();
    let log = get(a, &format!("/api/runs/r1/logs/{name}?bytes=10"));
    assert_eq!(log.status, 200);
    assert!(log.json()["truncated"] == true && log.json()["text"].as_str().unwrap().len() <= 10);
    let big = get(a, &format!("/api/runs/r1/logs/{name}?bytes=999999999"));
    assert!(big.json()["text"].as_str().unwrap().len() <= 64 * 1024);

    // Hardening headers on success and on error; no CORS anywhere.
    for r in [&list, &log, &get(a, "/api/runs/zzz"), &get(a, "/")] {
        assert_eq!(r.header("cache-control"), Some("no-store"));
        assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
        assert!(
            r.header("content-security-policy")
                .unwrap()
                .contains("default-src 'none'")
        );
        assert!(r.header("access-control-allow-origin").is_none());
    }
    assert_eq!(get(a, "/").status, 200);
    assert_eq!(get(a, "/app.js").status, 200);
    assert_eq!(get(a, "/api/runs/zzz").status, 404);
    assert_eq!(get(a, "/api/runs/zzz").kind(), "no_run");
    assert_eq!(get(a, "/api/runs/r1/logs/..%2Fstate.json").status, 400);
    assert_eq!(get(a, "/api/runs/r1/logs/nope.log").status, 404);
    assert_eq!(get(a, "/api/runs/r1/logs/x.log?bytes=abc").status, 400);
    assert_eq!(get(a, "/nope").status, 404);
    let pre = raw(
        a,
        "OPTIONS",
        "/api/runs/r1/control",
        &[
            ("Host", a),
            ("Origin", "http://evil.example"),
            ("Access-Control-Request-Method", "POST"),
        ],
        b"",
    );
    assert!(pre.status == 405 && pre.header("access-control-allow-origin").is_none());

    // Refused by the Operator's own availability: 409 with its reason.
    let r = post(a, "/api/runs/r1/control", json!({"control": "stop"}));
    assert_eq!((r.status, r.kind().as_str()), (409, "unavailable"));
    let r = post(a, "/api/runs/r1/decision", json!({"decision": "approved"}));
    assert_eq!((r.status, r.kind().as_str()), (409, "unavailable"));
    let r = post(
        a,
        "/api/runs/r1/decision",
        json!({"decision": "changes_requested"}),
    );
    assert_eq!((r.status, r.kind().as_str()), (400, "note_required"));
    assert_eq!(post(a, "/api/runs/zzz/resume", json!({})).status, 404);

    // Resume through the shared engine path; empty body is accepted as `{}`.
    let r = raw(
        a,
        "POST",
        "/api/runs/r1/resume",
        &[
            ("Host", a),
            ("Origin", &format!("http://{a}")),
            ("Content-Type", "application/json; charset=utf-8"),
        ],
        b"",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["outcome"], "complete");
    assert_eq!(get(a, "/api/runs/r1").json()["complete"], true);
    assert_eq!(post(a, "/api/runs/r1/resume", json!({})).status, 409);

    // A decision records one file and never moves Git or run state.
    let head = s.git(&["rev-parse", "HEAD"]);
    let run_json = fs::read_to_string(s.state().join("v1/runs/r1/run.json")).unwrap();
    let r = post(
        a,
        "/api/runs/r1/decision",
        json!({"decision": "approved", "note": "ship\u{1b}[31m it"}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["action"], "approve");
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head, "decision moved Git");
    assert_eq!(s.git(&["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(s.state().join("v1/runs/r1/run.json")).unwrap(),
        run_json
    );
    let d = get(a, "/api/runs/r1").json()["decision"].clone();
    assert_eq!(d["note"], "ship\u{fffd}[31m it");
    assert_eq!(d["effect"], "recorded_only_no_git_change");
    let r = post(
        a,
        "/api/runs/r1/decision",
        json!({"decision": "changes_requested", "note": "x"}),
    );
    assert_eq!(r.status, 409);
}

#[test]
fn long_resume_does_not_block_reads_or_stop() {
    let s = Scratch::new("slow");
    s.start_paused("r1", "slow|docs/two.txt|x");
    let srv = s.serve();
    let a = srv.addr.clone();
    let head = s.git(&["rev-parse", "HEAD"]);

    let addr = a.clone();
    let resume = std::thread::spawn(move || post(&addr, "/api/runs/r1/resume", json!({})));
    let t = Instant::now();
    loop {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "worker never started"
        );
        if get(&a, "/api/runs/r1").json()["actions"]["stop_now"]["available"] == true {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // The resume is still running: reads answer, a second resume is refused.
    let t = Instant::now();
    assert_eq!(get(&a, "/api/runs").status, 200);
    assert!(t.elapsed() < Duration::from_secs(5));
    let again = post(&a, "/api/runs/r1/resume", json!({}));
    assert_eq!(again.status, 409);

    // Shared controls equivalence: the web view is the Operator's view, before and after.
    let op = Operator::new(&s.state());
    assert_eq!(
        get(&a, "/api/runs/r1").json()["actions"],
        serde_json::to_value(op.view("r1").unwrap().actions).unwrap()
    );
    let r = post(&a, "/api/runs/r1/control", json!({"control": "stop_now"}));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["action"], "STOP-NOW");
    let done = resume.join().unwrap();
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(done.json()["outcome"], "stopped");
    let v = get(&a, "/api/runs/r1").json();
    assert_eq!(v["phase"], "stopped");
    assert_eq!(v, serde_json::to_value(op.view("r1").unwrap()).unwrap());
    assert_eq!(
        s.git(&["rev-parse", "HEAD"]),
        head,
        "stop committed something"
    );

    // The flag is released: a stopped run's resume is now the engine's refusal, not "busy".
    assert_eq!(post(&a, "/api/runs/r1/resume", json!({})).status, 409);
    let r = post(
        &a,
        "/api/runs/r1/decision",
        json!({"decision": "changes_requested", "note": "redo t2"}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
}

#[test]
fn denies_foreign_hosts_origins_types_and_oversize_bodies() {
    let s = Scratch::new("deny");
    s.start_paused("r1", "ok|docs/two.txt|second");
    let srv = s.serve();
    let a = srv.addr.as_str();
    let origin = format!("http://{a}");
    let path = "/api/runs/r1/resume";
    let ok = [
        ("Host", a),
        ("Origin", origin.as_str()),
        ("Content-Type", "application/json"),
    ];
    let with = |name: &str, value: &str| -> Vec<(String, String)> {
        let mut h: Vec<_> = ok
            .iter()
            .filter(|(k, _)| *k != name)
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        h.push((name.into(), value.into()));
        h
    };
    let send = |h: Vec<(String, String)>, body: &[u8]| {
        let h: Vec<(&str, &str)> = h.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        raw(a, "POST", path, &h, body)
    };
    let paused = || get(a, "/api/runs/r1").json()["phase"] == "paused";

    // Host: wrong, localhost alias, other port, absent, repeated, forwarded override.
    for host in [
        "evil.example",
        "localhost",
        &format!("127.0.0.1:{}", 1),
        "127.0.0.1",
    ] {
        assert_eq!(
            raw(a, "GET", "/api/runs", &[("Host", host)], b"").status,
            403,
            "{host}"
        );
        assert_eq!(send(with("Host", host), b"{}").status, 403, "{host}");
    }
    assert_eq!(raw(a, "GET", "/api/runs", &[], b"").status, 403); // no Host
    assert_eq!(
        raw(a, "GET", "/api/runs", &[("Host", a), ("Host", a)], b"").status,
        403
    );
    let spoof = raw(
        a,
        "GET",
        "/api/runs",
        &[
            ("Host", "evil.example"),
            ("X-Forwarded-Host", a),
            ("Forwarded", &format!("host={a}")),
        ],
        b"",
    );
    assert_eq!(
        spoof.status, 403,
        "forwarding headers must not satisfy Host"
    );
    let benign = raw(
        a,
        "GET",
        "/api/runs",
        &[("Host", a), ("X-Forwarded-Host", "evil.example")],
        b"",
    );
    assert_eq!(benign.status, 200);

    // Origin: absent, null, foreign, scheme/port/case variants; Sec-Fetch-Site not same-origin.
    let mut none = with("Origin", "x");
    none.retain(|(k, _)| k != "Origin");
    assert_eq!(send(none, b"{}").kind(), "bad_origin");
    for o in [
        "null",
        "http://evil.example",
        &format!("https://{a}"),
        &format!("http://{a}/"),
        "http://127.0.0.1:1",
        &origin.to_uppercase(),
    ] {
        let r = send(with("Origin", o), b"{}");
        assert_eq!((r.status, r.kind().as_str()), (403, "bad_origin"), "{o}");
    }
    for site in ["cross-site", "same-site", "none"] {
        let mut h = with("Sec-Fetch-Site", site);
        h.sort();
        assert_eq!(send(h, b"{}").status, 403, "{site}");
    }
    // Content type: form posts and text/plain (the CSRF-simple types) are refused.
    for ct in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
        "application/jsonx",
    ] {
        assert_eq!(send(with("Content-Type", ct), b"{}").status, 415, "{ct}");
    }
    let mut no_ct = with("Content-Type", "x");
    no_ct.retain(|(k, _)| k != "Content-Type");
    assert_eq!(send(no_ct, b"{}").status, 415);

    // Bodies: oversize and unknown/malformed shapes are refused before any action.
    assert_eq!(
        send(with("Origin", &origin), &vec![b' '; 64 * 1024]).status,
        413
    );
    assert_eq!(
        send(with("Origin", &origin), br#"{"argv":["touch","/tmp/x"]}"#).status,
        400
    );
    assert_eq!(send(with("Origin", &origin), b"[]").status, 400);
    assert_eq!(send(with("Origin", &origin), b"{").status, 400);
    for (p, body) in [
        ("/api/runs/r1/control", r#"{"control":"rm"}"#),
        ("/api/runs/r1/control", r#"{"control":"stop","path":"/"}"#),
        ("/api/runs/r1/decision", r#"{"decision":"merged"}"#),
        (
            "/api/runs/r1/decision",
            r#"{"decision":"approved","note":"x","push":true}"#,
        ),
    ] {
        let h = with("Origin", &origin);
        let h: Vec<(&str, &str)> = h.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            raw(a, "POST", p, &h, body.as_bytes()).status,
            400,
            "{p} {body}"
        );
    }
    // No endpoint takes a path, argv or import, and GET cannot act.
    for p in [
        "/api/runs/r1/resume",
        "/api/runs/r1/control",
        "/api/runs/r1/decision",
    ] {
        assert_eq!(raw(a, "GET", p, &[("Host", a)], b"").status, 405, "{p}");
    }
    assert_eq!(post(a, "/api/import", json!({})).status, 404);
    // Without --launch-registry a POST to /api/runs launches nothing, whatever it carries.
    let r = post(a, "/api/runs", json!({"argv": ["sh"]}));
    assert_eq!((r.status, r.kind().as_str()), (404, "launch_disabled"));
    let long = "n".repeat(3000);
    let r = post(
        a,
        "/api/runs/r1/decision",
        json!({"decision": "changes_requested", "note": long}),
    );
    assert_eq!((r.status, r.kind().as_str()), (400, "usage"));

    assert!(paused(), "a denied request changed the run");
    for f in ["PAUSE", "STOP", "STOP-NOW", "decision.json"] {
        // PAUSE was archived by the engine when the fixture run paused; the rest never existed.
        assert!(
            !s.state().join("v1/runs/r1").join(f).exists() || f == "PAUSE",
            "{f}"
        );
    }
}

#[test]
fn optional_auth_and_public_origin_gate_every_route_and_mutation() {
    let s = Scratch::new("auth");
    s.start_paused("r1", "ok|docs/two.txt|second");
    let srv = s.serve_auth();
    let a = srv.addr.as_str();
    let unauth = raw(a, "GET", "/", &[("Host", a)], b"");
    assert_eq!(unauth.status, 401);
    assert_eq!(
        unauth.header("www-authenticate"),
        Some("Basic realm=\"lmloop\", charset=\"UTF-8\"")
    );
    assert_eq!(unauth.header("cache-control"), Some("no-store"));
    let auth = "Basic dGVzdC11c2VyOmNvcnJlY3QtaG9yc2UtYmF0dGVyeQ==";
    let headers = [
        ("Host", "loop.example"),
        ("Origin", "https://loop.example"),
        ("Authorization", auth),
    ];
    assert_eq!(raw(a, "GET", "/api/runs", &headers, b"").status, 200);
    assert_eq!(
        raw(
            a,
            "GET",
            "/api/runs",
            &[
                ("Host", "loop.example"),
                ("Origin", "https://loop.example"),
                ("Authorization", "Basic dGVzdC11c2VyOmJhZA==")
            ],
            b""
        )
        .status,
        401
    );
    assert_eq!(
        raw(
            a,
            "POST",
            "/api/runs/r1/decision",
            &[
                ("Host", "loop.example"),
                ("Origin", "https://evil.example"),
                ("Authorization", auth),
                ("Content-Type", "application/json")
            ],
            br#"{"decision":"approved"}"#
        )
        .status,
        403
    );
    // Top-level browser GET: exact public Host, no Origin. Challenge, then serve.
    let nav = raw(a, "GET", "/", &[("Host", "loop.example")], b"");
    assert_eq!(nav.status, 401);
    assert_eq!(
        nav.header("www-authenticate"),
        Some("Basic realm=\"lmloop\", charset=\"UTF-8\"")
    );
    let nav_auth = [("Host", "loop.example"), ("Authorization", auth)];
    assert_eq!(raw(a, "GET", "/api/runs", &nav_auth, b"").status, 200);
    // Absent Origin is for GET only; wrong/other Host or any present wrong Origin still denied.
    for (host, origin) in [
        ("evil.example", None),
        ("loop.example", Some("https://evil.example")),
        ("loop.example", Some("null")),
        ("loop.example", Some("")),
    ] {
        let mut h = vec![("Host", host), ("Authorization", auth)];
        h.extend(origin.map(|o| ("Origin", o)));
        assert_eq!(
            raw(a, "GET", "/api/runs", &h, b"").status,
            403,
            "{host} {origin:?}"
        );
    }
    let dup = [
        ("Host", "loop.example"),
        ("Origin", "https://loop.example"),
        ("Origin", "https://loop.example"),
        ("Authorization", auth),
    ];
    assert_eq!(raw(a, "GET", "/api/runs", &dup, b"").status, 403);
    let no_origin_post = raw(
        a,
        "POST",
        "/api/runs/r1/resume",
        &[
            ("Host", "loop.example"),
            ("Authorization", auth),
            ("Content-Type", "application/json"),
        ],
        b"{}",
    );
    assert_eq!(no_origin_post.status, 403);
    let ok = raw(
        a,
        "POST",
        "/api/runs/r1/resume",
        &[
            ("Host", "loop.example"),
            ("Origin", "https://loop.example"),
            ("Authorization", auth),
            ("Content-Type", "application/json"),
        ],
        b"{}",
    );
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert_eq!(ok.json()["outcome"], "complete");
}

#[test]
fn invalid_auth_startup_configurations_are_rejected() {
    let s = Scratch::new("auth-invalid");
    let file = s.0.join("bad-auth");
    fs::write(&file, "user:pass").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    for origin in [
        "https://loop.example:",
        "https://loop.example:abc",
        "https://loop.example:65536",
        "https://loop.example:999999999999999999999",
        "https://user@loop.example",
        "https://loop.example/path",
        "https://bad host.example",
        "https://-bad.example",
        "https://bad..example",
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_lmloop"))
            .arg("--state-root")
            .arg(s.state())
            .args([
                "web",
                "--auth-file",
                file.to_str().unwrap(),
                "--public-origin",
                origin,
            ])
            .output()
            .unwrap();
        assert!(!out.status.success(), "accepted {origin}");
    }
    for args in [
        vec!["web", "--auth-file", file.to_str().unwrap()],
        vec!["web", "--public-origin", "https://loop.example"],
        vec![
            "web",
            "--auth-file",
            file.to_str().unwrap(),
            "--public-origin",
            "http://loop.example",
        ],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_lmloop"))
            .arg("--state-root")
            .arg(s.state())
            .args(&args)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{args:?}");
    }
}

#[test]
fn web_has_no_remote_bind_option() {
    let s = Scratch::new("flags");
    for args in [
        &["web", "--host", "0.0.0.0"][..],
        &["web", "--bind", "0.0.0.0:0"],
        &["web", "--port", "70000"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_lmloop"))
            .arg("--state-root")
            .arg(s.state())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
    }
}

/// Shapes `android/.../net/{LmloopApiClient,RunModels}.kt` decode: `HealthResponse`
/// (status/auth/oidc/read_only all required), `ServerInfo`, `RunsResponse`, `RunSummary`.
fn assert_android_summary(r: &Value, id: &str, state: &str) {
    for (k, want) in [
        ("run_id", id),
        ("route_id", id),
        ("project", "lmloop"),
        ("title", id),
        ("state", state),
    ] {
        assert_eq!(r[k], want, "{k} in {r}");
    }
    assert_eq!(r["paused"], state == "paused");
    assert_eq!(r["stopping"], false);
    // Original Rust fields stay; nothing Android would read as progress is invented.
    for k in [
        "kind",
        "phase",
        "lease",
        "owner_lost",
        "tasks_done",
        "tasks_total",
        "failure",
        "pending_controls",
        "problem",
    ] {
        assert!(r.get(k).is_some(), "lost {k}");
    }
    for k in [
        "iteration",
        "max_iterations",
        "plan_done",
        "plan_total",
        "model",
        "agent",
        "commits",
    ] {
        assert!(r.get(k).is_none(), "fabricated {k}");
    }
}

#[test]
fn android_read_contract_additive_and_guarded() {
    let s = Scratch::new("android");
    s.start_paused("r1", "ok|docs/two.txt|second");
    let srv = s.serve();
    let a = srv.addr.as_str();

    let h = get(a, "/health").json();
    assert_eq!(
        h,
        json!({"status": "ok", "auth": "none", "oidc": false, "read_only": false})
    );
    let c = get(a, "/api/config");
    assert_eq!(
        (
            c.status,
            c.json()["poll_seconds"].as_f64(),
            c.json()["read_only"].as_bool()
        ),
        (200, Some(5.0), Some(false))
    );
    let list = get(a, "/api/runs").json();
    assert_android_summary(&list["runs"][0], "r1", "paused");
    let run = get(a, "/api/runs/lmloop/r1");
    assert_eq!(run.status, 200);
    assert_android_summary(&run.json(), "r1", "paused");
    // The detail still carries every original view field, and the old route is unchanged.
    let old = get(a, "/api/runs/r1").json();
    for k in [
        "actions", "tasks", "complete", "evidence", "decision", "logs", "events", "warnings",
    ] {
        assert_eq!(run.json()[k], old[k], "{k}");
    }
    assert_eq!(get(a, "/api/runs/other/r1").status, 404);
    assert_eq!(get(a, "/api/runs/lmloop/zzz").status, 404);
    // Still no mutation and still exact-host: an alias POST is not a route, a foreign Host is denied.
    assert_eq!(post(a, "/api/runs/lmloop/r1/resume", json!({})).status, 404);
    assert_eq!(
        raw(a, "GET", "/health", &[("Host", "evil.example")], b"").status,
        403
    );

    // Behind Basic auth: only /health is open (no credentials), and only for the exact Host.
    let s = Scratch::new("android-auth");
    s.start_paused("r1", "ok|docs/two.txt|second");
    let srv = s.serve_auth();
    let a = srv.addr.as_str();
    let pub_host = [("Host", "loop.example")];
    let h = raw(a, "GET", "/health", &pub_host, b"");
    assert_eq!(h.status, 200);
    assert_eq!(h.json()["auth"], "basic");
    assert_eq!(raw(a, "GET", "/health", &[("Host", a)], b"").status, 200);
    assert_eq!(
        raw(a, "GET", "/health", &[("Host", "evil.example")], b"").status,
        403
    );
    assert_eq!(
        raw(
            a,
            "GET",
            "/health",
            &[("Host", "loop.example"), ("Origin", "https://evil.example")],
            b""
        )
        .status,
        403
    );
    for p in ["/api/config", "/api/runs", "/api/runs/lmloop/r1"] {
        assert_eq!(raw(a, "GET", p, &pub_host, b"").status, 401, "{p}");
    }
    let auth = (
        "Authorization",
        "Basic dGVzdC11c2VyOmNvcnJlY3QtaG9yc2UtYmF0dGVyeQ==",
    );
    for p in ["/api/config", "/api/runs/lmloop/r1"] {
        assert_eq!(
            raw(a, "GET", p, &[pub_host[0], auth], b"").status,
            200,
            "{p}"
        );
    }
    let resume = raw(
        a,
        "POST",
        "/api/runs/r1/resume",
        &[pub_host[0], ("Content-Type", "application/json")],
        b"{}",
    );
    assert_eq!(resume.status, 403);
}

// ---- "New run": opt-in launch registry over operator-owned one-task pilot templates ----

impl Scratch {
    /// A private one-task template (fake worker/gate, scope `src/one.txt`) and a private registry
    /// naming it as project `demo`; returns (registry, template) paths. `mode` is the task prompt.
    fn registry(&self, mode: &str) -> (PathBuf, PathBuf) {
        let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let bin = self.0.join("bin");
        fs::create_dir_all(&bin).unwrap();
        // The launcher name the fixture runner refuses; a pilot template may use it.
        fs::copy(fx.join("fake-opencode"), bin.join("opencode")).unwrap();
        let template = self.0.join("template.json");
        self.private(
            &template,
            &json!({
                "manifest_version": 1,
                "repo": self.0.join("repo"),
                "worker": { "argv": [bin.join("opencode")], "timeout_secs": 30 },
                "gate": { "argv": [fx.join("gate-no-fail")], "timeout_secs": 30 },
                "tasks": [{ "id": "t1", "prompt": mode, "scopes": ["src/one.txt"] }],
            }),
        );
        let registry = self.0.join("registry.json");
        self.private(
            &registry,
            &json!({
                "registry_version": 1,
                "projects": [{ "id": "demo", "label": "Demo project", "manifest": template }],
            }),
        );
        (registry, template)
    }

    fn private(&self, path: &Path, v: &Value) {
        self.private_bytes(path, v.to_string().as_bytes());
    }

    fn private_bytes(&self, path: &Path, body: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// Launching needs authentication, even on loopback.
    fn serve_launch(&self, registry: &Path) -> Server {
        let auth = self.auth_file();
        self.serve_args(&[
            "--auth-file",
            auth.to_str().unwrap(),
            "--launch-registry",
            registry.to_str().unwrap(),
        ])
    }

    /// Rewrite the template through `f` (private, as the registry requires).
    fn edit_template(&self, template: &Path, f: impl FnOnce(&mut Value)) {
        let mut v: Value = serde_json::from_slice(&fs::read(template).unwrap()).unwrap();
        f(&mut v);
        self.private(template, &v);
    }
}

fn demo(a: &str) -> Value {
    get(a, "/api/launch-options").json()["projects"][0].clone()
}

fn wait_phase(a: &str, id: &str, phase: &str) -> Value {
    let t = Instant::now();
    loop {
        let v = get(a, &format!("/api/runs/{id}")).json();
        if v["phase"] == phase {
            return v;
        }
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "{id} never reached {phase}: {v}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn launch_is_off_unless_a_registry_is_given() {
    let s = Scratch::new("launch-off");
    let srv = s.serve();
    let a = srv.addr.as_str();
    assert_eq!(get(a, "/api/launch-options").json()["enabled"], false);
    let r = post(
        a,
        "/api/runs",
        json!({"project": "demo", "objective": "x", "revision": "y"}),
    );
    assert_eq!((r.status, r.kind().as_str()), (404, "launch_disabled"));
    assert!(!s.state().join("v1/runs").exists());
}

#[test]
fn registry_must_be_private_and_needs_an_explicit_state_root() {
    let s = Scratch::new("launch-reg");
    let (registry, _) = s.registry("ok|src/one.txt|x");
    let run = |explicit: bool| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lmloop"));
        c.env_remove("LMLOOP_RUST_STATE_ROOT");
        if explicit {
            c.arg("--state-root").arg(s.state());
        } else {
            c.env("LMLOOP_RUST_STATE_ROOT", s.state());
        }
        c.args(["web", "--port", "0", "--launch-registry"])
            .arg(&registry)
            .output()
            .unwrap()
    };
    assert_eq!(run(false).status.code(), Some(2));
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o644)).unwrap();
    let out = run(true);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("launch registry"));

    // A valid registry without --auth-file is refused before anything listens.
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o600)).unwrap();
    let out = run(true);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--launch-registry requires --auth-file")
    );
}

#[test]
fn launch_templates_with_unsafe_scope_stale_watchdog_or_no_prompt_room_are_not_eligible() {
    let s = Scratch::new("launch-preflight");
    let (registry, template) = s.registry("ok|src/one.txt|x");
    let srv = s.serve_launch(&registry);
    let a = srv.addr.as_str();
    let rev = demo(a)["revision"].as_str().unwrap().to_string();

    // A literal scope that git would refuse is unusable, not launchable.
    s.edit_template(&template, |v| v["tasks"][0]["scopes"] = json!(["../x"]));
    let p = demo(a);
    assert_eq!(p["eligible"], false);
    assert_eq!(p["reason"], "the project template is not usable");
    assert!(p.get("scopes").is_none());

    // The worker watchdog's progress file must be a fresh, empty, usable file: absent, a partial
    // `model` prefix (baseline 0, but not empty) and a whole record are all refused on GET and POST.
    let progress = s.0.join("progress");
    s.edit_template(&template, |v| {
        v["tasks"][0]["scopes"] = json!(["src/one.txt"]);
        v["worker"]["watchdog"] =
            json!({"progress_file": progress, "startup_secs": 30, "idle_secs": 30});
    });
    let stale = "the worker progress file is not fresh or usable";
    let launch = |rev: &str| {
        post(
            a,
            "/api/runs",
            json!({"project": "demo", "objective": "x", "revision": rev}),
        )
    };
    for content in [None, Some(&b"mod"[..]), Some(&b"model\n"[..])] {
        match content {
            Some(c) => s.private_bytes(&progress, c),
            None => {
                let _ = fs::remove_file(&progress);
            }
        }
        let p = demo(a);
        assert_eq!(
            (p["eligible"].as_bool(), p["reason"].as_str()),
            (Some(false), Some(stale))
        );
        let r = launch(p["revision"].as_str().unwrap());
        assert_eq!(
            (r.status, r.kind().as_str()),
            (409, "ineligible"),
            "{content:?}"
        );
        assert!(!r.body.contains("lmloop-web-"), "path leaked: {}", r.body);
    }
    s.private_bytes(&progress, b"");
    assert_eq!(demo(a)["eligible"], true);

    // No room left for even a minimal objective: not advertised as launchable.
    s.edit_template(&template, |v| {
        v["tasks"][0]["prompt"] = json!("x".repeat(16 * 1024));
    });
    let p = demo(a);
    assert_eq!(p["eligible"], false);
    assert_eq!(p["reason"], "the project template is not usable");
    assert_ne!(p["revision"], rev.as_str());
}

#[test]
fn options_show_project_scopes_and_budget_but_no_argv_paths_or_prompt() {
    let s = Scratch::new("launch-options");
    let (registry, template) = s.registry("ok|src/one.txt|secret-prompt");
    let srv = s.serve_launch(&registry);
    let a = srv.addr.as_str();
    let r = get(a, "/api/launch-options");
    assert_eq!(r.status, 200);
    for leak in [
        "opencode",
        "gate-no-fail",
        "secret-prompt",
        "argv",
        "template.json",
        "lmloop-web-",
    ] {
        assert!(!r.body.contains(leak), "{leak} leaked: {}", r.body);
    }
    let p = demo(a);
    assert_eq!(
        (p["id"].as_str(), p["eligible"].as_bool()),
        (Some("demo"), Some(true))
    );
    assert_eq!(p["label"], "Demo project");
    assert_eq!(p["scopes"], json!(["src/one.txt"]));
    assert_eq!(p["budget"]["tasks"], 1);
    assert_eq!(p["budget"]["worker_timeout_secs"], 30);
    assert_eq!(p["budget"]["gate_timeout_secs"], 30);
    let rev = p["revision"].as_str().unwrap().to_string();
    assert_eq!(demo(a)["revision"], rev.as_str(), "revision is stable");

    // A changed template is a changed revision; a dirty checkout is shown ineligible.
    let mut v: Value = serde_json::from_slice(&fs::read(&template).unwrap()).unwrap();
    v["gate"]["timeout_secs"] = json!(31);
    s.private(&template, &v);
    let p = demo(a);
    assert_ne!(p["revision"], rev.as_str());
    assert_eq!(p["budget"]["gate_timeout_secs"], 31);
    fs::write(s.0.join("repo/README"), "dirty\n").unwrap();
    let p = demo(a);
    assert_eq!(p["eligible"], false);
    assert!(p["reason"].as_str().unwrap().contains("uncommitted"));

    // Two tasks make the project unusable, not launchable.
    v["tasks"] = json!([
        { "id": "t1", "prompt": "ok|src/one.txt|x", "scopes": ["src/one.txt"] },
        { "id": "t2", "prompt": "ok|src/two.txt|x", "scopes": ["src/two.txt"] },
    ]);
    s.private(&template, &v);
    assert_eq!(demo(a)["eligible"], false);
    assert!(demo(a).get("scopes").is_none());
}

#[test]
fn launch_checks_input_revision_and_eligibility_before_starting_anything() {
    let s = Scratch::new("launch-refuse");
    let (registry, _) = s.registry("ok|src/one.txt|x");
    let srv = s.serve_launch(&registry);
    let a = srv.addr.as_str();
    let rev = demo(a)["revision"].as_str().unwrap().to_string();
    let body = |project: &str, objective: &str, revision: &str| {
        post(
            a,
            "/api/runs",
            json!({"project": project, "objective": objective, "revision": revision}),
        )
    };
    let kind = |r: Reply| (r.status, r.kind());
    assert_eq!(
        kind(body("demo", "do it", "stale")),
        (409, "stale_revision".into())
    );
    assert_eq!(
        kind(body("nope", "do it", &rev)),
        (404, "no_project".into())
    );
    assert_eq!(kind(body("demo", "  ", &rev)), (400, "usage".into()));
    assert_eq!(
        kind(body("demo", &"x".repeat(4 * 1024 + 1), &rev)),
        (400, "usage".into())
    );
    assert_eq!(
        kind(body("demo", "bad\u{1b}[31m", &rev)),
        (400, "usage".into())
    );
    for extra in [
        json!({"argv": ["sh"]}),
        json!({"scope_set": "x"}),
        json!({"run_id": "mine"}),
    ] {
        let mut b = json!({"project": "demo", "objective": "x", "revision": rev});
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert_eq!(post(a, "/api/runs", b).status, 400);
    }
    fs::write(s.0.join("repo/README"), "dirty\n").unwrap();
    assert_eq!(
        kind(body("demo", "do it", &rev)),
        (409, "ineligible".into())
    );
    assert!(
        !s.state().join("v1/runs").exists(),
        "a refused launch created a run"
    );
    // The refusals released the one-at-a-time slot.
    fs::write(s.0.join("repo/README"), "base\n").unwrap();
    assert_eq!(body("demo", "do it", &rev).status, 202);
}

#[test]
fn launch_starts_one_pilot_through_the_engine_and_names_the_created_run() {
    let s = Scratch::new("launch-ok");
    let (registry, _) = s.registry("ok|src/one.txt|from-template");
    let srv = s.serve_launch(&registry);
    let a = srv.addr.as_str();
    let rev = demo(a)["revision"].as_str().unwrap().to_string();
    let head = s.git(&["rev-parse", "HEAD"]);
    let r = post(
        a,
        "/api/runs",
        json!({"project": "demo", "objective": "add the file", "revision": rev}),
    );
    assert_eq!(r.status, 202, "{}", r.body);
    assert_eq!(r.json().as_object().unwrap().len(), 1);
    let id = r.json()["id"].as_str().unwrap().to_string();

    // The id is the created run: a one-shot pilot, committed through the gate.
    let v = wait_phase(a, &id, "complete");
    assert_eq!(v["kind"], "serial_pilot");
    assert_ne!(s.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(s.git(&["log", "-1", "--format=%s"]), "lmloop pilot: t1");
    assert_eq!(
        fs::read_to_string(s.0.join("repo/src/one.txt")).unwrap(),
        "from-template\n"
    );
    let st: Value = serde_json::from_slice(
        &fs::read(s.state().join(format!("v1/runs/{id}/run.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(st["pilot"], true);
    let prompt = st["policy"]["tasks"][0]["prompt"].as_str().unwrap();
    assert!(
        prompt.starts_with("ok|src/one.txt|from-template") && prompt.ends_with("add the file"),
        "{prompt}"
    );
    assert_eq!(st["policy"]["tasks"][0]["scopes"], json!(["src/one.txt"]));

    // One-shot: no resume, and a second launch gets its own fresh id.
    assert_eq!(
        post(a, &format!("/api/runs/{id}/resume"), json!({})).status,
        409
    );
    let r2 = post(
        a,
        "/api/runs",
        json!({"project": "demo", "objective": "again", "revision": rev}),
    );
    assert_eq!(r2.status, 202, "{}", r2.body);
    assert_ne!(r2.json()["id"], id.as_str());
}

#[test]
fn one_launch_at_a_time_and_stop_now_still_ends_it() {
    let s = Scratch::new("launch-busy");
    let (registry, _) = s.registry("slow|src/one.txt|x");
    let srv = s.serve_launch(&registry);
    let a = srv.addr.as_str();
    let rev = demo(a)["revision"].as_str().unwrap().to_string();
    let head = s.git(&["rev-parse", "HEAD"]);
    let launch = || {
        post(
            a,
            "/api/runs",
            json!({"project": "demo", "objective": "x", "revision": rev}),
        )
    };
    let r = launch();
    assert_eq!(r.status, 202, "{}", r.body);
    let id = r.json()["id"].as_str().unwrap().to_string();
    let t = Instant::now();
    while get(a, &format!("/api/runs/{id}")).json()["actions"]["stop_now"]["available"] != true {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "worker never started"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let again = launch();
    assert_eq!((again.status, again.kind().as_str()), (409, "busy"));
    assert_eq!(
        post(a, &format!("/api/runs/{id}/resume"), json!({})).status,
        409
    );
    let view = get(a, &format!("/api/runs/{id}")).json();
    assert_eq!(view["actions"]["stop"]["available"], false); // one-shot: only Stop now
    let r = post(
        a,
        &format!("/api/runs/{id}/control"),
        json!({"control": "stop_now"}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    wait_phase(a, &id, "stopped");
    assert_eq!(
        s.git(&["rev-parse", "HEAD"]),
        head,
        "stop committed something"
    );
    // The slot frees when the pilot's own thread ends; the stopped worker left src/one.txt, so
    // the next launch is then refused as ineligible (never "busy" again).
    let t = Instant::now();
    loop {
        let r = launch();
        if r.kind() != "busy" {
            assert_eq!(
                (r.status, r.kind().as_str()),
                (409, "ineligible"),
                "{}",
                r.body
            );
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "slot never freed");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_fast_launch_that_published_its_run_is_always_accepted_with_that_id() {
    // Runs finish within ms, so the handler often sees a finished engine thread: a published
    // run.json (complete, then task_failed for the unchanged file) must still be a 202 + id.
    let s = Scratch::new("launch-fast");
    let (registry, _) = s.registry("ok|src/one.txt|same");
    let srv = s.serve_launch(&registry);
    let a = srv.addr.as_str();
    for n in 0..6 {
        let rev = demo(a)["revision"].as_str().unwrap().to_string();
        let body = json!({"project": "demo", "objective": "x", "revision": rev});
        let t = Instant::now();
        let r = loop {
            let r = post(a, "/api/runs", body.clone());
            if r.kind() != "busy" {
                break r;
            }
            assert!(t.elapsed() < Duration::from_secs(10), "slot never freed");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(r.status, 202, "launch {n}: {}", r.body);
        let id = r.json()["id"].as_str().unwrap().to_string();
        assert_eq!(get(a, &format!("/api/runs/{id}")).status, 200, "launch {n}");
    }
}
