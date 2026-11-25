//! Serial fixture runner, black box: the real `lmloop` binary, the fixture
//! worker and gate, and disposable real Git repositories.

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let base = std::env::var_os("LMLOOP_TEST_TMP")
            .map_or_else(|| PathBuf::from("/tmp/opencode"), PathBuf::from);
        let dir = base.join(format!("lmloop-serial-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        fs::create_dir_all(dir.join("state")).unwrap();
        let s = Self(dir);
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["config", "user.name", "t"]);
        s.git(&["config", "user.email", "t@example.com"]);
        fs::write(s.repo().join("README"), "base\n").unwrap();
        s.git(&["add", "-A"]);
        s.git(&["commit", "-q", "-m", "base"]);
        s
    }
    fn repo(&self) -> PathBuf {
        self.0.join("repo")
    }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(self.repo())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }
    fn commits(&self) -> usize {
        self.git(&["rev-list", "--count", "HEAD"]).parse().unwrap()
    }
    /// A manifest over `(task id, "MODE|path|content")` pairs; scopes are the path's top directory.
    fn manifest(&self, tasks: &[(&str, &str)], worker_timeout: u64) -> PathBuf {
        self.manifest_gate(tasks, worker_timeout, "gate-no-fail")
    }
    fn manifest_gate(&self, tasks: &[(&str, &str)], worker_timeout: u64, gate: &str) -> PathBuf {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let tasks: Vec<Value> = tasks
            .iter()
            .map(|(id, prompt)| {
                let path = prompt.split('|').nth(1).unwrap();
                json!({ "id": id, "prompt": prompt, "scopes": [path] })
            })
            .collect();
        let m = json!({
            "manifest_version": 1,
            "repo": self.repo(),
            "worker": { "argv": [fixtures.join("fake-opencode")], "timeout_secs": worker_timeout },
            "gate": { "argv": [fixtures.join(gate)], "timeout_secs": 30 },
            "tasks": tasks,
        });
        let p = self.0.join("manifest.json");
        fs::write(&p, m.to_string()).unwrap();
        p
    }
    fn lmloop(&self, args: &[&str], crash: Option<&str>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lmloop"));
        c.env_remove("LMLOOP_RUST_STATE_ROOT")
            .arg("--state-root")
            .arg(self.0.join("state"))
            .args(args)
            .current_dir(&self.0);
        if let Some(at) = crash {
            c.env("LMLOOP_FIXTURE_CRASH", at);
        }
        c.output().unwrap()
    }
    fn run(&self, id: &str, manifest: &Path, crash: Option<&str>) -> Output {
        self.lmloop(
            &["fixture-run", id, "--manifest", manifest.to_str().unwrap()],
            crash,
        )
    }
    fn run_dir(&self, id: &str) -> PathBuf {
        self.0.join(format!("state/v1/runs/{id}"))
    }
    /// Start `fixture-run` without waiting; the child holds the run lease until it ends.
    fn spawn_run(&self, id: &str, manifest: &Path) -> Child {
        Command::new(env!("CARGO_BIN_EXE_lmloop"))
            .env_remove("LMLOOP_RUST_STATE_ROOT")
            .arg("--state-root")
            .arg(self.0.join("state"))
            .args(["fixture-run", id, "--manifest", manifest.to_str().unwrap()])
            .current_dir(&self.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
    /// Operator control file, e.g. `STOP`, dropped into the run directory.
    fn control(&self, id: &str, name: &str) {
        fs::write(self.run_dir(id).join(name), "").unwrap();
    }
    fn status(&self, id: &str) -> Value {
        let out = self.lmloop(&["status", id, "--json"], None);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn events(&self, id: &str) -> Vec<Value> {
        fs::read_to_string(self.0.join(format!("state/v1/runs/{id}/events.jsonl")))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(15);
    while !ready() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn pid_of(file: &Path) -> String {
    wait_until("a pid file", || {
        fs::read_to_string(file).is_ok_and(|t| t.ends_with('\n'))
    });
    fs::read_to_string(file).unwrap().trim().to_string()
}

/// Gone, or a zombie nobody has reaped yet.
fn dead(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| {
        s.rsplit(") ").next().is_some_and(|r| r.starts_with('Z'))
    })
}

fn kinds(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["kind"].as_str().unwrap()).collect()
}

#[test]
fn two_fresh_tasks_commit_exact_head_serially() {
    let s = Scratch::new("two");
    let base = s.head();
    let m = s.manifest(
        &[
            ("t1", "ok|src/one.txt|first"),
            ("t2", "ok|docs/two.txt|second"),
        ],
        30,
    );
    let out = s.run("r1", &m, None);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));

    assert_eq!(s.commits(), 3);
    let (c2, c1) = (s.head(), s.git(&["rev-parse", "HEAD~1"]));
    assert_eq!(s.git(&["rev-parse", "HEAD~2"]), base);
    assert_eq!(s.git(&["diff", "--name-only", &base, &c1]), "src/one.txt");
    assert_eq!(s.git(&["diff", "--name-only", &c1, &c2]), "docs/two.txt");
    assert_eq!(
        s.git(&["status", "--porcelain"]),
        "",
        "worktree and index are clean"
    );

    let st = s.status("r1");
    assert_eq!(st["state"], "ok");
    assert_eq!(st["run"]["phase"], "complete");
    let done = st["run"]["done"].as_array().unwrap();
    assert_eq!(done.len(), 2);
    assert_eq!(done[0]["commit"], c1);
    assert_eq!(done[1]["commit"], c2);
    assert_ne!(
        done[0]["session_id"], done[1]["session_id"],
        "fresh session per task"
    );
    // The resolved policy is persisted verbatim.
    assert_eq!(st["run"]["policy"]["gate"]["timeout_secs"], 30);
    assert_eq!(st["run"]["policy"]["tasks"].as_array().unwrap().len(), 2);

    let ev = s.events("r1");
    assert!(ev.iter().all(|e| e["v"] == 1));
    let seqs: Vec<u64> = ev.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=ev.len() as u64).collect::<Vec<_>>());
    assert_eq!(
        kinds(&ev),
        [
            "run_started",
            "task_started",
            "worker_finished",
            "snapshot_captured",
            "gate_finished",
            "commit_intent",
            "task_committed",
            "task_started",
            "worker_finished",
            "snapshot_captured",
            "gate_finished",
            "commit_intent",
            "task_committed",
            "run_completed",
        ]
    );
    // A finished run cannot be started again under the same id.
    assert_eq!(code(&s.run("r1", &m, None)), 2);
}

#[test]
fn gate_failure_commits_nothing_and_preserves_work() {
    let s = Scratch::new("gatefail");
    let base = s.head();
    let m = s.manifest(
        &[
            ("t1", "ok|src/one.txt|FAIL here"),
            ("t2", "ok|docs/two.txt|never"),
        ],
        30,
    );
    assert_eq!(code(&s.run("r1", &m, None)), 10);
    assert_eq!(s.head(), base);
    assert_eq!(s.commits(), 1);
    assert_eq!(
        fs::read_to_string(s.repo().join("src/one.txt")).unwrap(),
        "FAIL here\n"
    );
    assert!(
        !s.repo().join("docs/two.txt").exists(),
        "task 2 never started"
    );
    let st = s.status("r1");
    assert_eq!(st["run"]["phase"], "failed");
    assert_eq!(st["run"]["failure"]["reason"], "gate_failed");
    assert_eq!(st["run"]["next_task"], 0);
    // A failed run is not resumable without a human.
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 8);
    assert_eq!(s.head(), base);
}

#[test]
fn invalid_worker_or_missing_evidence_commits_nothing() {
    for (mode, why) in [
        ("nostop", "no clean final stop"),
        ("nosession", "sessionID"),
        ("silent", "no events"),
        ("error", "error event"),
        ("exit1", "Failed"),
        ("slow", "TimedOut"),
    ] {
        let s = Scratch::new(mode);
        let base = s.head();
        let timeout = if mode == "slow" { 1 } else { 30 };
        let m = s.manifest(&[("t1", &format!("{mode}|src/one.txt|content"))], timeout);
        assert_eq!(code(&s.run("r1", &m, None)), 10, "{mode}");
        assert_eq!(s.head(), base, "{mode}");
        assert_eq!(s.commits(), 1, "{mode}");
        assert!(
            s.repo().join("src/one.txt").exists(),
            "{mode}: work preserved"
        );
        let st = s.status("r1");
        assert_eq!(st["run"]["failure"]["reason"], "worker_rejected", "{mode}");
        let detail = st["run"]["failure"]["detail"].as_str().unwrap();
        assert!(detail.contains(why), "{mode}: {detail}");
    }
}

#[test]
fn reused_session_id_is_rejected_after_the_first_task_commits() {
    let s = Scratch::new("samesid");
    let m = s.manifest(
        &[
            ("t1", "samesid|src/one.txt|a"),
            ("t2", "samesid|docs/two.txt|b"),
        ],
        30,
    );
    assert_eq!(code(&s.run("r1", &m, None)), 10);
    assert_eq!(s.commits(), 2, "only the first task committed");
    let st = s.status("r1");
    assert_eq!(st["run"]["failure"]["task"], "t2");
    assert!(
        st["run"]["failure"]["detail"]
            .as_str()
            .unwrap()
            .contains("already used")
    );
}

#[test]
fn crash_after_cas_recovers_without_a_duplicate_commit() {
    let s = Scratch::new("aftercas");
    let base = s.head();
    let m = s.manifest(
        &[("t1", "ok|src/one.txt|a"), ("t2", "ok|docs/two.txt|b")],
        30,
    );
    assert_eq!(code(&s.run("r1", &m, Some("after_cas"))), 86);
    let landed = s.head();
    assert_ne!(landed, base);
    assert_eq!(s.commits(), 2);
    let st = s.status("r1");
    assert_eq!(st["run"]["phase"], "committing");
    assert_eq!(st["owner_lost"], true);

    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        s.commits(),
        3,
        "task 1 not committed twice, task 2 committed once"
    );
    assert_eq!(s.git(&["rev-parse", "HEAD~1"]), landed);
    assert_eq!(s.git(&["status", "--porcelain"]), "");
    let st = s.status("r1");
    assert_eq!(st["run"]["phase"], "complete");
    assert_eq!(st["run"]["done"].as_array().unwrap().len(), 2);
    assert!(kinds(&s.events("r1")).contains(&"cas_recovered"));
}

#[test]
fn crash_before_cas_is_refused_and_changes_nothing() {
    let s = Scratch::new("beforecas");
    let base = s.head();
    let m = s.manifest(&[("t1", "ok|src/one.txt|a")], 30);
    assert_eq!(code(&s.run("r1", &m, Some("before_cas"))), 86);
    assert_eq!(s.head(), base);
    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 8);
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing was changed"));
    assert_eq!(s.head(), base);
    assert_eq!(s.commits(), 1);
}

#[test]
fn bad_manifests_are_usage_errors_and_create_no_run() {
    let s = Scratch::new("badmanifest");
    let good = s.manifest(&[("t1", "ok|src/one.txt|a")], 30);
    let mut v: Value = serde_json::from_slice(&fs::read(&good).unwrap()).unwrap();
    let edits: [(&str, Value); 4] = [
        ("manifest_version", json!(2)),
        (
            "worker",
            json!({ "argv": ["/usr/local/bin/opencode"], "timeout_secs": 5 }),
        ),
        ("gate", json!({ "argv": [], "timeout_secs": 5 })),
        ("tasks", json!([])),
    ];
    for (key, val) in edits {
        let mut bad = v.clone();
        bad[key] = val;
        let p = s.0.join("bad.json");
        fs::write(&p, bad.to_string()).unwrap();
        assert_eq!(code(&s.run("r1", &p, None)), 2, "{key}");
    }
    v["unknown_field"] = json!(1);
    let p = s.0.join("bad.json");
    fs::write(&p, v.to_string()).unwrap();
    assert_eq!(code(&s.run("r1", &p, None)), 2);
    assert!(!s.0.join("state/v1/runs/r1").exists());
    assert_eq!(code(&s.lmloop(&["fixture-resume", "nope"], None)), 3);
}

fn ctl_prompt(s: &Scratch, mode: &str, path: &str, name: &str) -> String {
    format!("{mode}|{path}|{}", s.run_dir("r1").join(name).display())
}

#[test]
fn stop_waits_for_the_task_boundary_and_commits_the_running_task() {
    let s = Scratch::new("stop");
    let base = s.head();
    let first = ctl_prompt(&s, "ctl", "src/one.txt", "STOP");
    let m = s.manifest(&[("t1", &first), ("t2", "ok|docs/two.txt|never")], 30);
    assert_eq!(code(&s.run("r1", &m, None)), 11);
    assert_eq!(
        s.commits(),
        2,
        "t1 finished and committed; t2 never started"
    );
    assert_ne!(s.head(), base);
    assert!(!s.repo().join("docs/two.txt").exists());
    assert_eq!(s.git(&["status", "--porcelain"]), "");
    let st = s.status("r1");
    assert_eq!(st["run"]["phase"], "stopped");
    assert_eq!(st["run"]["done"].as_array().unwrap().len(), 1);
    assert!(!s.run_dir("r1").join("STOP").exists());
    assert_eq!(
        fs::read_dir(s.run_dir("r1").join("stop-archive"))
            .unwrap()
            .count(),
        1
    );
    assert!(kinds(&s.events("r1")).contains(&"run_stopped"));
    // Stopped is terminal without a human.
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 8);
    assert_eq!(s.commits(), 2);
}

#[test]
fn stop_now_kills_the_worker_group_promptly_and_commits_nothing() {
    let s = Scratch::new("stopnow-worker");
    let base = s.head();
    let pidfile = s.0.join("pid");
    let m = s.manifest(
        &[("t1", &format!("hang|src/one.txt|{}", pidfile.display()))],
        30,
    );
    let child = s.spawn_run("r1", &m);
    let descendant = pid_of(&pidfile);
    let started = Instant::now();
    s.control("r1", "STOP-NOW");
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 11, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(started.elapsed() < Duration::from_secs(8), "not prompt");
    wait_until("the descendant to die", || dead(&descendant));
    assert_eq!(s.head(), base);
    assert_eq!(s.commits(), 1);
    assert!(s.repo().join("src/one.txt").exists(), "changes preserved");
    let st = s.status("r1");
    assert_eq!(st["run"]["phase"], "stopped");
    assert_eq!(st["run"]["failure"]["reason"], "stop_now");
    assert!(!s.run_dir("r1").join("STOP-NOW").exists(), "archived");
}

#[test]
fn stop_now_cancels_a_running_gate_and_keeps_the_candidate() {
    let s = Scratch::new("stopnow-gate");
    let base = s.head();
    let m = s.manifest(&[("t1", "ok|HANG|x")], 30);
    let child = s.spawn_run("r1", &m);
    wait_until("the gate phase", || {
        fs::read_to_string(s.run_dir("r1").join("run.json"))
            .is_ok_and(|t| t.contains("\"phase\": \"gate\""))
    });
    s.control("r1", "STOP-NOW");
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 11, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.head(), base);
    assert!(s.repo().join("HANG").exists());
    let st = s.status("r1");
    assert_eq!(st["run"]["phase"], "stopped");
    assert!(
        st["run"]["failure"]["detail"]
            .as_str()
            .unwrap()
            .contains("gate")
    );
}

#[test]
fn pause_parks_at_the_boundary_and_resume_uses_the_persisted_manifest() {
    let s = Scratch::new("pause");
    let first = ctl_prompt(&s, "ctl", "src/one.txt", "PAUSE");
    let m = s.manifest(
        &[
            ("t1", &first),
            ("t2", "ok|docs/two.txt|b"),
            ("t3", "ok|docs/three.txt|c"),
        ],
        30,
    );
    assert_eq!(code(&s.run("r1", &m, None)), 12);
    assert_eq!(s.commits(), 2);
    let st = s.status("r1");
    assert_eq!(st["run"]["phase"], "paused");
    assert_eq!(st["run"]["next_task"], 1);
    assert!(!s.run_dir("r1").join("PAUSE").exists());

    // The manifest file is gone: everything comes from run.json.
    fs::remove_file(&m).unwrap();
    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.commits(), 4);
    let st = s.status("r1");
    assert_eq!(st["run"]["phase"], "complete");
    assert_eq!(st["run"]["done"].as_array().unwrap().len(), 3);
    let ev = s.events("r1");
    let k = kinds(&ev);
    assert!(k.contains(&"run_paused") && k.contains(&"run_resumed"));
}

#[test]
fn a_competing_owner_cannot_resume_or_touch_controls() {
    let s = Scratch::new("compete");
    let pidfile = s.0.join("pid");
    let m = s.manifest(
        &[("t1", &format!("hang|src/one.txt|{}", pidfile.display()))],
        30,
    );
    let child = s.spawn_run("r1", &m);
    pid_of(&pidfile);
    let before = fs::read(s.run_dir("r1").join("run.json")).unwrap();
    s.control("r1", "PAUSE");

    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 7);
    assert_eq!(code(&s.run("r1", &m, None)), 7);
    assert_eq!(fs::read(s.run_dir("r1").join("run.json")).unwrap(), before);
    assert!(
        s.run_dir("r1").join("PAUSE").exists(),
        "loser left controls alone"
    );
    assert!(!s.run_dir("r1").join("stop-archive").exists());

    s.control("r1", "STOP-NOW");
    assert_eq!(code(&child.wait_with_output().unwrap()), 11);
    assert!(
        s.run_dir("r1").join("PAUSE").exists(),
        "unrelated control kept"
    );
}

#[test]
fn exit_127_from_worker_or_gate_is_a_blocking_failure_that_keeps_the_work() {
    let s = Scratch::new("w127");
    let base = s.head();
    let m = s.manifest(&[("t1", "exit127|src/one.txt|a")], 30);
    assert_eq!(code(&s.run("r1", &m, None)), 10);
    assert_eq!(s.head(), base);
    assert!(s.repo().join("src/one.txt").exists());
    let st = s.status("r1");
    assert_eq!(st["run"]["failure"]["reason"], "worker_rejected");
    assert!(
        st["run"]["failure"]["detail"]
            .as_str()
            .unwrap()
            .contains("Missing")
    );

    let s = Scratch::new("g127");
    let base = s.head();
    let m = s.manifest_gate(&[("t1", "ok|src/one.txt|a")], 30, "gate-127");
    assert_eq!(code(&s.run("r1", &m, None)), 10);
    assert_eq!(s.head(), base);
    assert_eq!(s.status("r1")["run"]["failure"]["reason"], "gate_missing");
}

#[test]
fn a_descendant_left_by_a_clean_worker_is_swept_with_its_group() {
    let s = Scratch::new("descendant");
    let pidfile = s.0.join("pid");
    let m = s.manifest(
        &[("t1", &format!("bg|src/one.txt|{}", pidfile.display()))],
        30,
    );
    let out = s.run("r1", &m, None);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    wait_until("the descendant to die", || dead(&pid_of(&pidfile)));
    assert_eq!(s.commits(), 2);
}

#[test]
fn a_gate_that_rewrites_the_export_is_caught_even_at_equal_length() {
    let s = Scratch::new("mutate");
    let base = s.head();
    let m = s.manifest(&[("t1", "ok|MUTATE|x")], 30);
    assert_eq!(code(&s.run("r1", &m, None)), 10);
    assert_eq!(s.head(), base);
    assert_eq!(
        s.status("r1")["run"]["failure"]["reason"],
        "export_modified"
    );
}

#[test]
fn a_gate_that_changes_export_mode_is_caught() {
    let s = Scratch::new("mode-mutate");
    let base = s.head();
    let gate = s.0.join("gate.sh");
    fs::write(&gate, "#!/bin/sh\nchmod +x MUTATE\n").unwrap();
    let mut perms = fs::metadata(&gate).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    fs::set_permissions(&gate, perms).unwrap();
    let m = s.manifest_gate(&[("t1", "ok|MUTATE|x")], 30, gate.to_str().unwrap());
    assert_eq!(code(&s.run("r1", &m, None)), 10);
    assert_eq!(s.head(), base);
    assert_eq!(
        s.status("r1")["run"]["failure"]["reason"],
        "export_modified"
    );
}

#[test]
fn recovery_never_clobbers_an_independently_staged_index() {
    let s = Scratch::new("staged");
    let m = s.manifest(
        &[("t1", "ok|src/one.txt|a"), ("t2", "ok|docs/two.txt|b")],
        30,
    );
    assert_eq!(code(&s.run("r1", &m, Some("after_cas"))), 86);
    let landed = s.head();
    fs::write(s.repo().join("other.txt"), "mine\n").unwrap();
    s.git(&["add", "other.txt"]);

    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 8, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("staged work left untouched"));
    // Index = old tree + the operator's file: untouched, not read-tree'd to the new commit.
    assert_eq!(
        s.git(&["diff", "--cached", "--name-only", "HEAD~1"]),
        "other.txt"
    );
    assert_eq!(s.head(), landed);
    assert_eq!(
        s.status("r1")["run"]["phase"],
        "committing",
        "evidence kept"
    );
    assert_eq!(s.status("r1")["run"]["intent"]["commit"], landed);
}

#[test]
fn recovery_refuses_a_stale_index_lock_keeps_the_intent_and_succeeds_on_retry() {
    let s = Scratch::new("indexlock");
    let m = s.manifest(
        &[("t1", "ok|src/one.txt|a"), ("t2", "ok|docs/two.txt|b")],
        30,
    );
    assert_eq!(code(&s.run("r1", &m, Some("after_cas"))), 86);
    let landed = s.head();
    let lock = s.repo().join(".git/index.lock");
    fs::write(&lock, "stale\n").unwrap();

    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 8, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("index.lock"));
    assert_eq!(fs::read(&lock).unwrap(), b"stale\n", "never cleared");
    assert_eq!(s.head(), landed);
    assert_eq!(s.status("r1")["run"]["phase"], "committing");
    assert_eq!(s.status("r1")["run"]["intent"]["commit"], landed);

    fs::remove_file(&lock).unwrap();
    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.commits(), 3);
    assert_eq!(s.git(&["status", "--porcelain"]), "");
    assert_eq!(s.status("r1")["run"]["phase"], "complete");
}

#[test]
fn crash_before_cas_keeps_the_intent_and_the_commit_object_as_evidence() {
    let s = Scratch::new("evidence");
    let m = s.manifest(&[("t1", "ok|src/one.txt|a")], 30);
    assert_eq!(code(&s.run("r1", &m, Some("before_cas"))), 86);
    let intended = s.status("r1")["run"]["intent"]["commit"]
        .as_str()
        .unwrap()
        .to_string();
    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 8);
    assert!(String::from_utf8_lossy(&out.stderr).contains(&intended));
    assert_eq!(s.git(&["cat-file", "-t", &intended]), "commit");
    assert_eq!(s.status("r1")["run"]["phase"], "committing");
}

#[test]
fn a_preexisting_index_lock_after_the_cas_exits_8_with_guidance_and_retries_without_a_duplicate() {
    let s = Scratch::new("normal-indexlock");
    let base = s.head();
    let m = s.manifest(
        &[("t1", "ok|src/one.txt|a"), ("t2", "ok|docs/two.txt|b")],
        30,
    );
    let lock = s.repo().join(".git/index.lock");
    fs::write(&lock, "stale\n").unwrap();

    let out = s.run("r1", &m, None);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(code(&out), 8, "{err}");
    for want in [
        "landed at the ref CAS",
        "intent is preserved",
        "fixture-resume r1",
        "index.lock",
    ] {
        assert!(err.contains(want), "missing {want:?}: {err}");
    }
    assert_eq!(fs::read(&lock).unwrap(), b"stale\n", "never cleared");
    let landed = s.head();
    assert_eq!(s.git(&["rev-parse", "HEAD~1"]), base);
    assert_eq!(s.commits(), 2, "exactly the CAS commit, no task 2");
    let st = s.status("r1");
    assert_eq!(st["run"]["phase"], "committing");
    assert_eq!(st["run"]["intent"]["commit"], landed);
    assert_eq!(st["run"]["next_task"], 0, "no advance");

    fs::remove_file(&lock).unwrap();
    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.commits(), 3, "t1 committed once, then t2");
    assert_eq!(s.git(&["rev-parse", "HEAD~1"]), landed);
    assert_eq!(s.git(&["status", "--porcelain"]), "");
    assert_eq!(s.status("r1")["run"]["phase"], "complete");
}

// ---- journal durability (lm-1x4): torn tails, archives, refusals ----

fn append_bytes(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

fn log_path(s: &Scratch) -> PathBuf {
    s.run_dir("r1").join("events.jsonl")
}

fn archives(s: &Scratch) -> Vec<String> {
    let dir = s.run_dir("r1").join("journal-recovery");
    let mut names: Vec<String> = fs::read_dir(dir).map_or(Vec::new(), |d| {
        d.map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    });
    names.sort();
    names
}

/// A run parked by PAUSE after t1, with t2 and t3 still to do.
fn paused(name: &str) -> (Scratch, PathBuf) {
    let s = Scratch::new(name);
    let first = ctl_prompt(&s, "ctl", "src/one.txt", "PAUSE");
    let m = s.manifest(
        &[
            ("t1", &first),
            ("t2", "ok|docs/two.txt|b"),
            ("t3", "ok|docs/three.txt|c"),
        ],
        30,
    );
    assert_eq!(code(&s.run("r1", &m, None)), 12);
    (s, m)
}

fn assert_clean_seqs(s: &Scratch) {
    let seqs: Vec<u64> = s
        .events("r1")
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
}

#[test]
fn paused_run_with_a_torn_tail_archives_the_exact_bytes_then_continues_the_sequence() {
    let s = Scratch::new("torn-paused-unused");
    drop(s);
    let (s, _) = paused("torn-paused");
    let prefix = fs::read(log_path(&s)).unwrap();
    let last_seq = s.events("r1").len() as u64;
    // Not UTF-8 and not JSON: whatever a crash left behind, kept byte for byte.
    let tail = b"{\"v\":1,\"seq\":99,\"kind\":\"ta\xff\xfe";
    append_bytes(&log_path(&s), tail);

    // Read-only status never repairs.
    let before = fs::read(log_path(&s)).unwrap();
    s.status("r1");
    assert_eq!(fs::read(log_path(&s)).unwrap(), before);
    assert!(archives(&s).is_empty());

    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        archives(&s),
        [format!("events-tail-{}.0", prefix.len())],
        "name carries the prefix byte offset"
    );
    let arch = s
        .run_dir("r1")
        .join("journal-recovery")
        .join(&archives(&s)[0]);
    assert_eq!(fs::read(&arch).unwrap(), tail);
    let now = fs::read(log_path(&s)).unwrap();
    assert!(now.starts_with(&prefix), "complete records never rewritten");
    assert_clean_seqs(&s);
    assert_eq!(s.events("r1")[last_seq as usize]["kind"], "run_resumed");
    assert_eq!(s.events("r1")[last_seq as usize]["seq"], last_seq + 1);
    assert_eq!(s.status("r1")["run"]["phase"], "complete");
    assert_eq!(s.commits(), 4, "base + three tasks, none twice");
}

#[test]
fn an_unterminated_but_valid_record_is_still_torn_and_never_counted() {
    let (s, _) = paused("torn-valid");
    let prefix = fs::read(log_path(&s)).unwrap();
    let n = s.events("r1").len() as u64;
    // A syntactically perfect next record, minus its delimiter: it was never acknowledged.
    let tail = format!(
        "{{\"v\":1,\"seq\":{},\"at_unix\":1,\"run_id\":\"r1\",\"kind\":\"ghost\",\"task\":null,\"data\":{{}}}}",
        n + 1
    );
    append_bytes(&log_path(&s), tail.as_bytes());

    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 0);
    let arch = s
        .run_dir("r1")
        .join("journal-recovery")
        .join(format!("events-tail-{}.0", prefix.len()));
    assert_eq!(fs::read(arch).unwrap(), tail.as_bytes());
    assert!(!kinds(&s.events("r1")).contains(&"ghost"));
    assert_clean_seqs(&s);
}

#[test]
fn crash_after_cas_with_a_torn_tail_recovers_once_and_archives_the_tail() {
    let s = Scratch::new("torn-aftercas");
    let m = s.manifest(
        &[("t1", "ok|src/one.txt|a"), ("t2", "ok|docs/two.txt|b")],
        30,
    );
    assert_eq!(code(&s.run("r1", &m, Some("after_cas"))), 86);
    let landed = s.head();
    let prefix = fs::read(log_path(&s)).unwrap();
    append_bytes(&log_path(&s), b"{\"v\":1,\"seq\"");

    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.commits(), 3, "no duplicate commit");
    assert_eq!(s.git(&["rev-parse", "HEAD~1"]), landed);
    assert_eq!(archives(&s), [format!("events-tail-{}.0", prefix.len())]);
    assert_clean_seqs(&s);
    let ev = s.events("r1");
    let k = kinds(&ev);
    assert_eq!(k.iter().filter(|k| **k == "task_committed").count(), 2);
    assert!(k.contains(&"cas_recovered"));
}

#[test]
fn retry_after_an_interrupted_recovery_reuses_a_matching_archive_and_never_overwrites_another() {
    // Interrupted after the archive landed, before the truncate: same bytes, same name.
    let (s, _) = paused("torn-retry");
    let prefix = fs::read(log_path(&s)).unwrap();
    let tail = b"half a rec";
    append_bytes(&log_path(&s), tail);
    let dir = s.run_dir("r1").join("journal-recovery");
    fs::create_dir(&dir).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let existing = dir.join(format!("events-tail-{}.0", prefix.len()));
    fs::write(&existing, tail).unwrap();
    fs::set_permissions(&existing, fs::Permissions::from_mode(0o600)).unwrap();

    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 0);
    assert_eq!(archives(&s), [format!("events-tail-{}.0", prefix.len())]);
    assert_eq!(fs::read(&existing).unwrap(), tail);
    assert_clean_seqs(&s);

    // A different file already holds the name (e.g. a partial one from another failure): kept, next slot used.
    let (s, _) = paused("torn-differs");
    let prefix = fs::read(log_path(&s)).unwrap();
    append_bytes(&log_path(&s), tail);
    let dir = s.run_dir("r1").join("journal-recovery");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let other = dir.join(format!("events-tail-{}.0", prefix.len()));
    fs::write(&other, b"someone else's evidence").unwrap();
    fs::set_permissions(&other, fs::Permissions::from_mode(0o600)).unwrap();

    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 0);
    assert_eq!(fs::read(&other).unwrap(), b"someone else's evidence");
    assert_eq!(
        fs::read(dir.join(format!("events-tail-{}.1", prefix.len()))).unwrap(),
        tail
    );
    // A second resume of the finished run has nothing left to repair or archive.
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 0);
    assert_eq!(archives(&s).len(), 2);
}

#[test]
fn a_bad_complete_record_is_refused_with_the_log_controls_and_state_untouched() {
    let rec = |seq: u64, v: u64, run: &str| {
        format!(
            "{{\"v\":{v},\"seq\":{seq},\"at_unix\":1,\"run_id\":\"{run}\",\"kind\":\"x\",\"task\":null,\"data\":{{}}}}\n"
        )
    };
    for name in [
        "malformed",
        "extra-field",
        "foreign",
        "version",
        "gap",
        "repeat",
    ] {
        let (s, _) = paused(&format!("bad-{name}"));
        let n = s.events("r1").len() as u64 + 1;
        let bad = match name {
            "malformed" => "not json at all\n".to_string(),
            "extra-field" => rec(n, 1, "r1").replace("\"data\"", "\"zz\":1,\"data\""),
            "foreign" => rec(n, 1, "other"),
            "version" => rec(n, 2, "r1"),
            "gap" => rec(n + 1, 1, "r1"),
            _ => rec(n - 1, 1, "r1"),
        };
        append_bytes(&log_path(&s), bad.as_bytes());
        // A torn tail after the bad record must not be archived or truncated either.
        append_bytes(&log_path(&s), b"torn");
        s.control("r1", "STOP");
        let (log, run) = (
            fs::read(log_path(&s)).unwrap(),
            fs::read(s.run_dir("r1").join("run.json")).unwrap(),
        );
        let archived = fs::read_dir(s.run_dir("r1").join("stop-archive"))
            .unwrap()
            .count();

        let out = s.lmloop(&["fixture-resume", "r1"], None);
        assert_eq!(
            code(&out),
            8,
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("journal_"),
            "{name}"
        );
        assert_eq!(fs::read(log_path(&s)).unwrap(), log, "{name}");
        assert_eq!(
            fs::read(s.run_dir("r1").join("run.json")).unwrap(),
            run,
            "{name}"
        );
        assert!(
            s.run_dir("r1").join("STOP").exists(),
            "{name}: control kept"
        );
        assert_eq!(
            fs::read_dir(s.run_dir("r1").join("stop-archive"))
                .unwrap()
                .count(),
            archived,
            "{name}: nothing newly archived"
        );
        assert!(archives(&s).is_empty(), "{name}");
        assert_eq!(s.commits(), 2, "{name}");
    }
}

#[test]
fn only_resumable_phases_are_repaired_and_unsafe_artifacts_are_refused() {
    // A failed run is refused (exit 8) before the journal is looked at.
    let s = Scratch::new("torn-failed");
    let m = s.manifest(&[("t1", "ok|src/one.txt|FAIL here")], 30);
    assert_eq!(code(&s.run("r1", &m, None)), 10);
    append_bytes(&log_path(&s), b"torn");
    let log = fs::read(log_path(&s)).unwrap();
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 8);
    assert_eq!(fs::read(log_path(&s)).unwrap(), log);
    assert!(archives(&s).is_empty());

    // Crash before the CAS: refused, the torn tail stays for a human.
    let s = Scratch::new("torn-beforecas");
    let m = s.manifest(&[("t1", "ok|src/one.txt|a")], 30);
    assert_eq!(code(&s.run("r1", &m, Some("before_cas"))), 86);
    append_bytes(&log_path(&s), b"torn");
    let log = fs::read(log_path(&s)).unwrap();
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 8);
    assert_eq!(fs::read(log_path(&s)).unwrap(), log);
    assert!(archives(&s).is_empty());

    // A symlinked archive directory is refused; the tail is not truncated.
    let (s, _) = paused("torn-symlink");
    append_bytes(&log_path(&s), b"torn");
    let log = fs::read(log_path(&s)).unwrap();
    let elsewhere = s.0.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, s.run_dir("r1").join("journal-recovery")).unwrap();
    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 8, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(log_path(&s)).unwrap(), log);
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);

    // A symlinked journal is refused outright.
    let (s, _) = paused("link-journal");
    let real = s.0.join("real-events");
    fs::rename(log_path(&s), &real).unwrap();
    std::os::unix::fs::symlink(&real, log_path(&s)).unwrap();
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 8);
    assert_eq!(s.status("r1")["run"]["phase"], "paused");
}

#[test]
fn a_losing_owner_leaves_a_torn_journal_exactly_as_found() {
    let (s, _) = paused("torn-loser");
    // Hold the lease with a second run? No: hold it by resuming a hanging task.
    let pidfile = s.0.join("pid");
    let m2 = s.manifest(
        &[("t9", &format!("hang|src/nine.txt|{}", pidfile.display()))],
        30,
    );
    drop(m2);
    // Simplest owner: fixture-run on a different id would not share the dir, so lock it directly.
    use std::os::unix::fs::OpenOptionsExt;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(s.run_dir("r1").join("owner.lock"))
        .unwrap();
    lock.lock().unwrap();
    append_bytes(&log_path(&s), b"torn");
    let log = fs::read(log_path(&s)).unwrap();
    let run = fs::read(s.run_dir("r1").join("run.json")).unwrap();
    s.control("r1", "STOP");

    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 7);
    assert_eq!(fs::read(log_path(&s)).unwrap(), log);
    assert_eq!(fs::read(s.run_dir("r1").join("run.json")).unwrap(), run);
    assert!(s.run_dir("r1").join("STOP").exists());
    assert!(archives(&s).is_empty());
}

#[test]
fn a_missing_journal_with_run_state_is_refused_not_invented() {
    let (s, _) = paused("missing-journal");
    fs::remove_file(log_path(&s)).unwrap();
    let run = fs::read(s.run_dir("r1").join("run.json")).unwrap();
    let out = s.lmloop(&["fixture-resume", "r1"], None);
    assert_eq!(code(&out), 8, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("journal_missing"));
    assert!(!log_path(&s).exists(), "no history was created");
    assert_eq!(fs::read(s.run_dir("r1").join("run.json")).unwrap(), run);
}

#[test]
fn archive_never_truncates_a_predictable_temp_and_refuses_a_hard_linked_archive() {
    use std::os::unix::fs::PermissionsExt;
    let private =
        |p: &Path, mode: u32| fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
    let tail = b"half a rec";

    // A stale temp under the old predictable name, hard-linked to evidence, survives untouched.
    let (s, _) = paused("tmp-evidence");
    let prefix = fs::read(log_path(&s)).unwrap();
    append_bytes(&log_path(&s), tail);
    let dir = s.run_dir("r1").join("journal-recovery");
    fs::create_dir(&dir).unwrap();
    private(&dir, 0o700);
    let evidence = s.0.join("evidence");
    fs::write(&evidence, b"precious").unwrap();
    private(&evidence, 0o600);
    fs::hard_link(
        &evidence,
        dir.join(format!("events-tail-{}.0.tmp", prefix.len())),
    )
    .unwrap();
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 0);
    assert_eq!(fs::read(&evidence).unwrap(), b"precious");
    assert_eq!(
        fs::read(dir.join(format!("events-tail-{}.0", prefix.len()))).unwrap(),
        tail
    );

    // A matching archive that is hard-linked elsewhere is refused and the tail kept.
    let (s, _) = paused("archive-hardlink");
    let prefix = fs::read(log_path(&s)).unwrap();
    append_bytes(&log_path(&s), tail);
    let log = fs::read(log_path(&s)).unwrap();
    let dir = s.run_dir("r1").join("journal-recovery");
    fs::create_dir(&dir).unwrap();
    private(&dir, 0o700);
    let linked = dir.join(format!("events-tail-{}.0", prefix.len()));
    fs::write(&linked, tail).unwrap();
    private(&linked, 0o600);
    fs::hard_link(&linked, s.0.join("alias")).unwrap();
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 8);
    assert_eq!(fs::read(log_path(&s)).unwrap(), log);
}

/// A pilot manifest: fixture worker installed as `<scratch>/bin/opencode` (the launcher
/// name the fixture runner refuses); `pilot_tasks` supports one to three tasks.
fn pilot_manifest(s: &Scratch, prompt: &str, scope: &str, gate: &str) -> PathBuf {
    let p = pilot_tasks(s, &[("t1", prompt)], gate);
    let mut v: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    v["tasks"][0]["scopes"] = json!([scope]);
    fs::write(&p, v.to_string()).unwrap();
    p
}

/// As `pilot_manifest` for several tasks, each scoped to its own path.
fn pilot_tasks(s: &Scratch, tasks: &[(&str, &str)], gate: &str) -> PathBuf {
    let p = s.manifest_gate(tasks, 30, gate);
    let bin = s.0.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let worker = bin.join("opencode");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-opencode"),
        &worker,
    )
    .unwrap();
    let mut v: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    v["worker"]["argv"] = json!([worker]);
    fs::write(&p, v.to_string()).unwrap();
    p
}

fn pilot(s: &Scratch, id: &str, m: &Path) -> Output {
    s.lmloop(&["pilot-run", id, "--manifest", m.to_str().unwrap()], None)
}

#[test]
fn fixture_refuses_the_opencode_launcher_but_a_pilot_commits_with_the_gate() {
    let s = Scratch::new("pilot");
    let base = s.head();
    let m = pilot_manifest(&s, "ok|src/one.txt|first", "src/one.txt", "gate-no-fail");
    assert_eq!(
        code(&s.run("f1", &m, None)),
        2,
        "fixture must refuse opencode"
    );
    assert!(!s.run_dir("f1").exists());

    let out = pilot(&s, "p1", &m);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("pilot") && !said.contains("fixture"),
        "{said}"
    );
    assert_eq!(s.commits(), 2);
    assert_eq!(
        s.git(&["diff", "--name-only", &base, "HEAD"]),
        "src/one.txt"
    );
    assert_eq!(s.git(&["log", "-1", "--format=%s"]), "lmloop pilot: t1");
    assert_eq!(s.git(&["status", "--porcelain"]), "");

    let st = s.status("p1");
    assert_eq!(st["run"]["pilot"], true);
    assert_eq!(st["kind"], "serial_pilot");
    assert_eq!(st["run"]["phase"], "complete");
    let text = s.lmloop(&["status", "p1"], None).stdout;
    assert!(String::from_utf8_lossy(&text).contains("pilot"));

    // One-shot: neither the fixture resume path nor a re-run touches a pilot.
    let head = s.head();
    assert_eq!(code(&s.lmloop(&["fixture-resume", "p1"], None)), 8);
    assert_eq!(code(&pilot(&s, "p1", &m)), 2);
    assert_eq!(s.head(), head);
}

#[test]
fn a_pilot_with_a_failing_gate_or_out_of_scope_change_commits_nothing() {
    let s = Scratch::new("pilotfail");
    let base = s.head();
    let m = pilot_manifest(
        &s,
        "ok|src/one.txt|FAIL here",
        "src/one.txt",
        "gate-no-fail",
    );
    assert_eq!(code(&pilot(&s, "p1", &m)), 10);
    assert_eq!(s.head(), base);
    assert_eq!(s.status("p1")["run"]["failure"]["reason"], "gate_failed");
    assert_eq!(
        fs::read_to_string(s.repo().join("src/one.txt")).unwrap(),
        "FAIL here\n",
        "work preserved"
    );
    assert_eq!(code(&s.lmloop(&["fixture-resume", "p1"], None)), 8);

    let s = Scratch::new("pilotscope");
    let base = s.head();
    let m = pilot_manifest(&s, "ok|src/one.txt|x", "docs", "gate-no-fail");
    assert_eq!(code(&pilot(&s, "p1", &m)), 10);
    assert_eq!(s.head(), base);
    assert!(s.repo().join("src/one.txt").exists(), "work preserved");
}

#[test]
fn pilot_policy_is_set_by_the_cli_alone_and_needs_an_isolated_state_root() {
    let s = Scratch::new("pilotpolicy");
    let m = pilot_manifest(&s, "ok|src/one.txt|a", "src/one.txt", "gate-no-fail");
    let mut v: Value = serde_json::from_slice(&fs::read(&m).unwrap()).unwrap();
    let edit = |s: &Scratch, v: &Value| {
        let p = s.0.join("bad.json");
        fs::write(&p, v.to_string()).unwrap();
        p
    };

    // A manifest cannot opt in, and a pilot is at most three tasks (four are refused).
    let mut optin = v.clone();
    optin["pilot"] = json!(true);
    let p = edit(&s, &optin);
    assert_eq!(code(&s.run("r1", &p, None)), 2);
    assert_eq!(code(&pilot(&s, "r1", &p)), 2);
    let task = v["tasks"][0].clone();
    v["tasks"] = json!([task, { "id": "t2", "prompt": "ok|docs/b|b", "scopes": ["docs/b"] },
        { "id": "t3", "prompt": "ok|docs/c|c", "scopes": ["docs/c"] },
        { "id": "t4", "prompt": "ok|docs/d|d", "scopes": ["docs/d"] }]);
    let out = pilot(&s, "r1", &edit(&s, &v));
    assert_eq!(code(&out), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("1-3 tasks"));
    let mut no_tasks = v.clone();
    no_tasks["tasks"] = json!([]);
    assert_eq!(code(&pilot(&s, "r1", &edit(&s, &no_tasks))), 2);
    assert!(!s.run_dir("r1").exists());

    // The state root must be explicit (flag, not the environment or the default) ...
    let bare = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_lmloop"))
            .env("LMLOOP_RUST_STATE_ROOT", s.0.join("state"))
            .env("HOME", &s.0)
            .env("XDG_STATE_HOME", s.0.join("xdg"))
            .args(args)
            .output()
            .unwrap()
    };
    let out = bare(&["pilot-run", "r1", "--manifest", m.to_str().unwrap()]);
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!s.run_dir("r1").exists() && !s.0.join("xdg").exists());
    // ... also when given after the subcommand ...
    let late = bare(&[
        "pilot-run",
        "r1",
        "--manifest",
        m.to_str().unwrap(),
        "--state-root",
        s.0.join("state").to_str().unwrap(),
    ]);
    assert_eq!(code(&late), 0, "{}", String::from_utf8_lossy(&late.stderr));

    // ... and must not overlap the repository the pilot edits.
    let inside = s.repo().join("state");
    let out = Command::new(env!("CARGO_BIN_EXE_lmloop"))
        .args([
            "--state-root",
            inside.to_str().unwrap(),
            "pilot-run",
            "r2",
            "--manifest",
            m.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!inside.exists());

    // A fixture run's state without the pilot key still reads as a fixture.
    let f = Scratch::new("pilotlegacy");
    let fm = f.manifest(&[("t1", "ok|src/one.txt|a")], 30);
    assert_eq!(code(&f.run("r1", &fm, None)), 0);
    let rj = f.run_dir("r1").join("run.json");
    let mut st: Value = serde_json::from_slice(&fs::read(&rj).unwrap()).unwrap();
    assert_eq!(st["pilot"], false);
    st.as_object_mut().unwrap().remove("pilot");
    fs::write(&rj, st.to_string()).unwrap();
    let st = f.status("r1");
    assert_eq!(st["run"]["pilot"], false);
    assert_eq!(st["kind"], "serial_fixture");
}

#[test]
fn pilot_messages_never_point_at_fixture_resume() {
    let s = Scratch::new("pilotmsg");
    let m = pilot_manifest(&s, "ok|src/one.txt|a", "src/one.txt", "gate-no-fail");
    let lock = s.repo().join(".git/index.lock");
    fs::write(&lock, "stale\n").unwrap();
    let out = pilot(&s, "p1", &m);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(code(&out), 8, "{err}");
    assert!(
        err.contains("index.lock") && err.contains("one-shot"),
        "{err}"
    );
    assert!(!err.contains("fixture-resume"), "{err}");
    fs::remove_file(&lock).unwrap();

    // A duplicate id, asked of either runner, says one-shot and never names fixture-resume.
    for out in [pilot(&s, "p1", &m), s.run("p1", &m, None)] {
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(code(&out), 2, "{err}");
        assert!(!err.contains("fixture-resume"), "{err}");
    }
    assert!(String::from_utf8_lossy(&s.lmloop(&["status", "p1"], None).stdout).contains("pilot"));
}

#[test]
fn a_three_task_pilot_commits_in_order_with_a_fresh_session_each() {
    let s = Scratch::new("pilot3");
    let base = s.head();
    let m = pilot_tasks(
        &s,
        &[
            ("t1", "ok|src/one.txt|a"),
            ("t2", "ok|docs/two.txt|b"),
            ("t3", "ok|docs/three.txt|c"),
        ],
        "gate-no-fail",
    );
    let out = pilot(&s, "p1", &m);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.commits(), 4);
    assert_eq!(s.git(&["rev-parse", "HEAD~3"]), base);
    let subjects = s.git(&["log", "-3", "--format=%s", "--reverse"]);
    assert_eq!(
        subjects,
        "lmloop pilot: t1\nlmloop pilot: t2\nlmloop pilot: t3"
    );
    assert_eq!(s.git(&["status", "--porcelain"]), "");

    let st = s.status("p1");
    assert_eq!(st["kind"], "serial_pilot");
    assert_eq!(st["run"]["phase"], "complete");
    let sids: Vec<&Value> = st["run"]["done"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| &d["session_id"])
        .collect();
    assert_eq!(sids.len(), 3);
    assert!(sids[0] != sids[1] && sids[1] != sids[2] && sids[0] != sids[2]);

    let stage = [
        "task_started",
        "worker_finished",
        "snapshot_captured",
        "gate_finished",
        "commit_intent",
        "task_committed",
    ];
    let mut want = vec!["run_started"];
    (0..3).for_each(|_| want.extend(stage));
    want.push("run_completed");
    assert_eq!(kinds(&s.events("p1")), want);
    assert_eq!(code(&s.lmloop(&["fixture-resume", "p1"], None)), 8);
    assert_eq!(code(&pilot(&s, "p1", &m)), 2);
}

/// Event kinds for task `id` only.
fn task_kinds<'a>(ev: &'a [Value], id: &str) -> Vec<&'a str> {
    let of = |e: &&Value| e["task"] == id;
    ev.iter()
        .filter(of)
        .map(|e| e["kind"].as_str().unwrap())
        .collect()
}

#[test]
fn a_pilot_stops_at_the_first_failure_and_launches_nothing_later() {
    // Gate failure on task two.
    let s = Scratch::new("pilot3gate");
    let m = pilot_tasks(
        &s,
        &[
            ("t1", "ok|src/one.txt|a"),
            ("t2", "ok|docs/two.txt|FAIL"),
            ("t3", "ok|docs/three.txt|c"),
        ],
        "gate-no-fail",
    );
    assert_eq!(code(&pilot(&s, "p1", &m)), 10);
    assert_eq!(s.commits(), 2, "only task one committed");
    assert!(
        !s.repo().join("docs/three.txt").exists(),
        "task three never launched"
    );
    let st = s.status("p1");
    assert_eq!(st["run"]["phase"], "failed");
    assert_eq!(st["run"]["failure"]["task"], "t2");
    assert_eq!(st["run"]["next_task"], 1);
    let ev = s.events("p1");
    assert!(task_kinds(&ev, "t3").is_empty(), "no t3 event");
    assert_eq!(kinds(&ev).last(), Some(&"task_failed"));
    assert_eq!(code(&s.lmloop(&["fixture-resume", "p1"], None)), 8);

    // A repeated sessionID is refused: task one is the session task two would reuse.
    let s = Scratch::new("pilot3sid");
    let m = pilot_tasks(
        &s,
        &[
            ("t1", "samesid|src/one.txt|a"),
            ("t2", "samesid|docs/two.txt|b"),
            ("t3", "ok|docs/three.txt|c"),
        ],
        "gate-no-fail",
    );
    assert_eq!(code(&pilot(&s, "p1", &m)), 10);
    assert_eq!(s.commits(), 2);
    let st = s.status("p1");
    assert_eq!(st["run"]["failure"]["task"], "t2");
    assert_eq!(st["run"]["failure"]["reason"], "worker_rejected");
    assert!(
        st["run"]["failure"]["detail"]
            .as_str()
            .unwrap()
            .contains("already used")
    );
    assert!(!s.repo().join("docs/three.txt").exists());
    assert!(task_kinds(&s.events("p1"), "t3").is_empty());
}

#[test]
fn a_pilot_honours_pause_and_stop_only_at_task_boundaries_and_cannot_resume() {
    for (ctl, exit, phase) in [("PAUSE", 12, "paused"), ("STOP", 11, "stopped")] {
        let s = Scratch::new(&format!("pilot3{ctl}"));
        let first = format!("ctl|src/one.txt|{}", s.run_dir("p1").join(ctl).display());
        let m = pilot_tasks(
            &s,
            &[
                ("t1", &first),
                ("t2", "ok|docs/two.txt|b"),
                ("t3", "ok|docs/three.txt|c"),
            ],
            "gate-no-fail",
        );
        let out = pilot(&s, "p1", &m);
        assert_eq!(
            code(&out),
            exit,
            "{ctl}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            s.commits(),
            2,
            "{ctl}: the running task finished and committed"
        );
        assert!(
            !s.repo().join("docs/two.txt").exists(),
            "{ctl}: t2 never launched"
        );
        let st = s.status("p1");
        assert_eq!(st["run"]["phase"], phase);
        assert_eq!(st["run"]["next_task"], 1);
        assert!(task_kinds(&s.events("p1"), "t2").is_empty());
        // One-shot: no resume, and the message never points at fixture-resume.
        let again = s.lmloop(&["fixture-resume", "p1"], None);
        assert_eq!(code(&again), 8, "{ctl}");
        assert!(!String::from_utf8_lossy(&out.stderr).contains("fixture-resume"));
        assert_eq!(s.commits(), 2);
    }
}

// ---- worker watchdog (pilot-run seam, fake worker, no model) ----

/// A pilot over `(id, mode, path)` tasks whose worker has a watchdog on a precreated private
/// progress file (the last argument of every prompt, as the fixture expects).
fn watch_run(
    s: &Scratch,
    tasks: &[(&str, &str, &str)],
    (startup, idle): (u64, u64),
    hard: Option<u64>,
) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::OpenOptionsExt;
    let progress = s.0.join("progress");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&progress)
        .unwrap();
    let prompts: Vec<String> = tasks
        .iter()
        .map(|(_, m, p)| format!("{m}|{p}|{}", progress.display()))
        .collect();
    let pairs: Vec<(&str, &str)> = tasks
        .iter()
        .zip(&prompts)
        .map(|((id, _, _), p)| (*id, p.as_str()))
        .collect();
    let m = pilot_tasks(s, &pairs, "gate-no-fail");
    let mut v: Value = serde_json::from_slice(&fs::read(&m).unwrap()).unwrap();
    v["worker"] = json!({
        "argv": v["worker"]["argv"],
        "watchdog": { "progress_file": progress, "startup_secs": startup, "idle_secs": idle },
    });
    if let Some(h) = hard {
        v["worker"]["timeout_secs"] = json!(h);
    }
    fs::write(&m, v.to_string()).unwrap();
    (m, progress)
}

type Tamper = fn(&Scratch, &Path);
type Edit = fn(&mut Value);

fn watchdog(s: &Scratch, id: &str) -> Value {
    serde_json::from_slice(&fs::read(s.run_dir(id).join("watchdog.json")).unwrap()).unwrap()
}

fn failure(s: &Scratch, id: &str) -> (String, String) {
    let st = s.status(id);
    let f = &st["run"]["failure"];
    (
        f["reason"].as_str().unwrap().into(),
        f["detail"].as_str().unwrap().into(),
    )
}

#[test]
fn streaming_model_activity_keeps_a_worker_alive_past_the_idle_window() {
    let s = Scratch::new("wd-stream");
    // 4 s of work against a 2 s idle window and no hard timeout; a record every 0.5 s.
    let (m, _) = watch_run(&s, &[("t1", "pulse", "src/one.txt")], (3, 2), None);
    let out = pilot(&s, "p1", &m);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.commits(), 2);
    let w = watchdog(&s, "p1");
    assert_eq!(w["state"], "ended");
    assert_eq!(w["model_pulses"], 8);
    assert_eq!(w["tools_observed"], "completed_only");
    assert_eq!(w["tools_completed"], 1);
    assert_eq!(w["trip"], Value::Null);
    // The same evidence reaches the shared operator API and the CLI status text.
    let view = lmloop::operator::Operator::new(&s.0.join("state"))
        .view("p1")
        .unwrap();
    assert_eq!(view.watchdog.unwrap().model_pulses, 8);
    let text = s.lmloop(&["status", "p1"], None);
    assert!(String::from_utf8_lossy(&text.stdout).contains("watchdog: ended"));
    // The policy keeps no timeout_secs it was never given.
    assert!(st_policy_worker(&s, "p1").get("timeout_secs").is_none());
}

fn st_policy_worker(s: &Scratch, id: &str) -> Value {
    s.status(id)["run"]["policy"]["worker"].clone()
}

#[test]
fn a_startup_stall_trips_and_preserves_the_work_and_the_evidence() {
    let s = Scratch::new("wd-startup");
    let base = s.head();
    let (m, _) = watch_run(&s, &[("t1", "stall", "src/one.txt")], (1, 30), None);
    let started = Instant::now();
    let out = pilot(&s, "p1", &m);
    assert_eq!(code(&out), 10, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(started.elapsed() < Duration::from_secs(10));
    let (reason, detail) = failure(&s, "p1");
    assert_eq!(reason, "worker_watchdog");
    assert!(detail.contains("startup stall"), "{detail}");
    assert_eq!(s.head(), base);
    assert!(
        s.repo().join("src/one.txt").exists(),
        "dirty work preserved"
    );
    let w = watchdog(&s, "p1");
    assert_eq!(
        (w["state"].as_str(), w["model_pulses"].as_u64()),
        (Some("tripped"), Some(0))
    );
    assert!(w["trip"].as_str().unwrap().contains("startup stall"));
    let view = lmloop::operator::Operator::new(&s.0.join("state"))
        .view("p1")
        .unwrap();
    assert!(
        view.watchdog.unwrap().trip.is_some(),
        "kept through failure"
    );
}

#[test]
fn idle_after_the_first_record_trips_on_the_idle_window() {
    let s = Scratch::new("wd-idle");
    let (m, _) = watch_run(&s, &[("t1", "pulse1", "src/one.txt")], (20, 1), None);
    let out = pilot(&s, "p1", &m);
    assert_eq!(code(&out), 10, "{}", String::from_utf8_lossy(&out.stderr));
    let (reason, detail) = failure(&s, "p1");
    assert_eq!(reason, "worker_watchdog");
    assert!(detail.contains("idle: no model activity"), "{detail}");
    assert_eq!(watchdog(&s, "p1")["model_pulses"], 1);
}

#[test]
fn stdout_noise_is_not_liveness() {
    let s = Scratch::new("wd-noise");
    let (m, _) = watch_run(&s, &[("t1", "noise", "src/one.txt")], (1, 30), None);
    let out = pilot(&s, "p1", &m);
    assert_eq!(code(&out), 10, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(failure(&s, "p1").1.contains("startup stall"));
}

#[test]
fn a_hard_timeout_still_applies_and_stop_now_still_works_with_a_watchdog() {
    let s = Scratch::new("wd-cap");
    let (m, _) = watch_run(&s, &[("t1", "forever", "src/one.txt")], (5, 5), Some(2));
    let out = pilot(&s, "p1", &m);
    assert_eq!(code(&out), 10, "{}", String::from_utf8_lossy(&out.stderr));
    let (reason, detail) = failure(&s, "p1");
    assert_eq!(reason, "worker_rejected", "the cap is not the watchdog");
    assert!(detail.contains("TimedOut"), "{detail}");
    let w = watchdog(&s, "p1");
    assert!(
        w["model_pulses"].as_u64().unwrap() > 3,
        "evidence kept: {w}"
    );
    assert_eq!(w["state"], "ended");
    assert!(s.repo().join("src/one.txt").exists());

    let s = Scratch::new("wd-stopnow");
    let base = s.head();
    let (m, progress) = watch_run(&s, &[("t1", "forever", "src/one.txt")], (5, 5), None);
    let child = s.spawn_pilot("p1", &m);
    wait_until("model records", || {
        fs::metadata(&progress).is_ok_and(|m| m.len() >= 12)
    });
    s.control("p1", "STOP-NOW");
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 11, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.head(), base);
    assert_eq!(s.status("p1")["run"]["failure"]["reason"], "stop_now");
    assert!(s.repo().join("src/one.txt").exists());
    assert!(watchdog(&s, "p1")["model_pulses"].as_u64().unwrap() >= 2);
}

impl Scratch {
    fn spawn_pilot(&self, id: &str, manifest: &Path) -> Child {
        Command::new(env!("CARGO_BIN_EXE_lmloop"))
            .env_remove("LMLOOP_RUST_STATE_ROOT")
            .arg("--state-root")
            .arg(self.0.join("state"))
            .args(["pilot-run", id, "--manifest", manifest.to_str().unwrap()])
            .current_dir(&self.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
}

#[test]
fn an_invalid_or_truncated_progress_source_fails_closed() {
    for (mode, want) in [
        ("badsrc", "not whole `model` records"),
        ("trunc", "truncated"),
    ] {
        let s = Scratch::new(&format!("wd-{mode}"));
        let (m, _) = watch_run(&s, &[("t1", mode, "src/one.txt")], (20, 20), None);
        let out = pilot(&s, "p1", &m);
        assert_eq!(
            code(&out),
            10,
            "{mode}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let (reason, detail) = failure(&s, "p1");
        assert_eq!(reason, "worker_watchdog", "{mode}");
        assert!(
            detail.contains("failed closed") && detail.contains(want),
            "{mode}: {detail}"
        );
        assert!(
            s.repo().join("src/one.txt").exists(),
            "{mode}: work preserved"
        );
    }
}

#[test]
fn an_unsafe_progress_file_stops_the_run_before_any_worker_starts() {
    use std::os::unix::fs::{OpenOptionsExt, symlink};
    let cases: [(&str, Tamper); 6] = [
        ("missing", |_, p| fs::remove_file(p).unwrap()),
        ("loose", |_, p| {
            fs::set_permissions(p, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap()
        }),
        ("preexisting", |_, p| fs::write(p, "model\n").unwrap()),
        ("hardlink", |s, p| {
            fs::hard_link(p, s.0.join("alias")).unwrap()
        }),
        ("symlink", |s, p| {
            let real = s.0.join("real");
            fs::rename(p, &real).unwrap();
            symlink(&real, p).unwrap();
        }),
        ("invalid", |_, p| fs::write(p, "model\nmod").unwrap()),
    ];
    for (name, tamper) in cases {
        let s = Scratch::new(&format!("wd-unsafe-{name}"));
        let (m, progress) = watch_run(&s, &[("t1", "ok", "src/one.txt")], (5, 5), None);
        tamper(&s, &progress);
        let out = pilot(&s, "p1", &m);
        assert_eq!(
            code(&out),
            10,
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(failure(&s, "p1").0, "watchdog_unavailable", "{name}");
        assert!(
            !s.repo().join("src/one.txt").exists(),
            "{name}: the worker never ran"
        );
    }
    // A progress file inside the checkout is refused too.
    let s = Scratch::new("wd-inrepo");
    let (m, _) = watch_run(&s, &[("t1", "ok", "src/one.txt")], (5, 5), None);
    let inside = s.repo().join(".progress");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&inside)
        .unwrap();
    let mut v: Value = serde_json::from_slice(&fs::read(&m).unwrap()).unwrap();
    v["worker"]["watchdog"]["progress_file"] = json!(inside);
    fs::write(&m, v.to_string()).unwrap();
    // The untracked file makes the checkout dirty, which the engine refuses first; either way
    // the worker never ran. `Progress::open` has its own unit test for the location rule.
    assert_eq!(code(&pilot(&s, "p1", &m)), 10);
    assert!(["git_dirty", "watchdog_unavailable"].contains(&failure(&s, "p1").0.as_str()));
    assert!(!s.repo().join("src/one.txt").exists());
}

#[test]
fn each_task_gets_its_own_baseline_so_old_records_never_excuse_a_stall() {
    let s = Scratch::new("wd-baseline");
    let (m, _) = watch_run(
        &s,
        &[
            ("t1", "pulse", "src/one.txt"),
            ("t2", "stall", "docs/two.txt"),
        ],
        (1, 30),
        None,
    );
    let out = pilot(&s, "p1", &m);
    assert_eq!(code(&out), 10, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.commits(), 2, "t1 committed");
    let w = watchdog(&s, "p1");
    assert_eq!(w["task"], "t2");
    assert_eq!(
        (w["baseline_pulses"].as_u64(), w["model_pulses"].as_u64()),
        (Some(8), Some(0))
    );
    assert!(failure(&s, "p1").1.contains("startup stall"));
}

#[test]
fn identical_repeated_tool_results_over_unchanged_files_warn_but_never_stop() {
    let s = Scratch::new("wd-loop");
    let (m, _) = watch_run(&s, &[("t1", "loop", "src/one.txt")], (10, 10), None);
    let out = pilot(&s, "p1", &m);
    assert_eq!(
        code(&out),
        0,
        "advisory only: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let w = watchdog(&s, "p1");
    assert_eq!(w["tools_completed"], 4);
    assert_eq!(w["repeat_count"], 4);
    assert_eq!(w["last_tool"], "read");
    assert!(w["warning"].as_str().unwrap().contains("advisory only"));
    assert_eq!(w["tools_observed"], "completed_only");
}

#[test]
fn a_worker_stream_over_the_old_128_kib_cap_is_read_whole() {
    let s = Scratch::new("wd-big");
    let m = pilot_manifest(&s, "big|src/one.txt|x", "src/one.txt", "gate-no-fail");
    let out = pilot(&s, "p1", &m);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let log = s.status("p1")["run"]["done"][0]["gate_log"].clone();
    assert!(log.is_string());
    let logs = fs::read_dir(s.run_dir("p1").join("logs")).unwrap();
    let biggest = logs
        .map(|e| e.unwrap().metadata().unwrap().len())
        .max()
        .unwrap();
    assert!(biggest > 300 * 1024, "worker log kept {biggest} bytes");
}

#[test]
fn watchdog_manifest_rules() {
    let s = Scratch::new("wd-rules");
    let (good, _) = watch_run(&s, &[("t1", "ok", "src/one.txt")], (5, 5), None);
    let base: Value = serde_json::from_slice(&fs::read(&good).unwrap()).unwrap();
    let edits: [(&str, Edit); 6] = [
        ("neither", |v| {
            v["worker"].as_object_mut().unwrap().remove("watchdog");
        }),
        ("on gate", |v| {
            v["gate"]["watchdog"] = v["worker"]["watchdog"].clone();
        }),
        ("zero startup", |v| {
            v["worker"]["watchdog"]["startup_secs"] = json!(0)
        }),
        ("relative file", |v| {
            v["worker"]["watchdog"]["progress_file"] = json!("p")
        }),
        ("unknown key", |v| {
            v["worker"]["watchdog"]["tool_secs"] = json!(5)
        }),
        ("zero timeout", |v| v["worker"]["timeout_secs"] = json!(0)),
    ];
    for (name, edit) in edits {
        let mut v = base.clone();
        edit(&mut v);
        let p = s.0.join("bad.json");
        fs::write(&p, v.to_string()).unwrap();
        assert_eq!(code(&pilot(&s, "r1", &p)), 2, "{name}");
        assert!(!s.run_dir("r1").exists(), "{name}: no run created");
    }
}

// ---- `run OBJECTIVE --manifest M --run-id ID`: the same engine as pilot-run ----

fn chmod(m: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(m, fs::Permissions::from_mode(mode)).unwrap();
}

fn private(m: &Path) {
    chmod(m, 0o600);
}

fn run_manifest(s: &Scratch, m: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "run",
        "ignored objective",
        "--manifest",
        m.to_str().unwrap(),
    ];
    args.extend(extra);
    let repo = s.repo();
    args.extend(["--repo", repo.to_str().unwrap()]);
    s.lmloop(&args, None)
}

#[test]
fn run_with_a_private_manifest_uses_the_pilot_engine_and_its_labels() {
    let s = Scratch::new("runmanifest");
    let base = s.head();
    let m = pilot_manifest(&s, "ok|src/one.txt|first", "src/one.txt", "gate-no-fail");
    private(&m);
    let out = run_manifest(&s, &m, &["--run-id", "r1"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("objective is not sent"));
    assert_eq!(s.commits(), 2);
    assert_eq!(
        s.git(&["diff", "--name-only", &base, "HEAD"]),
        "src/one.txt"
    );
    assert_eq!(s.git(&["log", "-1", "--format=%s"]), "lmloop pilot: t1");
    assert_eq!(s.status("r1")["kind"], "serial_pilot");
    // One-shot: neither a repeat nor fixture-resume touches it.
    let head = s.head();
    assert_eq!(code(&run_manifest(&s, &m, &["--run-id", "r1"])), 2);
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 8);
    assert_eq!(s.head(), head);
}

#[test]
fn run_refuses_unsafe_manifest_use_and_creates_nothing() {
    let s = Scratch::new("runrefuse");
    let base = s.head();
    let m = pilot_manifest(&s, "ok|src/one.txt|a", "src/one.txt", "gate-no-fail");
    let refused = |out: Output, want: i32, why: &str| {
        assert_eq!(
            code(&out),
            want,
            "{why}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!s.run_dir("r1").exists(), "{why}");
    };
    let repo = s.repo();
    let objective_only = s.lmloop(&["run", "x", "--repo", repo.to_str().unwrap()], None);
    refused(objective_only, 69, "bare objective, no manifest");
    chmod(&m, 0o644);
    refused(run_manifest(&s, &m, &["--run-id", "r1"]), 2, "mode 0644");
    private(&m);
    refused(
        run_manifest(&s, &m, &["--run-id", "r1", "--dry-run"]),
        2,
        "dry-run",
    );
    refused(
        run_manifest(&s, &m, &["--run-id", "r1", "--detach"]),
        2,
        "detach",
    );
    refused(
        run_manifest(&s, &m, &["--run-id", "r1", "--max-iterations", "3"]),
        2,
        "bound",
    );
    refused(run_manifest(&s, &m, &[]), 2, "no run id");
    refused(run_manifest(&s, &m, &["--run-id", "bad id"]), 2, "bad id");
    // Environment or default state root does not count as explicit.
    let env_root = Command::new(env!("CARGO_BIN_EXE_lmloop"))
        .env("LMLOOP_RUST_STATE_ROOT", s.0.join("state"))
        .env("HOME", &s.0)
        .args([
            "run",
            "x",
            "--manifest",
            m.to_str().unwrap(),
            "--run-id",
            "r1",
            "--repo",
        ])
        .arg(&repo)
        .output()
        .unwrap();
    refused(env_root, 2, "environment state root");
    // A manifest inside the repo, or naming a different repo, is refused.
    let inside = repo.join("m.json");
    fs::copy(&m, &inside).unwrap();
    private(&inside);
    refused(
        run_manifest(&s, &inside, &["--run-id", "r1"]),
        2,
        "manifest inside repo",
    );
    fs::remove_file(&inside).unwrap();
    let other = Scratch::new("runrefuse-other");
    let other_repo = other.repo();
    let out = other.lmloop(
        &[
            "run",
            "x",
            "--manifest",
            m.to_str().unwrap(),
            "--run-id",
            "r1",
            "--repo",
            other_repo.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(code(&out), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not the repo"));
    assert_eq!(s.head(), base);
    assert_eq!(s.commits(), 1);
}

/// `fixture-run`/`fixture-resume` against the scratch repo with another state root.
fn other_root(s: &Scratch, root: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lmloop"))
        .env_remove("LMLOOP_RUST_STATE_ROOT")
        .arg("--state-root")
        .arg(s.0.join(root))
        .args(args)
        .current_dir(&s.0)
        .output()
        .unwrap()
}

fn checkout_guard(s: &Scratch) -> Option<lmloop::lease::Lease> {
    let git_dir = lmloop::git::checkout_dir(&s.repo()).unwrap().unwrap();
    lmloop::lease::claim_checkout(&git_dir).unwrap()
}

#[test]
fn a_held_checkout_refuses_a_fresh_run_from_another_root_and_frees_on_release() {
    let s = Scratch::new("co-fresh");
    let base = s.head();
    let m = s.manifest(&[("t1", "ok|src/one.txt|first")], 30);
    let held = checkout_guard(&s).expect("free checkout");
    assert!(checkout_guard(&s).is_none(), "second claim must lose");

    let args = ["fixture-run", "r1", "--manifest", m.to_str().unwrap()];
    let out = other_root(&s, "state2", &args);
    assert_eq!(code(&out), 7, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Git checkout"));
    assert!(!s.0.join("state2/v1/runs/r1").exists(), "loser published");
    assert!(!s.repo().join("src").exists());
    assert_eq!(s.head(), base);

    drop(held);
    let out = other_root(&s, "state2", &args);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.commits(), 2);
    assert!(checkout_guard(&s).is_some(), "the finished run released it");
}

#[test]
fn a_held_checkout_stops_a_writing_resume_but_not_a_read_only_one() {
    let s = Scratch::new("co-resume");
    let first = ctl_prompt(&s, "ctl", "src/one.txt", "PAUSE");
    let m = s.manifest(&[("t1", &first), ("t2", "ok|docs/two.txt|b")], 30);
    assert_eq!(code(&s.run("r1", &m, None)), 12);
    let (head, run_json) = (
        s.head(),
        fs::read(s.run_dir("r1").join("run.json")).unwrap(),
    );
    let log = fs::read(log_path(&s)).unwrap();

    let held = checkout_guard(&s).unwrap();
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 7);
    assert_eq!(s.head(), head);
    assert_eq!(
        fs::read(s.run_dir("r1").join("run.json")).unwrap(),
        run_json
    );
    assert_eq!(fs::read(log_path(&s)).unwrap(), log, "journal untouched");

    drop(held);
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 0);
    assert_eq!(s.commits(), 3);

    // Complete is a read-only answer: it needs no checkout.
    let held = checkout_guard(&s).unwrap();
    assert_eq!(code(&s.lmloop(&["fixture-resume", "r1"], None)), 0);
    drop(held);
}

#[test]
fn checkout_guard_is_per_worktree_and_refuses_unsafe_lock_files() {
    let s = Scratch::new("co-unsafe");
    let linked = s.0.join("linked");
    s.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "other",
        linked.to_str().unwrap(),
    ]);
    let main = checkout_guard(&s).unwrap();
    let git_dir = lmloop::git::checkout_dir(&linked).unwrap().unwrap();
    assert_ne!(
        git_dir,
        lmloop::git::checkout_dir(&s.repo()).unwrap().unwrap()
    );
    let other = lmloop::lease::claim_checkout(&git_dir).unwrap();
    assert!(other.is_some(), "a linked worktree is independent");
    drop((main, other));

    // A pre-existing lock file is reused, never unlinked; links are refused.
    let lock = lmloop::git::checkout_dir(&s.repo())
        .unwrap()
        .unwrap()
        .join("lmloop-checkout.lock");
    let ino = |p: &Path| std::os::unix::fs::MetadataExt::ino(&fs::metadata(p).unwrap());
    let before = ino(&lock);
    drop(checkout_guard(&s).unwrap());
    assert_eq!(ino(&lock), before);
    let alias = s.0.join("alias");
    fs::hard_link(&lock, &alias).unwrap();
    let git_dir = lmloop::git::checkout_dir(&s.repo()).unwrap().unwrap();
    let err = lmloop::lease::claim_checkout(&git_dir).unwrap_err();
    assert_eq!(err.kind, "unsafe_path");
    fs::remove_file(&alias).unwrap();
    fs::remove_file(&lock).unwrap();
    std::os::unix::fs::symlink(s.0.join("elsewhere"), &lock).unwrap();
    let err = lmloop::lease::claim_checkout(&git_dir).unwrap_err();
    assert_eq!(err.kind, "unsafe_path");
    assert!(
        !s.0.join("elsewhere").exists(),
        "symlink target was created"
    );
}

#[test]
fn checkout_lookup_failure_is_an_error_and_only_a_non_repo_is_unguarded() {
    let s = Scratch::new("co-lookup");
    let plain = s.0.join("plain");
    fs::create_dir_all(&plain).unwrap();
    assert!(lmloop::git::checkout_dir(&plain).unwrap().is_none());
    let err = lmloop::git::checkout_dir(&s.0.join("missing")).unwrap_err();
    assert_eq!(err.kind, "git");
}

#[test]
fn a_committing_resume_whose_checkout_lookup_fails_changes_nothing() {
    let s = Scratch::new("co-commit-lookup");
    let m = s.manifest(
        &[("t1", "ok|src/one.txt|a"), ("t2", "ok|docs/two.txt|b")],
        30,
    );
    assert_eq!(code(&s.run("r1", &m, Some("after_cas"))), 86);
    assert_eq!(s.status("r1")["run"]["phase"], "committing");
    let (head, run_json) = (
        s.head(),
        fs::read(s.run_dir("r1").join("run.json")).unwrap(),
    );
    let log = fs::read(log_path(&s)).unwrap();

    let gone = s.0.join("repo-gone");
    fs::rename(s.repo(), &gone).unwrap();
    let out = s.lmloop(&["fixture-resume", "r1"], None);
    fs::rename(&gone, s.repo()).unwrap();
    assert_ne!(code(&out), 0, "resumed without the checkout guard");
    assert!(String::from_utf8_lossy(&out.stderr).contains("git"));
    assert_eq!(s.head(), head);
    assert_eq!(
        fs::read(s.run_dir("r1").join("run.json")).unwrap(),
        run_json
    );
    assert_eq!(fs::read(log_path(&s)).unwrap(), log, "journal untouched");
}

#[test]
fn the_pilot_hook_runs_under_the_checkout_guard_and_a_refusal_publishes_nothing() {
    let s = Scratch::new("co-hook");
    let m = s.manifest(&[("t1", "ok|src/one.txt|a")], 30);
    let policy = || lmloop::serial::load_pilot_manifest(&m).unwrap();
    let store = lmloop::state::Store::new(&s.0.join("state"));
    let git_dir = lmloop::git::checkout_dir(&s.repo()).unwrap().unwrap();

    // The hook sees the guard held (a second claim loses) and the run directory absent; its Err
    // is the launch's result, with no run directory, worker or commit.
    let head = s.head();
    let mut ran = false;
    let err = match lmloop::serial::start_pilot_guarded(&store, "r1", policy(), |_| {
        ran = true;
        assert!(lmloop::lease::claim_checkout(&git_dir).unwrap().is_none());
        assert!(!s.0.join("state/v1/runs/r1").exists());
        Err(lmloop::state::Error::new("ineligible", "no"))
    }) {
        Err(e) => e,
        Ok(_) => panic!("a refused hook launched"),
    };
    assert!(ran);
    assert_eq!(err.kind, "ineligible");
    assert!(!s.0.join("state/v1/runs/r1").exists());
    assert_eq!(s.head(), head);
    assert!(
        lmloop::lease::claim_checkout(&git_dir).unwrap().is_some(),
        "the guard outlived a refused launch"
    );

    // A busy checkout never reaches the hook.
    let held = checkout_guard(&s).unwrap();
    let out = lmloop::serial::start_pilot_guarded(&store, "r2", policy(), |_| panic!("hook ran"));
    assert!(matches!(out, Ok(lmloop::serial::Finish::CheckoutBusy)));
    drop(held);

    // An accepting hook lets the run go on.
    let out = lmloop::serial::start_pilot_guarded(&store, "r3", policy(), |_| Ok(())).unwrap();
    assert!(matches!(out, lmloop::serial::Finish::Complete));
}
