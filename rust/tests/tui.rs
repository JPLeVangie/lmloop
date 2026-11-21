//! Operator console: fixed-size buffer renders, key-driven behaviour against a
//! real fixture run, and a PTY run of the real binary.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lmloop::tui::{App, Scope, draw};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let base = std::env::var_os("LMLOOP_TEST_TMP")
            .map_or_else(|| PathBuf::from("/tmp/opencode"), PathBuf::from);
        let dir = base.join(format!("lmloop-tui-{name}-{}", std::process::id()));
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

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(self.0.join("repo"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    }

    fn state(&self) -> PathBuf {
        self.0.join("state")
    }

    fn repo(&self) -> PathBuf {
        fs::canonicalize(self.0.join("repo")).unwrap()
    }

    fn out(&self, args: &[&str]) -> String {
        let o = Command::new("git")
            .current_dir(self.0.join("repo"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// A pilot manifest outside the repo: the fixture worker installed as `bin/opencode`.
    fn pilot_manifest(&self, name: &str, prompt: &str) -> PathBuf {
        let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        fs::create_dir_all(self.0.join("bin")).unwrap();
        fs::copy(fx.join("fake-opencode"), self.0.join("bin/opencode")).unwrap();
        let m = json!({
            "manifest_version": 1,
            "repo": self.repo(),
            "worker": { "argv": [self.0.join("bin/opencode")], "timeout_secs": 30 },
            "gate": { "argv": [fx.join("gate-no-fail")], "timeout_secs": 30 },
            "tasks": [{ "id": "t1", "prompt": prompt, "scopes": ["src/one.txt"] }],
        });
        let path = self.0.join(name);
        write_private(&path, &m.to_string());
        path
    }

    /// A two-task fixture run whose first worker drops PAUSE: ends `paused` after task one.
    fn start_paused(&self, id: &str) {
        let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let pause = self.state().join(format!("v1/runs/{id}/PAUSE"));
        let manifest = json!({
            "manifest_version": 1,
            "repo": self.0.join("repo"),
            "worker": { "argv": [fx.join("fake-opencode")], "timeout_secs": 30 },
            "gate": { "argv": [fx.join("gate-no-fail")], "timeout_secs": 30 },
            "tasks": [
                { "id": "t1", "prompt": format!("ctl|src/one.txt|{}", pause.display()), "scopes": ["src/one.txt"] },
                { "id": "t2", "prompt": "ok|docs/two.txt|second", "scopes": ["docs/two.txt"] },
            ],
        });
        let path = self.0.join("manifest.json");
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
}

/// The TUI only loads owner-only (0600) manifests.
fn write_private(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn render(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(f, app)).unwrap();
    let buf = term.backend().buffer().clone();
    (0..h)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
        .collect()
}

fn text(app: &App, w: u16, h: u16) -> String {
    render(app, w, h).join("\n")
}

fn key(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn ctrl_y(app: &mut App) {
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
}

fn keys(app: &mut App, s: &str) {
    for c in s.chars() {
        key(app, KeyCode::Char(c));
    }
}

fn wait_resume(app: &mut App) {
    let t = Instant::now();
    while app.is_resuming() {
        assert!(t.elapsed() < Duration::from_secs(60), "resume never ended");
        std::thread::sleep(Duration::from_millis(50));
        app.tick();
    }
}

#[test]
fn renders_wide_and_narrow_and_survives_tiny() {
    let s = Scratch::new("render");
    s.start_paused("r1");
    let mut app = App::new(&s.state());
    app.refresh();

    let wide = text(&app, 120, 30);
    for want in [
        "Runs (1)",
        "r1 paused 1/2",
        "R resume: continues",
        "ok t1",
        "-- t2",
        "run_paused",
    ] {
        assert!(wide.contains(want), "wide missing {want:?}:\n{wide}");
    }

    // Narrow: the list alone, then the detail alone; every line fits.
    let narrow = render(&app, 50, 20);
    assert!(narrow.join("\n").contains("r1 paused 1/2"));
    assert!(!narrow.join("\n").contains("actions"));
    key(&mut app, KeyCode::Enter);
    let detail = text(&app, 50, 20);
    assert!(
        detail.contains("actions") && !detail.contains("Runs (1)"),
        "{detail}"
    );
    key(&mut app, KeyCode::Esc);
    assert!(text(&app, 50, 20).contains("Runs (1)"));

    keys(&mut app, "?");
    assert!(text(&app, 50, 20).contains("Keys"));
    key(&mut app, KeyCode::Char('x'));
    for (w, h) in [(1, 1), (10, 3), (20, 6), (30, 8)] {
        render(&app, w, h);
        keys(&mut app, "?");
        render(&app, w, h);
        key(&mut app, KeyCode::Esc);
    }
}

#[test]
fn keys_drive_resume_off_thread_logs_and_final_decision() {
    let s = Scratch::new("keys");
    s.start_paused("r1");
    let mut app = App::new(&s.state());
    app.refresh();

    // Unavailable actions are refused with the engine's reason, not offered.
    keys(&mut app, "s");
    assert!(app.status.contains("not available"), "{}", app.status);
    keys(&mut app, "a");
    assert!(app.status.contains("not complete"), "{}", app.status);

    // Logs: the committed task's gate log is readable and bounded.
    keys(&mut app, "2");
    assert!(
        text(&app, 100, 24).contains(".log"),
        "{}",
        text(&app, 100, 24)
    );
    keys(&mut app, "1");

    // Resume returns at once; the outcome arrives through tick().
    keys(&mut app, "R");
    assert!(app.status.starts_with("resuming r1"), "{}", app.status);
    keys(&mut app, "q");
    assert!(!app.quit, "quit must wait for the in-process resume");
    wait_resume(&mut app);
    assert!(app.status.contains("complete"), "{}", app.status);

    // Approve needs no note, records once, and is then unavailable.
    keys(&mut app, "c");
    assert!(text(&app, 100, 24).contains("Request changes"));
    key(&mut app, KeyCode::Enter); // empty note is refused and kept open
    assert!(app.status.contains("not recorded"), "{}", app.status);
    keys(&mut app, "tidy up");
    key(&mut app, KeyCode::Enter);
    assert!(app.status.contains("request_changes"), "{}", app.status);
    assert!(text(&app, 100, 24).contains("ChangesRequested - tidy up"));
    keys(&mut app, "a");
    assert!(app.status.contains("not available"), "{}", app.status);
    assert!(s.state().join("v1/runs/r1/decision.json").exists());

    keys(&mut app, "q");
    assert!(app.quit);
}

#[test]
fn irreversible_stop_needs_confirmation() {
    let s = Scratch::new("stop");
    s.start_paused("r1");
    // Make the run look live: hold its lease and put it mid-task.
    let dir = s.state().join("v1/runs/r1");
    let run = fs::read_to_string(dir.join("run.json")).unwrap();
    fs::write(
        dir.join("run.json"),
        run.replace("\"phase\": \"paused\"", "\"phase\": \"worker\""),
    )
    .unwrap();
    let lease = lmloop::lease::claim(&dir, "test").unwrap();
    assert!(matches!(lease, lmloop::lease::Claim::Won(_)));
    let mut app = App::new(&s.state());
    app.refresh();

    // Pause is a single keystroke: it only parks at a boundary.
    keys(&mut app, "p");
    assert!(dir.join("PAUSE").exists(), "{}", app.status);

    keys(&mut app, "S");
    assert!(text(&app, 100, 24).contains("STOP NOW run r1?"));
    keys(&mut app, "n");
    assert!(!dir.join("STOP-NOW").exists(), "cancel must write nothing");
    keys(&mut app, "S");
    keys(&mut app, "y");
    assert!(dir.join("STOP-NOW").exists());
    assert!(!dir.join("STOP").exists());
}

#[test]
fn pty_quit_restores_terminal_and_leaves_state_alone() {
    let s = Scratch::new("pty");
    s.start_paused("r1");
    let script = r#"
import fcntl, os, pty, select, struct, sys, termios, time
state, binary = sys.argv[1:3]
pid, fd = pty.fork()
if pid == 0:
    os.execv(binary, [binary, "--state-root", state, "tui"])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
def pump(seconds):
    out, end = b"", time.time() + seconds
    while time.time() < end:
        if select.select([fd], [], [], 0.1)[0]:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            out += data
    return out
out = pump(1.5)
os.write(fd, b"?")
out += pump(0.5)
os.write(fd, b"x")
os.write(fd, b"q")
out += pump(1.5)
_, status = os.waitpid(pid, 0)
assert os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0, status
assert b"r1" in out and b"Keys" in out, out[-400:]
assert b"\x1b[?1049h" in out and b"\x1b[?1049l" in out, "alternate screen not entered and left"
print("ok")
"#;
    let out = Command::new("python3")
        .args(["-c", script])
        .arg(s.state())
        .arg(env!("CARGO_BIN_EXE_lmloop"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let run = fs::read_to_string(s.state().join("v1/runs/r1/run.json")).unwrap();
    assert!(run.contains("\"paused\""), "quit changed run state");
    assert!(!s.state().join("v1/runs/r1/STOP").exists());
}

/// Open the launch form and type a manifest path and run id, ending on Enter.
fn fill_launch(app: &mut App, manifest: &Path, id: &str) {
    keys(app, "N");
    if app.status.starts_with("N needs") {
        return; // refused up front: the rest would be typed as hotkeys
    }
    keys(app, &manifest.display().to_string());
    key(app, KeyCode::Enter);
    keys(app, id);
    key(app, KeyCode::Enter);
}

#[test]
fn git_toplevel_is_read_only_canonical_and_per_worktree() {
    let s = Scratch::new("toplevel");
    let repo = s.repo();
    fs::create_dir_all(repo.join("a/b")).unwrap();
    assert_eq!(lmloop::git::toplevel(&repo.join("a/b")), Some(repo.clone()));
    // Dirty and detached checkouts still resolve.
    fs::write(repo.join("README"), "dirty\n").unwrap();
    s.git(&["checkout", "-q", "--detach"]);
    assert_eq!(lmloop::git::toplevel(&repo.join("a")), Some(repo.clone()));
    // A linked worktree is its own checkout.
    let linked = s.0.join("linked");
    s.git(&["worktree", "add", "-q", linked.to_str().unwrap()]);
    let linked = fs::canonicalize(linked).unwrap();
    assert_eq!(lmloop::git::toplevel(&linked), Some(linked.clone()));
    assert_ne!(Some(linked), lmloop::git::toplevel(&repo));
    // No repository (and a missing directory) is None, never a fallback.
    assert_eq!(lmloop::git::toplevel(&s.0), None);
    assert_eq!(lmloop::git::toplevel(&s.0.join("missing")), None);
}

#[test]
fn scope_lists_only_the_exact_checkout() {
    let s = Scratch::new("scope");
    s.start_paused("r1");
    fs::create_dir_all(s.state().join("v1/runs/zz")).unwrap(); // unreadable run
    let linked = s.0.join("linked");
    s.git(&["worktree", "add", "-q", linked.to_str().unwrap()]);
    let show = |scope: Scope| {
        let mut app = App::scoped(&s.state(), scope, true);
        app.refresh();
        text(&app, 120, 20)
    };
    let own = show(Scope::Repo(s.repo()));
    assert!(
        own.contains("Runs (1)") && own.contains("r1 paused"),
        "{own}"
    );
    assert!(
        own.contains(&format!("repo: {}", s.repo().display())),
        "{own}"
    );
    assert!(own.contains("N launch pilot"), "{own}");
    let other = show(Scope::Repo(fs::canonicalize(linked).unwrap()));
    assert!(
        other.contains("Runs (0)") && !other.contains("r1"),
        "{other}"
    );
    assert!(other.contains("no runs for this checkout"), "{other}");
    let none = show(Scope::NoRepo);
    assert!(
        none.contains("Runs (0)") && none.contains("not inside a Git repository"),
        "{none}"
    );
    let all = show(Scope::All);
    assert!(
        all.contains("Runs (2)") && all.contains("r1 paused"),
        "{all}"
    );
}

#[test]
fn n_reviews_the_loaded_policy_and_launches_exactly_it() {
    let s = Scratch::new("launch");
    let base = s.out(&["rev-parse", "HEAD"]);
    let m = s.pilot_manifest("m.json", "ok|src/one.txt|first");
    let mut app = App::scoped(&s.state(), Scope::Repo(s.repo()), true);
    app.refresh();
    fill_launch(&mut app, &m, "p1");
    let review = text(&app, 220, 40);
    for want in [
        "PILOT: one task, one shot, not production verification",
        &format!("repo: {}", s.repo().display()),
        &s.state().display().to_string(),
        "run id: p1",
        &m.display().to_string(),
        &s.0.join("bin/opencode").display().to_string(),
        "timeout 30s",
        "task t1 scopes [\"src/one.txt\"]",
        "ok|src/one.txt|first",
        "Ctrl-Y launches this exact policy",
    ] {
        assert!(review.contains(want), "review missing {want:?}:\n{review}");
    }
    assert!(
        !s.state().join("v1/runs/p1").exists(),
        "review must start nothing"
    );

    // The manifest is not reread: swap in one that would fail the gate; the reviewed one runs.
    fs::write(&m, "not json").unwrap();
    ctrl_y(&mut app);
    assert!(
        app.status.starts_with("launching pilot p1"),
        "{}",
        app.status
    );
    keys(&mut app, "q");
    assert!(!app.quit, "quit must wait for the in-process launch");
    wait_resume(&mut app);
    assert!(app.status.contains("launch p1: complete"), "{}", app.status);
    assert!(app.status.contains("pilot evidence"), "{}", app.status);
    assert_eq!(s.out(&["rev-list", "--count", "HEAD"]), "2");
    assert_eq!(s.out(&["log", "-1", "--format=%s"]), "lmloop pilot: t1");
    assert_eq!(
        s.out(&["diff", "--name-only", &base, "HEAD"]),
        "src/one.txt"
    );
    assert!(text(&app, 120, 20).contains("p1 complete 1/1"));
    let run = fs::read_to_string(s.state().join("v1/runs/p1/run.json")).unwrap();
    assert!(run.contains("\"pilot\": true"), "{run}");
}

#[test]
fn launch_cancel_gate_failure_and_refusals_start_nothing_wrong() {
    // Cancel at the review: no run, no commit.
    let s = Scratch::new("cancel");
    let base = s.out(&["rev-parse", "HEAD"]);
    let m = s.pilot_manifest("m.json", "ok|src/one.txt|first");
    let mut app = App::scoped(&s.state(), Scope::Repo(s.repo()), true);
    fill_launch(&mut app, &m, "p1");
    keys(&mut app, "n");
    assert!(app.status.contains("nothing started"), "{}", app.status);
    assert!(!s.state().join("v1/runs/p1").exists());
    assert_eq!(s.out(&["rev-parse", "HEAD"]), base);

    // A failing operator gate: task_failed, nothing committed, the work preserved.
    let bad = s.pilot_manifest("bad.json", "ok|src/one.txt|FAIL here");
    fill_launch(&mut app, &bad, "p2");
    ctrl_y(&mut app);
    wait_resume(&mut app);
    assert!(app.status.contains("task_failed"), "{}", app.status);
    assert_eq!(s.out(&["rev-parse", "HEAD"]), base);
    assert_eq!(
        fs::read_to_string(s.repo().join("src/one.txt")).unwrap(),
        "FAIL here\n"
    );

    // Refusals keep the form open with the typed text and write nothing.
    let s = Scratch::new("refuse");
    fs::create_dir_all(s.repo().join("sub")).unwrap();
    let inside = s.repo().join("in.json");
    fs::copy(s.pilot_manifest("m.json", "ok|src/one.txt|a"), &inside).unwrap();
    let sub = {
        let mut v: serde_json::Value =
            serde_json::from_slice(&fs::read(s.0.join("m.json")).unwrap()).unwrap();
        v["repo"] = json!(s.repo().join("sub"));
        let p = s.0.join("sub.json");
        write_private(&p, &v.to_string());
        p
    };
    let good = s.0.join("m.json");
    let attempt = |scope: Scope, explicit: bool, root: &Path, m: &Path, id: &str| {
        let mut app = App::scoped(root, scope, explicit);
        fill_launch(&mut app, m, id);
        app
    };
    let st = s.state();
    for (app, why) in [
        (
            attempt(Scope::Repo(s.repo()), false, &st, &good, "p1"),
            "explicit --state-root",
        ),
        (
            attempt(Scope::All, true, &st, &good, "p1"),
            "needs a Git checkout",
        ),
        (
            attempt(Scope::NoRepo, true, &st, &good, "p1"),
            "needs a Git checkout",
        ),
        (
            attempt(Scope::Repo(s.repo()), true, &st, Path::new("m.json"), "p1"),
            "absolute",
        ),
        (
            attempt(Scope::Repo(s.repo()), true, &st, &inside, "p1"),
            "outside the repository",
        ),
        (
            attempt(Scope::Repo(s.repo()), true, &st, &sub, "p1"),
            "is not this checkout",
        ),
        (
            attempt(Scope::Repo(s.repo()), true, &st, &good, "bad id!"),
            "run id",
        ),
        (
            attempt(
                Scope::Repo(s.repo()),
                true,
                &s.repo().join("sub"),
                &good,
                "p1",
            ),
            "overlap",
        ),
    ] {
        assert!(app.status.contains(why), "want {why:?}: {}", app.status);
        assert!(!text(&app, 160, 30).contains("Review PILOT"), "{why}");
        assert!(!s.state().join("v1/runs/p1").exists(), "{why}");
    }
    assert!(!s.repo().join("src/one.txt").exists());
}

/// Drive the real binary on a PTY. `args` follow the binary; each step is `[keys, pause_secs]`.
/// After the steps it sends Esc then `q` until the process exits; prints the screen bytes.
const PTY: &str = r#"
import fcntl, json, os, pty, select, struct, sys, termios, time
cwd, binary, args, steps = sys.argv[1:5]
pid, fd = pty.fork()
if pid == 0:
    os.chdir(cwd)
    os.environ.pop("LMLOOP_RUST_STATE_ROOT", None)
    os.execv(binary, [binary] + json.loads(args))
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 200, 0, 0))
out = b""
def pump(seconds):
    global out
    end = time.time() + seconds
    while time.time() < end:
        if select.select([fd], [], [], 0.05)[0]:
            try:
                data = os.read(fd, 65536)
            except OSError:
                return
            out += data
pump(1.5)
for keys, pause in json.loads(steps):
    os.write(fd, keys.encode())
    pump(pause)
status = None
for _ in range(80):
    done, st = os.waitpid(pid, os.WNOHANG)
    if done:
        status = st
        break
    os.write(fd, b"\x1b")  # leave any open form first; a separate write, so not Alt-q
    pump(0.25)
    os.write(fd, b"q")
    pump(0.25)
assert status is not None and os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0, status
sys.stdout.write(out.decode("utf-8", "replace"))
"#;

fn pty(cwd: &Path, args: &[&str], steps: serde_json::Value) -> String {
    let out = Command::new("python3")
        .args(["-c", PTY])
        .arg(cwd)
        .arg(env!("CARGO_BIN_EXE_lmloop"))
        .arg(json!(args).to_string())
        .arg(steps.to_string())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn pty_bare_lmloop_scopes_to_the_cwd_checkout() {
    let s = Scratch::new("ptyscope");
    s.start_paused("r1");
    let state = s.state().display().to_string();
    let sub = s.repo().join("deep/er");
    fs::create_dir_all(&sub).unwrap();
    let other = s.0.join("other/sub");
    fs::create_dir_all(&other).unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .arg(s.0.join("other"))
        .output()
        .unwrap();
    assert!(init.status.success());

    // From a subdirectory: the checkout's run, its repo and the launch hint.
    let own = pty(&sub, &["--state-root", &state], json!([]));
    assert!(own.contains(&s.repo().display().to_string()), "{own}");
    assert!(own.contains("r1") && own.contains("launch"), "{own}");
    // Another repo and a non-repo directory never see it; no global fallback.
    let theirs = pty(&other, &["--state-root", &state], json!([]));
    assert!(
        theirs.contains("other") && !theirs.contains("r1"),
        "{theirs}"
    );
    fs::create_dir_all(s.0.join("plain")).unwrap();
    let none = pty(&s.0.join("plain"), &["--state-root", &state], json!([]));
    assert!(none.contains("Git") && !none.contains("r1"), "{none}");
    // Viewing never writes: the run is untouched.
    assert!(s.state().join("v1/runs/r1/run.json").exists());
}

#[test]
fn pty_n_launches_a_fake_worker_through_the_gated_engine() {
    let s = Scratch::new("ptylaunch");
    let base = s.out(&["rev-parse", "HEAD"]);
    let m = s.pilot_manifest("m.json", "ok|src/one.txt|first");
    let state = s.state().display().to_string();
    let sub = s.repo().join("sub");
    fs::create_dir_all(&sub).unwrap();
    let go = |m: &Path, id: &str, confirm: &str| {
        json!([
            ["N", 0.4],
            [m.display().to_string(), 0.3],
            ["\r", 0.3],
            [id, 0.3],
            ["\r", 0.6],
            [confirm, 3.0]
        ])
    };

    // A bare `y` (what pasted form text would end in) and a bracketed paste launch nothing.
    let paste = |t: &str| format!("\x1b[200~{t}\x1b[201~");
    let steps = json!([
        ["N", 0.4],
        [paste(&format!("{}\ny", m.display())), 0.3],
        ["\r", 0.3],
        [paste("c0\ny\ny"), 0.3],
        ["\r", 0.6],
        [paste("y\n\x19"), 0.5],
        ["y", 2.0]
    ]);
    pty(&sub, &["--state-root", &state], steps);
    assert!(!s.state().join("v1/runs/c0").exists());
    assert_eq!(s.out(&["rev-parse", "HEAD"]), base);

    // Cancel at review: nothing is launched.
    let out = pty(&sub, &["--state-root", &state], go(&m, "c1", "n"));
    assert!(out.contains("PILOT"), "{out}");
    assert!(!s.state().join("v1/runs/c1").exists());
    assert_eq!(s.out(&["rev-parse", "HEAD"]), base);

    // Mismatch (the manifest's repo is another checkout): refused before any run.
    let other = s.0.join("other");
    Command::new("git")
        .args(["init", "-q"])
        .arg(&other)
        .output()
        .unwrap();
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&m).unwrap()).unwrap();
    v["repo"] = json!(fs::canonicalize(&other).unwrap());
    let mm = s.0.join("mismatch.json");
    write_private(&mm, &v.to_string());
    pty(&sub, &["--state-root", &state], go(&mm, "x1", "\x19"));
    assert!(!s.state().join("v1/runs/x1").exists());
    assert_eq!(s.out(&["rev-parse", "HEAD"]), base);

    // Confirm: the fake worker runs, the gate passes, one scoped commit lands.
    pty(&sub, &["--state-root", &state], go(&m, "p1", "\x19"));
    assert_eq!(s.out(&["rev-list", "--count", "HEAD"]), "2");
    assert_eq!(
        s.out(&["diff", "--name-only", &base, "HEAD"]),
        "src/one.txt"
    );
    assert_eq!(s.out(&["log", "-1", "--format=%s"]), "lmloop pilot: t1");
    let run = fs::read_to_string(s.state().join("v1/runs/p1/run.json")).unwrap();
    assert!(
        run.contains("\"pilot\": true") && run.contains("\"complete\""),
        "{run}"
    );
}

#[test]
fn review_is_private_canonical_and_shows_invisible_characters() {
    let s = Scratch::new("hardening");
    let m = s.pilot_manifest("m.json", "ok|src/one.txt|a\u{202e}b\u{200b}c");
    let go = |m: &Path| {
        let mut app = App::scoped(&s.state(), Scope::Repo(s.repo()), true);
        fill_launch(&mut app, m, "p1");
        app
    };

    // A plain `y` never launches; the review escapes bidi and zero-width characters.
    let mut app = go(&m);
    let review = text(&app, 220, 40);
    assert!(
        review.contains("\\u{202e}") && review.contains("\\u{200b}"),
        "{review}"
    );
    assert!(!review.contains('\u{202e}') && !review.contains('\u{200b}'));
    keys(&mut app, "y");
    assert!(app.status.contains("nothing started"), "{}", app.status);
    assert!(!s.state().join("v1/runs/p1").exists());

    // A paste fills the launch form but never confirms or advances it.
    let mut app = App::scoped(&s.state(), Scope::Repo(s.repo()), true);
    keys(&mut app, "N");
    app.handle_paste(&format!("{}\n", m.display()));
    assert!(text(&app, 220, 40).contains("Launch PILOT"));
    key(&mut app, KeyCode::Enter);
    app.handle_paste("p1\ny");
    key(&mut app, KeyCode::Enter);
    app.handle_paste("y\ny");
    assert!(text(&app, 220, 40).contains("Review PILOT"));
    assert!(!app.is_resuming() && !s.state().join("v1/runs/p1").exists());

    // Group/world-accessible, FIFO and dangling manifests are refused without blocking.
    let writable = s.0.join("writable.json");
    fs::copy(&m, &writable).unwrap();
    fs::set_permissions(&writable, fs::Permissions::from_mode(0o666)).unwrap();
    let fifo = s.0.join("fifo.json");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    for (bad, why) in [(&writable, "mode 0600"), (&fifo, "regular file")] {
        let app = go(bad);
        assert!(app.status.contains(why), "{why}: {}", app.status);
        assert!(!text(&app, 160, 30).contains("Review PILOT"), "{why}");
    }
    // A symlink is followed once, by canonicalising; the canonical file is what is shown.
    let link = s.0.join("link.json");
    std::os::unix::fs::symlink(&m, &link).unwrap();
    let app = go(&link);
    assert!(text(&app, 220, 40).contains(&fs::canonicalize(&m).unwrap().display().to_string()));
}
