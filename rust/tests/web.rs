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

    fn serve_auth(&self) -> Server {
        let file = self.0.join("auth");
        use std::os::unix::fs::PermissionsExt;
        fs::write(&file, "test-user:correct-horse-battery\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
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

fn get(addr: &str, path: &str) -> Reply {
    raw(addr, "GET", path, &[("Host", addr)], b"")
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
    assert_eq!(post(a, "/api/runs", json!({"argv": ["sh"]})).status, 405);
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
