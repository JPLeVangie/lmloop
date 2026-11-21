//! Black-box tests: the real `lmloop` binary, real fixture processes, and
//! competing OS processes, all in per-test scratch directories.

use serde_json::Value;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let base = std::env::var_os("LMLOOP_TEST_TMP")
            .map_or_else(|| PathBuf::from("/tmp/opencode"), PathBuf::from);
        let dir = base.join(format!("lmloop-it-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("state")).unwrap();
        Self(dir)
    }
    fn state(&self) -> PathBuf {
        self.0.join("state")
    }
    fn run_dir(&self, id: &str) -> PathBuf {
        self.state().join("v1/runs").join(id)
    }
    fn script(&self, name: &str, body: &str) -> String {
        let p = self.0.join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lmloop"));
        c.env_remove("LMLOOP_RUST_STATE_ROOT")
            .arg("--state-root")
            .arg(self.state())
            .current_dir(&self.0);
        c
    }
    fn repo(&self) -> PathBuf {
        let r = self.0.join("repo");
        fs::create_dir_all(r.join(".git")).unwrap();
        fs::write(r.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        r
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn mkdirs(p: &Path) {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
        .unwrap();
}

fn write_private(p: &Path, body: &str) {
    fs::write(p, body).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o600)).unwrap();
}

fn alive(pid: i32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat.contains(") Z "),
        Err(_) => false,
    }
}

fn wait_for(p: &Path) {
    let start = Instant::now();
    while !p.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{p:?} never appeared"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn code(o: &Output) -> i32 {
    o.status.code().expect("exited normally")
}

fn json_of(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&o.stdout)))
}

fn gate(s: &Scratch, id: &str, extra: &[&str], argv: &[&str]) -> Output {
    s.cmd()
        .args(["diagnostic-gate", id, "--json"])
        .args(extra)
        .arg("--")
        .args(argv)
        .output()
        .unwrap()
}

fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(base).unwrap().display().to_string();
            if p.is_dir() {
                out.push((rel, vec![]));
                walk(base, &p, out);
            } else {
                out.push((rel, fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = vec![];
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn passed_gate_publishes_diagnostic_only_snapshot() {
    let s = Scratch::new("passed");
    let ok = s.script("ok.sh", "echo gate-ran; echo oops >&2; exit 0");
    let out = gate(&s, "r1", &[], &[&ok]);
    assert_eq!(code(&out), 0);
    let snap = json_of(&out);
    assert_eq!(snap["gate"]["outcome"], "passed");
    assert_eq!(snap["verified_completion"], false);
    assert_eq!(snap["gate_evidence_reusable"], false);
    assert_eq!(
        snap["evidence"],
        "diagnostic_only_not_bound_to_git_snapshot"
    );
    let log =
        fs::read_to_string(s.run_dir("r1").join(snap["gate"]["log"].as_str().unwrap())).unwrap();
    assert!(log.contains("gate-ran") && log.contains("oops"));

    let st = s.cmd().args(["status", "r1", "--json"]).output().unwrap();
    assert_eq!(code(&st), 0);
    let v = json_of(&st);
    assert_eq!(v["state"], "ok");
    assert_eq!(v["snapshot"]["gate"]["outcome"], "passed");
    assert_eq!(v["snapshot"]["verified_completion"], false);
    assert_eq!(v["lease"]["state"], "stale"); // owner exited, PID marker remains
}

#[test]
fn absent_gate_is_not_a_pass() {
    let s = Scratch::new("absent");
    let out = gate(&s, "r1", &[], &[]);
    assert_eq!(code(&out), 3);
    assert_eq!(json_of(&out)["gate"]["outcome"], "absent");
}

#[test]
fn missing_failed_and_127_are_distinct_blocking_outcomes() {
    let s = Scratch::new("blocking");
    let missing = gate(&s, "m", &[], &["/nonexistent/gate-binary"]);
    assert_eq!(
        (code(&missing), json_of(&missing)["gate"]["outcome"].clone()),
        (5, "missing".into())
    );

    let one = s.script("one.sh", "echo bad; exit 1");
    let failed = gate(&s, "f", &[], &[&one]);
    let v = json_of(&failed);
    assert_eq!(
        (
            code(&failed),
            &v["gate"]["outcome"],
            &v["gate"]["exit_code"]
        ),
        (4, &"failed".into(), &1.into())
    );

    let e127 = s.script("e127.sh", "exit 127");
    let nf = gate(&s, "n", &[], &[&e127]);
    assert_eq!(
        (code(&nf), json_of(&nf)["gate"]["outcome"].clone()),
        (5, "missing".into())
    );
}

#[test]
fn timeout_tears_down_the_whole_process_group_and_keeps_the_log() {
    let s = Scratch::new("timeout");
    let pidfile = s.0.join("child.pid");
    let slow = s.script(
        "slow.sh",
        &format!(
            "echo started; sleep 300 &\necho $! > {}\nwait",
            pidfile.display()
        ),
    );
    let started = Instant::now();
    let out = gate(&s, "t", &["--timeout-secs", "1"], &[&slow]);
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(code(&out), 6);
    let snap = json_of(&out);
    assert_eq!(snap["gate"]["outcome"], "timed_out");
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists()
            || fs::read_to_string(format!("/proc/{pid}/stat"))
                .unwrap()
                .contains(") Z "),
        "descendant survived"
    );
    let log =
        fs::read_to_string(s.run_dir("t").join(snap["gate"]["log"].as_str().unwrap())).unwrap();
    assert!(log.contains("started"));
}

#[test]
fn output_is_bounded_and_stdin_is_null() {
    let s = Scratch::new("bounded");
    let big = s.script(
        "big.sh",
        "head -c 2000000 /dev/zero | tr '\\0' x; cat >/dev/null; echo stdin-eof >&2",
    );
    let out = gate(&s, "b", &[], &[&big]);
    assert_eq!(code(&out), 0);
    let snap = json_of(&out);
    assert_eq!(snap["gate"]["log_truncated"], true);
    let log = fs::read(s.run_dir("b").join(snap["gate"]["log"].as_str().unwrap())).unwrap();
    assert!(log.len() < 300 * 1024);
    assert!(String::from_utf8_lossy(&log).contains("stdin-eof"));
}

#[test]
fn competing_owner_wins_and_loser_leaves_stop_files_and_snapshot_untouched() {
    let s = Scratch::new("contend");
    let hold = s.script(
        "hold.sh",
        &format!("touch {}/held; sleep 30", s.0.display()),
    );
    // Prime the run dir with a stale STOP/STOP-NOW that the winner will clear.
    let prime = s.script("prime.sh", "exit 0");
    assert_eq!(code(&gate(&s, "r", &[], &[&prime])), 0);
    let dir = s.run_dir("r");
    fs::write(dir.join("STOP"), "x").unwrap();
    fs::write(dir.join("STOP-NOW"), "y").unwrap();

    let mut owner = s
        .cmd()
        .args(["diagnostic-gate", "r", "--timeout-secs", "60", "--"])
        .arg(&hold)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !s.0.join("held").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "owner never reached its gate"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !dir.join("STOP").exists() && !dir.join("STOP-NOW").exists(),
        "owner clears stale signals after claiming"
    );

    // New signals arrive while the owner holds the lease; a competitor must not touch them.
    fs::write(dir.join("STOP"), "new").unwrap();
    fs::write(dir.join("STOP-NOW"), "new").unwrap();
    let before = fs::read(dir.join("status.json")).unwrap();
    let loser = gate(&s, "r", &[], &[&prime]);
    assert_eq!(code(&loser), 7);
    assert_eq!(fs::read_to_string(dir.join("STOP")).unwrap(), "new");
    assert_eq!(fs::read_to_string(dir.join("STOP-NOW")).unwrap(), "new");
    assert_eq!(fs::read(dir.join("status.json")).unwrap(), before);

    let st = s.cmd().args(["status", "r", "--json"]).output().unwrap();
    let v = json_of(&st);
    assert_eq!(v["lease"]["state"], "held");
    assert_eq!(v["lease"]["pid"], owner.id());
    assert_eq!(v["snapshot"]["phase"], "gate_running");

    owner.kill().unwrap();
    owner.wait().unwrap();
    let st = s.cmd().args(["status", "r", "--json"]).output().unwrap();
    assert_eq!(json_of(&st)["lease"]["state"], "stale");
}

#[test]
fn dry_run_has_no_side_effects_and_conflicts_with_detach() {
    let s = Scratch::new("dry");
    let repo = s.repo();
    let before = tree(&s.0);
    let out = s
        .cmd()
        .args(["run", "do the thing", "--dry-run", "--repo"])
        .arg(&repo)
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("do the thing"));
    assert_eq!(tree(&s.0), before);
    assert!(!s.state().join("v1").exists());

    let both = s
        .cmd()
        .args(["run", "x", "--dry-run", "--detach", "--repo"])
        .arg(&repo)
        .output()
        .unwrap();
    assert_eq!(code(&both), 2);
    let blank = s
        .cmd()
        .args(["run", "   ", "--dry-run", "--repo"])
        .arg(&repo)
        .output()
        .unwrap();
    assert_eq!(code(&blank), 2);
    let norepo = s
        .cmd()
        .args(["run", "x", "--dry-run", "--repo", "/nonexistent"])
        .output()
        .unwrap();
    assert_eq!(code(&norepo), 2);
    assert_eq!(tree(&s.0), before);
}

#[test]
fn non_dry_run_fails_closed() {
    let s = Scratch::new("launch");
    let repo = s.repo();
    let before = tree(&s.0);
    let out = s
        .cmd()
        .args(["run", "x", "--repo"])
        .arg(&repo)
        .output()
        .unwrap();
    assert_eq!(code(&out), 69);
    assert_eq!(tree(&s.0), before);
}

#[test]
fn status_distinguishes_no_run_invalid_and_unsupported_and_never_writes() {
    let s = Scratch::new("status");
    let none = s.cmd().args(["status", "--json"]).output().unwrap();
    assert_eq!(
        (code(&none), json_of(&none)["state"].clone()),
        (3, "no_run".into())
    );
    let none = s
        .cmd()
        .args(["status", "ghost", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        (code(&none), json_of(&none)["state"].clone()),
        (3, "no_run".into())
    );
    assert!(
        !s.state().join("v1").exists(),
        "status must not create state"
    );

    let dir = s.run_dir("r");
    mkdirs(&dir);
    let missing = s.cmd().args(["status", "r", "--json"]).output().unwrap();
    let v = json_of(&missing);
    assert_eq!(
        (
            code(&missing),
            v["state"].clone(),
            v["error"]["kind"].clone()
        ),
        (4, "invalid".into(), "missing_snapshot".into())
    );

    write_private(&dir.join("status.json"), r#"{"schema_version": 99}"#);
    let before = tree(&s.0);
    let unsupported = s.cmd().args(["status", "r", "--json"]).output().unwrap();
    let v = json_of(&unsupported);
    assert_eq!(
        (code(&unsupported), v["error"]["kind"].clone()),
        (4, "unsupported_version".into())
    );

    write_private(&dir.join("status.json"), "not json");
    let junk = s.cmd().args(["status", "r", "--json"]).output().unwrap();
    assert_eq!(
        (code(&junk), json_of(&junk)["error"]["kind"].clone()),
        (4, "malformed".into())
    );

    let bad_id = s
        .cmd()
        .args(["status", "../etc", "--json"])
        .output()
        .unwrap();
    assert_eq!(code(&bad_id), 2);
    assert!(
        !dir.join("owner.lock").exists(),
        "status must not create a lease file"
    );
    assert_eq!(tree(&s.0).len(), before.len());
}

#[test]
fn symlinked_run_status_and_state_paths_are_refused() {
    let s = Scratch::new("symlink");
    let python = s.0.join("python-state");
    fs::create_dir_all(&python).unwrap();
    fs::write(python.join("status.json"), r#"{"schema_version":1}"#).unwrap();
    mkdirs(&s.state().join("v1/runs"));
    symlink(&python, s.run_dir("linked")).unwrap();
    let st = s
        .cmd()
        .args(["status", "linked", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        (code(&st), json_of(&st)["error"]["kind"].clone()),
        (4, "unsafe_path".into())
    );
    let g = gate(&s, "linked", &[], &[]);
    assert_eq!(code(&g), 1);
    assert!(!python.join("owner.lock").exists() && !python.join("status.json.tmp").exists());

    let real = s.run_dir("real");
    mkdirs(&real);
    symlink(python.join("status.json"), real.join("status.json")).unwrap();
    let st = s.cmd().args(["status", "real", "--json"]).output().unwrap();
    assert_eq!(
        (code(&st), json_of(&st)["error"]["kind"].clone()),
        (4, "unsafe_path".into())
    );
}

#[test]
fn help_documents_the_contract() {
    let s = Scratch::new("help");
    let out = s.cmd().arg("--help").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for needle in ["diagnostic-gate", "EXIT CODES", "status", "run"] {
        assert!(text.contains(needle), "{needle}");
    }
    let g = s
        .cmd()
        .args(["diagnostic-gate", "--help"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&g.stdout).contains("--timeout-secs"));
}

fn spawn_holder(s: &Scratch, id: &str) -> (std::process::Child, i32) {
    let pidfile = s.0.join("hold.pid");
    let hold = s.script(
        "hold.sh",
        &format!(
            "sleep 300 &\necho $! > {}\ntouch {}/held\nwait",
            pidfile.display(),
            s.0.display()
        ),
    );
    let owner = s
        .cmd()
        .args(["diagnostic-gate", id, "--timeout-secs", "120", "--"])
        .arg(&hold)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&s.0.join("held"));
    let pid = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    (owner, pid)
}

#[test]
fn concurrent_fresh_creates_never_fail_spuriously() {
    for round in 0..5 {
        let s = Scratch::new(&format!("race{round}"));
        let kids: Vec<_> = (0..8)
            .map(|_| {
                s.cmd()
                    .args(["diagnostic-gate", "r", "--json"])
                    .arg("--")
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for k in kids {
            let o = k.wait_with_output().unwrap();
            assert!(
                matches!(code(&o), 3 | 7),
                "exit {}: {}",
                code(&o),
                String::from_utf8_lossy(&o.stderr)
            );
        }
    }
}

#[test]
fn unsupported_or_malformed_snapshot_is_preserved_and_nothing_runs() {
    let s = Scratch::new("preserve");
    let dir = s.run_dir("r");
    mkdirs(&dir);
    let marker = s.0.join("ran");
    let ran = s.script("ran.sh", &format!("touch {}", marker.display()));
    for body in [r#"{"schema_version": 99, "future": true}"#, "not json"] {
        write_private(&dir.join("status.json"), body);
        fs::write(dir.join("STOP"), "keep").unwrap();
        let out = gate(&s, "r", &[], &[&ran]);
        assert_eq!(code(&out), 1);
        assert_eq!(fs::read_to_string(dir.join("status.json")).unwrap(), body);
        assert_eq!(fs::read_to_string(dir.join("STOP")).unwrap(), "keep");
        assert!(!marker.exists() && !dir.join("logs").exists());
    }
}

#[test]
fn sigterm_reaps_the_gate_records_interrupted_and_releases_the_run() {
    let s = Scratch::new("sigterm");
    let (mut owner, child) = spawn_holder(&s, "r");
    assert!(alive(child));
    unsafe { libc::kill(owner.id() as i32, libc::SIGTERM) };
    let status = owner.wait().unwrap();
    assert_eq!(status.code(), Some(9));
    std::thread::sleep(Duration::from_millis(100));
    assert!(!alive(child), "gate descendant orphaned");
    let st = s.cmd().args(["status", "r", "--json"]).output().unwrap();
    let v = json_of(&st);
    assert_eq!(v["snapshot"]["phase"], "interrupted");
    assert_eq!(v["snapshot"]["gate"]["outcome"], "interrupted");
    assert_eq!(v["needs_reconciliation"], false);
    let ok = s.script("ok.sh", "exit 0");
    assert_eq!(code(&gate(&s, "r", &[], &[&ok])), 0);
}

#[test]
fn abrupt_owner_loss_is_reported_interrupted_and_refuses_retry() {
    let s = Scratch::new("sigkill");
    let (mut owner, child) = spawn_holder(&s, "r");
    owner.kill().unwrap();
    owner.wait().unwrap();
    unsafe { libc::kill(child, libc::SIGKILL) };
    let dir = s.run_dir("r");
    let st = s.cmd().args(["status", "r", "--json"]).output().unwrap();
    let v = json_of(&st);
    assert_eq!(v["snapshot"]["phase"], "gate_running");
    assert_eq!(v["effective_phase"], "interrupted");
    assert_eq!(v["needs_reconciliation"], true);

    fs::write(dir.join("STOP"), "keep").unwrap();
    let before = fs::read(dir.join("status.json")).unwrap();
    let ok = s.script("ok.sh", "exit 0");
    assert_eq!(code(&gate(&s, "r", &[], &[&ok])), 8);
    assert_eq!(fs::read(dir.join("status.json")).unwrap(), before);
    assert_eq!(fs::read_to_string(dir.join("STOP")).unwrap(), "keep");
}

#[test]
fn consumed_stop_files_are_archived_not_deleted() {
    let s = Scratch::new("archive");
    let dir = s.run_dir("r");
    mkdirs(&dir);
    fs::write(dir.join("STOP"), "precious").unwrap();
    assert_eq!(code(&gate(&s, "r", &[], &[])), 3);
    assert!(!dir.join("STOP").exists());
    let archived: Vec<_> = fs::read_dir(dir.join("stop-archive")).unwrap().collect();
    assert_eq!(archived.len(), 1);
    assert_eq!(
        fs::read_to_string(archived[0].as_ref().unwrap().path()).unwrap(),
        "precious"
    );
}

#[test]
fn unsafe_log_directory_is_refused_before_the_gate_runs() {
    let s = Scratch::new("unsafelog");
    let dir = s.run_dir("r");
    mkdirs(&dir);
    let elsewhere = s.0.join("elsewhere");
    mkdirs(&elsewhere);
    symlink(&elsewhere, dir.join("logs")).unwrap();
    let marker = s.0.join("ran");
    let ran = s.script("ran.sh", &format!("touch {}", marker.display()));
    assert_eq!(code(&gate(&s, "r", &[], &[&ran])), 1);
    assert!(!marker.exists());
    assert!(!dir.join("status.json").exists());
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[test]
fn log_write_failure_publishes_the_finished_result_and_a_clear_error() {
    let s = Scratch::new("logfail");
    let big = s.script("big.sh", "head -c 20000 /dev/zero | tr '\\0' x");
    // RLIMIT_FSIZE with SIGXFSZ ignored makes the large log write fail with EFBIG.
    let out = Command::new("sh")
        .arg("-c")
        .arg("trap '' XFSZ; ulimit -f 4; exec \"$0\" \"$@\"")
        .arg(env!("CARGO_BIN_EXE_lmloop"))
        .arg("--state-root")
        .arg(s.state())
        .args(["diagnostic-gate", "r", "--"])
        .arg(&big)
        .output()
        .unwrap();
    assert_eq!(code(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("logs/gate-"));
    let snap = s.run_dir("r").join("status.json");
    let v: Value = serde_json::from_str(&fs::read_to_string(snap).unwrap()).unwrap();
    assert_eq!(v["phase"], "gate_finished");
    assert_eq!(v["gate"]["outcome"], "passed");
    assert!(
        v["gate"]["detail"]
            .as_str()
            .unwrap()
            .contains("log write failed")
    );
}

#[test]
fn state_root_overlapping_python_state_or_a_checkout_is_refused() {
    let s = Scratch::new("overlap");
    let home = s.0.join("home");
    mkdirs(&home.join(".local/state"));
    let run = |root: &Path| {
        s.cmd()
            .env("HOME", &home)
            .args(["status", "--json"])
            .arg("--state-root")
            .arg(root)
            .output()
            .unwrap()
    };
    for root in [
        home.join(".local/state/lmloop"),
        home.join(".local/state/lmloop/nested"),
        home.join(".local/state"),
    ] {
        assert_eq!(code(&run(&root)), 1, "{root:?}");
    }
    let checkout = s.0.join("checkout");
    mkdirs(&checkout);
    fs::write(checkout.join("lmloop.py"), "").unwrap();
    fs::write(checkout.join("rundir.py"), "").unwrap();
    assert_eq!(code(&run(&checkout.join("state"))), 1);
    assert_eq!(code(&run(&s.0.join("x/.lmloop/state"))), 1);
    // symlink into Python state is resolved and refused
    mkdirs(&home.join(".local/state/lmloop"));
    symlink(home.join(".local/state/lmloop"), s.0.join("alias")).unwrap();
    assert_eq!(code(&run(&s.0.join("alias"))), 1);
    // '..' is not normal
    assert_eq!(code(&run(&s.0.join("a/../state"))), 1);
    assert_eq!(
        fs::read_dir(home.join(".local/state/lmloop"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(code(&run(&s.state())), 3);
}

#[test]
fn state_files_must_be_private_regular_files() {
    let s = Scratch::new("private");
    let dir = s.run_dir("r");
    mkdirs(&dir);
    fs::write(dir.join("status.json"), "{}").unwrap();
    fs::set_permissions(dir.join("status.json"), fs::Permissions::from_mode(0o644)).unwrap();
    let st = s.cmd().args(["status", "r", "--json"]).output().unwrap();
    assert_eq!(
        (code(&st), json_of(&st)["error"]["kind"].clone()),
        (4, "unsafe_path".into())
    );
    fs::remove_file(dir.join("status.json")).unwrap();
    let fifo = std::ffi::CString::new(dir.join("status.json").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let st = s.cmd().args(["status", "r", "--json"]).output().unwrap();
    assert_eq!(
        (code(&st), json_of(&st)["error"]["kind"].clone()),
        (4, "unsafe_path".into())
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let st = s.cmd().args(["status", "r", "--json"]).output().unwrap();
    assert_eq!(code(&st), 4);
}

#[test]
fn pipe_held_by_an_escaped_descendant_keeps_partial_output_and_flags_it() {
    let s = Scratch::new("escaped");
    let sh = s.script("esc.sh", "echo before-escape; setsid sleep 6 &\nexit 0");
    let started = Instant::now();
    let out = gate(&s, "r", &[], &[&sh]);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(code(&out), 0);
    let snap = json_of(&out);
    assert_eq!(snap["gate"]["output_incomplete"], true);
    let log =
        fs::read_to_string(s.run_dir("r").join(snap["gate"]["log"].as_str().unwrap())).unwrap();
    assert!(log.contains("before-escape") && log.contains("output incomplete"));
}

#[test]
fn gate_gets_allowlisted_env_only_and_argv_is_not_serialized() {
    let s = Scratch::new("env");
    let dump = s.0.join("env.txt");
    let sh = s.script("env.sh", &format!("env > {}", dump.display()));
    let out = s
        .cmd()
        .env("LMLOOP_SECRET_TOKEN", "hunter2")
        .args(["diagnostic-gate", "r", "--json", "--"])
        .arg(&sh)
        .arg("--token=supersecret")
        .output()
        .unwrap();
    assert_eq!(code(&out), 0);
    assert!(!fs::read_to_string(&dump).unwrap().contains("hunter2"));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(!text.contains("supersecret"));
    let snap = json_of(&out);
    assert_eq!(snap["gate"]["argv_len"], 2);
    let disk = fs::read_to_string(s.run_dir("r").join("status.json")).unwrap();
    let log =
        fs::read_to_string(s.run_dir("r").join(snap["gate"]["log"].as_str().unwrap())).unwrap();
    assert!(!disk.contains("supersecret") && !log.contains("supersecret"));
}

#[test]
fn invalid_cwd_is_a_usage_error_and_dry_run_sanitises_the_objective() {
    let s = Scratch::new("cwd");
    let out = gate(&s, "r", &["--cwd", "/nonexistent/dir"], &["/bin/true"]);
    assert_eq!(code(&out), 2);
    assert!(!s.state().join("v1").exists());
    let repo = s.repo();
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let out = s
        .cmd()
        .args(["run", "line\u{1b}[31mred\nnext", "--dry-run", "--repo"])
        .arg(&repo)
        .output()
        .unwrap();
    assert_eq!(code(&out), 0);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains('\u{1b}') && text.contains("not proof"));
}

#[test]
fn bare_lmloop_without_a_terminal_prints_help_and_starts_nothing() {
    let s = Scratch::new("bare");
    let out = s.cmd().stdin(Stdio::null()).output().unwrap();
    assert_eq!(code(&out), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("Usage: lmloop"));
    assert!(out.stdout.is_empty());
    // A piped stdout is not a terminal either, and no state is created.
    assert_eq!(
        code(
            &s.cmd()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .output()
                .unwrap()
        ),
        2
    );
    assert!(!s.state().join("v1").exists());
}
