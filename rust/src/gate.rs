//! Diagnostic gate: run an operator-supplied argv (no shell) with stdin null,
//! an allowlisted environment, a wall-clock cap, process-group teardown and
//! bounded output.
//!
//! Containment boundary: the gate leads its own process group and that group
//! is killed on exit, timeout or signal. A descendant that changes group
//! (`setsid`) escapes it, and if this process itself is SIGKILLed nothing
//! tears the group down; the run is then reported as an owner-lost
//! `gate_running` snapshot. Neither case is claimed to be contained.

use crate::state::{self, Error, Result, io_err};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Kept per stream; the rest is read and discarded so the child never blocks.
pub const MAX_STREAM_BYTES: usize = 128 * 1024;
/// How long, in total, to wait for output pipes to close after the leader is gone.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// The only variables a gate inherits; everything else (tokens, keys) is dropped.
const ENV_ALLOWLIST: [&str; 10] = [
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR", "TERM",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Ran and exited 0. Diagnostic only; see `state::EVIDENCE`.
    Passed,
    /// No gate configured. Not a pass, and not evidence of anything.
    Absent,
    /// Ran and exited non-zero (other than 127) or died on a signal. Blocking.
    Failed,
    /// Executable not found, or exit 127. Blocking.
    Missing,
    /// Hit the time cap and was torn down. Blocking.
    TimedOut,
    /// The supervisor received SIGTERM/SIGINT/SIGHUP and tore the gate down. Blocking.
    Interrupted,
    /// The cancel file (operator STOP-NOW) appeared and the group was torn down. Blocking.
    Cancelled,
}

/// argv is operator-sensitive, so only the program and the argument count are kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRecord {
    pub outcome: Outcome,
    pub program: Option<String>,
    pub argv_len: usize,
    pub cwd: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timeout_secs: u64,
    pub duration_ms: u64,
    /// Path relative to the run directory; absent when no log was written.
    pub log: Option<String>,
    pub log_truncated: bool,
    /// True when a pipe was still open after the grace period: output may be partial.
    #[serde(default)]
    pub output_incomplete: bool,
    pub detail: Option<String>,
}

pub struct Executed {
    pub record: GateRecord,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Executed {
    /// The captured stdout, moved out (call after `write_log`); empty if already taken.
    pub fn take_stdout(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.stdout)
    }
}

/// Called every 10 ms poll with the stdout captured since the previous call (possibly empty,
/// split anywhere); `Some(reason)` tears the group down.
pub type Monitor<'a> = &'a mut dyn FnMut(&[u8]) -> Option<String>;

#[derive(Default)]
struct Capture {
    buf: Vec<u8>,
    truncated: bool,
    done: bool,
}

type Shared = Arc<Mutex<Capture>>;

fn drain<R: Read + Send + 'static>(mut r: R, max: usize) -> Shared {
    let cap = Shared::default();
    let shared = Arc::clone(&cap);
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let mut c = shared.lock().unwrap();
                    let room = max - c.buf.len();
                    c.truncated |= n > room;
                    c.buf.extend_from_slice(&chunk[..n.min(room)]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        shared.lock().unwrap().done = true;
    });
    cap
}

static SIGNALLED: AtomicBool = AtomicBool::new(false);
const SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

extern "C" fn on_signal(_: libc::c_int) {
    SIGNALLED.store(true, Ordering::SeqCst);
}

/// While alive, SIGTERM/SIGINT/SIGHUP set a flag instead of killing this
/// process, so the owner can tear its gate down before releasing the lease.
/// Dropping it restores the previous handlers.
pub struct SignalScope(Vec<libc::sigaction>);

impl SignalScope {
    pub fn install() -> Self {
        SIGNALLED.store(false, Ordering::SeqCst);
        let mut old = Vec::new();
        for sig in SIGNALS {
            // SAFETY: zeroed sigaction is valid; the handler only stores an atomic.
            unsafe {
                let mut act: libc::sigaction = std::mem::zeroed();
                act.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                libc::sigemptyset(&mut act.sa_mask);
                let mut prev: libc::sigaction = std::mem::zeroed();
                libc::sigaction(sig, &act, &mut prev);
                old.push(prev);
            }
        }
        Self(old)
    }
}

impl Drop for SignalScope {
    fn drop(&mut self) {
        for (sig, prev) in SIGNALS.iter().zip(&self.0) {
            // SAFETY: restoring the action sigaction returned for this signal.
            unsafe { libc::sigaction(*sig, prev, std::ptr::null_mut()) };
        }
    }
}

fn kill_group(pid: u32) {
    // SAFETY: plain signal to the process group we created with process_group(0).
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
}

/// Has the leader exited? WNOWAIT leaves it a zombie, so its pid (the group
/// id) cannot be reused before the group sweep. `true` on a wait error.
fn leader_exited(pid: u32) -> bool {
    // SAFETY: waitid fills the zeroed siginfo; WNOWAIT does not reap.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let r = libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        r != 0 || info.si_pid() != 0
    }
}

fn note(record: &mut GateRecord, text: &str) {
    record.detail = Some(match record.detail.take() {
        Some(d) => format!("{d}; {text}"),
        None => text.into(),
    });
}

enum Stop {
    Exited,
    Timeout,
    Signal,
    Cancel,
}

pub fn run(argv: &[String], cwd: &Path, timeout: Duration) -> Executed {
    run_cancellable(argv, cwd, timeout, None)
}

/// Like `run`, but the appearance of `cancel` (checked every 10 ms) tears the group down.
pub fn run_cancellable(
    argv: &[String],
    cwd: &Path,
    timeout: Duration,
    cancel: Option<&Path>,
) -> Executed {
    run_monitored(argv, cwd, Some(timeout), cancel, MAX_STREAM_BYTES, None)
}

/// The general form. `timeout: None` means no wall-clock cap (the worker's watchdog is then the
/// caller's liveness rule; `timeout_secs` is recorded as 0). `max_stream` bounds each captured
/// stream, and anything over it sets `log_truncated`. A `monitor` that answers `Some(reason)`
/// ends the run as `TimedOut` with `watchdog: <reason>` in the detail.
pub fn run_monitored(
    argv: &[String],
    cwd: &Path,
    timeout: Option<Duration>,
    cancel: Option<&Path>,
    max_stream: usize,
    mut monitor: Option<Monitor>,
) -> Executed {
    let mut record = GateRecord {
        outcome: Outcome::Absent,
        program: argv.first().cloned(),
        argv_len: argv.len(),
        cwd: cwd.display().to_string(),
        exit_code: None,
        signal: None,
        timeout_secs: timeout.map_or(0, |t| t.as_secs()),
        duration_ms: 0,
        log: None,
        log_truncated: false,
        output_incomplete: false,
        detail: None,
    };
    let done = |record: GateRecord| Executed {
        record,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    let Some((program, args)) = argv.split_first() else {
        record.detail = Some("no gate configured".into());
        return done(record);
    };
    let started = Instant::now();
    let spawned = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(
            ENV_ALLOWLIST
                .iter()
                .filter_map(|k| std::env::var_os(k).map(|v| (*k, v))),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            record.outcome = if e.kind() == io::ErrorKind::NotFound {
                Outcome::Missing
            } else {
                Outcome::Failed
            };
            record.detail = Some(format!("spawn {program}: {e}"));
            return done(record);
        }
    };
    let pid = child.id();
    let out = child.stdout.take().map(|r| drain(r, max_stream));
    let err = child.stderr.take().map(|r| drain(r, max_stream));

    let deadline = timeout.map(|t| started + t);
    let (mut tripped, mut seen) = (None, 0);
    let stop = loop {
        if SIGNALLED.load(Ordering::SeqCst) {
            break Stop::Signal;
        }
        if cancel.is_some_and(|p| fs::symlink_metadata(p).is_ok()) {
            break Stop::Cancel;
        }
        if leader_exited(pid) {
            break Stop::Exited;
        }
        if let (Some(m), Some(cap)) = (monitor.as_mut(), out.as_ref()) {
            // Copy out under the lock so the monitor never stalls the pipe reader.
            let fresh = {
                let c = cap.lock().unwrap();
                let v = c.buf[seen..].to_vec();
                seen = c.buf.len();
                v
            };
            tripped = m(&fresh);
            if tripped.is_some() {
                break Stop::Timeout;
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break Stop::Timeout;
        }
        thread::sleep(Duration::from_millis(10));
    };
    // The leader is still unreaped here, so the group id is still ours: sweep
    // the group first, then reap, so a backgrounded gate cannot outlive it.
    kill_group(pid);
    let status = child.wait();
    record.duration_ms = started.elapsed().as_millis() as u64;
    match &status {
        Ok(s) => {
            record.exit_code = s.code();
            record.signal = s.signal();
        }
        Err(e) => note(&mut record, &format!("wait: {e}")),
    }
    record.outcome = match (stop, &status) {
        (Stop::Signal, _) => Outcome::Interrupted,
        (Stop::Cancel, _) => Outcome::Cancelled,
        (Stop::Timeout, _) => Outcome::TimedOut,
        (Stop::Exited, Ok(s)) => match s.code() {
            Some(0) => Outcome::Passed,
            Some(127) => Outcome::Missing,
            _ => Outcome::Failed,
        },
        (Stop::Exited, Err(_)) => Outcome::Failed,
    };
    if let Some(why) = &tripped {
        note(&mut record, &format!("watchdog: {why}"));
    }
    match record.outcome {
        Outcome::Missing => note(&mut record, "exit 127: command not found"),
        Outcome::Interrupted => note(&mut record, "supervisor received a termination signal"),
        Outcome::Cancelled => note(&mut record, "cancelled by the operator's STOP-NOW"),
        _ => {}
    }

    // One shared grace period for both pipes. A descendant that escaped the
    // group may hold a pipe open: keep what was read and say it is partial.
    let caps: Vec<Shared> = out.into_iter().chain(err).collect();
    let grace = Instant::now() + DRAIN_GRACE;
    while Instant::now() < grace && caps.iter().any(|c| !c.lock().unwrap().done) {
        thread::sleep(Duration::from_millis(10));
    }
    let mut take = |cap: Option<&Shared>| {
        let Some(cap) = cap else { return Vec::new() };
        let mut c = cap.lock().unwrap();
        record.log_truncated |= c.truncated;
        record.output_incomplete |= !c.done;
        std::mem::take(&mut c.buf)
    };
    let stdout = take(caps.first());
    let stderr = take(caps.get(1));
    if let Some(m) = monitor.as_mut() {
        // Output that landed after the last poll still counts as evidence (a trip is moot now).
        let _ = m(stdout.get(seen..).unwrap_or_default());
    }
    if record.output_incomplete {
        note(
            &mut record,
            "output incomplete: a pipe stayed open after the gate was torn down",
        );
    }
    Executed {
        record,
        stdout,
        stderr,
    }
}

/// A log file created before the gate starts, so an unwritable log directory
/// is found while nothing has run yet.
pub struct LogSink {
    file: File,
    name: String,
}

pub fn open_log(run_dir: &Path) -> Result<LogSink> {
    let logs = run_dir.join("logs");
    state::ensure_private_dir(&logs, false)?;
    let name = format!("gate-{}-{}.log", state::now_nanos(), std::process::id());
    let path = logs.join(&name);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(io_err(&path))?;
    Ok(LogSink {
        file,
        name: format!("logs/{name}"),
    })
}

/// Fill the preflighted log; `record.log` is set only if the write succeeded.
pub fn write_log(mut sink: LogSink, exec: &mut Executed) -> Result<()> {
    let r = &exec.record;
    let mut body = format!(
        "outcome: {:?}\nprogram: {:?} (+{} args, not recorded)\nexit_code: {:?} signal: {:?}\ndetail: {:?}\n--- stdout ---\n",
        r.outcome,
        r.program,
        r.argv_len.saturating_sub(1),
        r.exit_code,
        r.signal,
        r.detail
    )
    .into_bytes();
    body.extend_from_slice(&exec.stdout);
    body.extend_from_slice(b"\n--- stderr ---\n");
    body.extend_from_slice(&exec.stderr);
    sink.file
        .write_all(&body)
        .and_then(|()| sink.file.sync_all())
        .map_err(|e| Error::new("io", format!("{}: {e}", sink.name)))?;
    exec.record.log = Some(sink.name);
    Ok(())
}
