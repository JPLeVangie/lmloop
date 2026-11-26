//! Serial fixture runner (M2 vertical slice). Fixture-only: it drives an
//! operator-supplied worker executable (never a real `opencode`) through the
//! manifest's tasks one at a time under one run lease.
//!
//! Per task: worker (must end with a clean `stop` and a fresh `sessionID`) ->
//! Git capture of the scoped change -> operator gate run on the exact export ->
//! export and candidate re-checked unchanged -> `commit-tree` -> durable intent
//! in `run.json` -> ref compare-and-swap -> task recorded. Any failure stops the
//! run with nothing committed and the worktree untouched. Git is the witness;
//! the worker stream only proves how the worker ended.
//!
//! Crash recovery: `run.json` is the single atomic state file. A run found in
//! `committing` whose ref already equals the intended commit (child of the
//! recorded head, recorded tree) is completed without creating another commit.
//! Every other mid-task state is refused as needing manual reconciliation; the
//! refusal writes nothing, so `run.json`'s intent and the unreferenced commit
//! object stay as evidence. The real index is only ever moved by the guarded
//! `sync_index` (it must hold exactly the old or the new tree), on the normal
//! path and on recovery alike, so independently staged work is never clobbered.
//!
//! Journal recovery: `fixture-resume` repairs `events.jsonl` under the winning lease, only
//! for resumable phases (paused, idle, committing with the commit landed) and before any
//! control or engine change. Complete records are never dropped or rewritten; an
//! unterminated final line is archived durably in `journal-recovery/` and only then
//! truncated (see `protocol::recover`). A malformed, foreign, other-version or misnumbered
//! complete record is refused with exit 8 and nothing altered.
//!
//! Controls are files in the run directory, honoured only by the lease holder
//! (a losing claimant never touches them) and archived, not deleted, once acted on:
//! `STOP` ends the run at the next task boundary, `PAUSE` parks it there
//! (`fixture-resume` continues from the persisted manifest), and `STOP-NOW`
//! also tears down the in-flight worker or gate process group at once. None of
//! them commits or cleans anything; an interrupted task's changes stay in the worktree.
//!
//! shortcut: the export fingerprint streams each file through std's SipHash
//! (not cryptographic) and refuses exports over `MAX_EXPORT_ENTRIES` entries or
//! `MAX_EXPORT_BYTES` bytes; the gate is operator-trusted, so a deliberate
//! collision is out of scope. Upgrade to a real digest if that ever matters.

use crate::gate::{self, GateRecord, Outcome};
use crate::git::{self, Repo};
use crate::lease::{self, Claim};
use crate::protocol::{self, EventLog, Progress, Summary, ToolEvent, tool_event};
use crate::state::{self, Error, Result, Store, io_err};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hasher};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const RUN_SCHEMA_VERSION: u32 = 1;
pub const MANIFEST_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 1 << 20;
const MAX_PROMPT_BYTES: usize = 16 * 1024;
const MAX_TASKS: usize = 16;
pub const MAX_PILOT_TASKS: usize = 16;
const MAX_HANDOFF_BYTES: usize = 4096;
const HANDOFF_HEADER: &str =
    "\n\nPrior committed tasks in this run (already on HEAD; do not redo them):\n";
const MAX_EXPORT_ENTRIES: usize = 5000;
const MAX_EXPORT_BYTES: u64 = 256 << 20;
const MAX_TIMEOUT_SECS: u64 = 3600;
/// Per-stream capture for a worker (a gate keeps 128 KiB): a skill-heavy session emits far more
/// than that, and a truncated stream is still refused, never read as partial evidence.
const WORKER_STREAM_BYTES: usize = 8 << 20;
const WATCHDOG_FILE: &str = "watchdog.json";
const WATCH_WRITE_EVERY: Duration = Duration::from_secs(1);
const MAX_TOOL_LINE: usize = 1 << 20;
/// The advisory scoped-file fingerprint runs on the supervising thread, so it is tiny: a few
/// literal file scopes and 1 MiB in all, once per stdout chunk. Anything else is "unknown".
const WATCH_FP_SCOPES: usize = 8;
const WATCH_FP_BYTES: u64 = 1 << 20;
/// Consecutive identical completed tool calls, over unchanged scoped files, that earn a warning.
const REPEAT_WARN: u64 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cmd {
    /// Absolute program path then arguments; run directly, never through a shell.
    pub argv: Vec<String>,
    /// Hard wall-clock cap. Required, except for a worker that has a `watchdog`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Worker only, opt-in: liveness from an external trusted writer's progress file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchdog: Option<Watchdog>,
}

/// The worker watchdog. `progress_file` is an operator-precreated private file outside the
/// checkout that a trusted bridge appends `model\n` to after each substantive model delta (see
/// `protocol::Progress`). Stdout lines, retries, keepalives and Git changes never count as
/// liveness. There is no `tool_secs`: the CLI reports completed tools only, so a running tool is
/// indistinguishable from a silent model and `idle_secs` must outlast the longest tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Watchdog {
    pub progress_file: PathBuf,
    /// Opt-in group for a relay account writing the shared 0620 progress file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_gid: Option<u32>,
    /// Allowed from worker start to the first model record.
    pub startup_secs: u64,
    /// Allowed between model records after the first.
    pub idle_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    /// Literal repo-relative paths the task may change.
    pub scopes: Vec<String>,
}

/// The operator manifest, persisted verbatim as the run's resolved policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub manifest_version: u32,
    pub repo: PathBuf,
    /// The prompt is appended as the final argument; stdin is null.
    pub worker: Cmd,
    /// Run with the exported tree as cwd; exit 0 is the only pass.
    pub gate: Cmd,
    pub tasks: Vec<Task>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeConfig {
    pub root: PathBuf,
    #[serde(default = "default_branch_prefix")]
    pub branch_prefix: String,
}

fn default_branch_prefix() -> String {
    "lmloop".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeState {
    pub path: PathBuf,
    pub branch: String,
    pub base_ref: String,
    pub base_head: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    /// Between tasks; safe to continue.
    Idle,
    Worker,
    Gate,
    /// Intent recorded; the ref may or may not have moved.
    Committing,
    /// Parked at a task boundary by PAUSE; `fixture-resume` continues.
    Paused,
    /// Ended by STOP or STOP-NOW; terminal, the worktree is preserved.
    Stopped,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub task: String,
    pub ref_name: String,
    pub head: String,
    pub tree: String,
    pub commit: String,
    pub session_id: String,
    pub gate_log: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Done {
    pub task: String,
    pub session_id: String,
    pub head: String,
    pub tree: String,
    pub commit: String,
    pub gate_log: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub task: String,
    pub reason: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunState {
    pub schema_version: u32,
    pub run_id: String,
    pub policy: Policy,
    pub phase: RunPhase,
    pub next_task: usize,
    pub done: Vec<Done>,
    pub intent: Option<Intent>,
    pub failure: Option<Failure>,
    pub updated_at_unix: u64,
    /// Set by `pilot-run` only, never by a manifest; absent in older state means fixture.
    #[serde(default)]
    pub pilot: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeState>,
}

impl RunState {
    pub fn active_repo(&self) -> &Path {
        self.worktree
            .as_ref()
            .map_or(self.policy.repo.as_path(), |w| w.path.as_path())
    }
}

pub enum Finish {
    Complete,
    TaskFailed,
    Stopped,
    Paused,
    OwnedElsewhere,
    /// Another lmloop process holds this Git checkout; nothing was changed.
    CheckoutBusy,
    NeedsReconciliation(String),
    NoRun,
    Usage(String),
}

fn validate(p: &Policy, pilot: bool) -> std::result::Result<(), String> {
    if p.manifest_version != MANIFEST_VERSION {
        return Err(format!("manifest_version must be {MANIFEST_VERSION}"));
    }
    if !p.repo.is_absolute() {
        return Err("repo must be an absolute path".into());
    }
    for (what, c) in [("worker", &p.worker), ("gate", &p.gate)] {
        let range = 1..=MAX_TIMEOUT_SECS;
        let Some(program) = c.argv.first() else {
            return Err(format!("{what}.argv must not be empty"));
        };
        if !Path::new(program).is_absolute() {
            return Err(format!("{what}.argv[0] must be an absolute path"));
        }
        if c.argv.iter().any(|a| a.contains('\0')) {
            return Err(format!("{what}.argv contains a NUL"));
        }
        match (&c.timeout_secs, &c.watchdog) {
            (Some(t), _) if !range.contains(t) => {
                return Err(format!("{what}.timeout_secs must be 1-{MAX_TIMEOUT_SECS}"));
            }
            (None, None) => return Err(format!("{what}.timeout_secs is required")),
            (_, Some(_)) if what == "gate" => {
                return Err("watchdog is for the worker only".into());
            }
            (_, Some(w)) => {
                let file = w.progress_file.to_str().unwrap_or("");
                if !w.progress_file.is_absolute() || file.contains('\0') {
                    return Err("watchdog.progress_file must be an absolute path".into());
                }
                if !range.contains(&w.startup_secs) || !range.contains(&w.idle_secs) {
                    return Err(format!(
                        "watchdog startup_secs and idle_secs must be 1-{MAX_TIMEOUT_SECS}"
                    ));
                }
            }
            _ => {}
        }
    }
    if !pilot
        && Path::new(&p.worker.argv[0])
            .file_name()
            .is_some_and(|n| n == "opencode")
    {
        return Err("the fixture runner refuses to launch a real opencode".into());
    }
    if pilot && p.tasks.len() > MAX_PILOT_TASKS {
        return Err(format!("a pilot runs 1-{MAX_PILOT_TASKS} tasks"));
    }
    if p.tasks.is_empty() || p.tasks.len() > MAX_TASKS {
        return Err(format!("tasks must number 1-{MAX_TASKS}"));
    }
    if let Some(w) = &p.worktree {
        let prefix = &w.branch_prefix;
        if !w.root.is_absolute() || w.root.as_os_str().as_encoded_bytes().contains(&0) {
            return Err("worktree.root must be an absolute path without NUL".into());
        }
        let bad_prefix = prefix.is_empty()
            || prefix.len() > 64
            || prefix.starts_with('/')
            || prefix.ends_with('/')
            || prefix.contains("..")
            || prefix.contains("//")
            || prefix.split('/').any(|s| {
                s.is_empty()
                    || s.starts_with('.')
                    || s.ends_with('.')
                    || s.ends_with(".lock")
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            });
        if bad_prefix {
            return Err("worktree.branch_prefix is invalid".into());
        }
    }
    for (i, t) in p.tasks.iter().enumerate() {
        state::validate_run_id(&t.id)
            .map_err(|_| format!("task id {:?} is not a simple id", t.id))?;
        if p.tasks[..i].iter().any(|o| o.id == t.id) {
            return Err(format!("duplicate task id {:?}", t.id));
        }
        let max_prompt = if i == 0 {
            MAX_PROMPT_BYTES
        } else {
            MAX_PROMPT_BYTES - HANDOFF_HEADER.len() - MAX_HANDOFF_BYTES
        };
        if t.prompt.trim().is_empty() || t.prompt.len() > max_prompt || t.prompt.contains('\0') {
            return Err(format!(
                "task {:?}: prompt must be 1-{max_prompt} bytes",
                t.id
            ));
        }
        if t.scopes.is_empty() {
            return Err(format!("task {:?}: scopes must not be empty", t.id));
        }
    }
    Ok(())
}

/// Parse and validate an operator manifest; `Err` is a usage message.
pub fn load_manifest(path: &Path) -> std::result::Result<Policy, String> {
    load(path, false)
}

/// As `load_manifest`, for `pilot-run`: one task, and a launcher named `opencode` is allowed.
pub fn load_pilot_manifest(path: &Path) -> std::result::Result<Policy, String> {
    load(path, true)
}

/// As `load_pilot_manifest`, for the TUI: the file must be a regular file this user owns with
/// mode 0600, opened without following a symlink or blocking on a FIFO (`state::read_bounded`).
pub fn load_private_pilot_manifest(path: &Path) -> std::result::Result<Policy, String> {
    let text = state::read_bounded(path)
        .map_err(|e| format!("manifest {}: {e}", path.display()))?
        .ok_or_else(|| format!("manifest {}: not found", path.display()))?;
    parse(&text, true)
}

/// A one-task pilot template with the requester's objective appended to the task prompt,
/// revalidated (so the prompt size cap still holds). Only the prompt changes.
pub fn with_objective(mut p: Policy, objective: &str) -> std::result::Result<Policy, String> {
    for t in &mut p.tasks {
        t.prompt = format!("{}\n\nObjective:\n{objective}", t.prompt);
    }
    validate(&p, true)?;
    Ok(p)
}

fn load(path: &Path, pilot: bool) -> std::result::Result<Policy, String> {
    let mut text = String::new();
    File::open(path)
        .and_then(|f| f.take(MAX_MANIFEST_BYTES + 1).read_to_string(&mut text))
        .map_err(|e| format!("manifest {}: {e}", path.display()))?;
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("manifest is too large".into());
    }
    parse(&text, pilot)
}

fn parse(text: &str, pilot: bool) -> std::result::Result<Policy, String> {
    let policy: Policy = serde_json::from_str(text).map_err(|e| format!("manifest: {e}"))?;
    validate(&policy, pilot)?;
    Ok(policy)
}

fn write_handoff(dir: &Path, body: &str) -> Result<()> {
    let tmp = dir.join(format!("handoff.{}.tmp", std::process::id()));
    let dest = dir.join("handoff.md");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .map_err(io_err(&tmp))?;
    file.write_all(body.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(io_err(&tmp))?;
    fs::rename(&tmp, &dest).map_err(io_err(&dest))?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_err(dir))
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

fn render_handoff(done: &[Done], head: &str, scopes: &[String]) -> String {
    let mut body = String::new();
    for (n, d) in done.iter().enumerate() {
        let files = if d.changed.is_empty() {
            String::new()
        } else {
            const MAX_FILES: usize = 16;
            let shown = d
                .changed
                .iter()
                .take(MAX_FILES)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            if d.changed.len() > MAX_FILES {
                format!(": {shown} (+{} more)", d.changed.len() - MAX_FILES)
            } else {
                format!(": {shown}")
            }
        };
        let line = format!(
            "{}. {} (commit {}, gate passed){files}\n",
            n + 1,
            d.task,
            short(&d.commit)
        );
        if body.len() + line.len() + 128 > MAX_HANDOFF_BYTES {
            body.push_str("... (additional tasks omitted for length)\n");
            break;
        }
        body.push_str(&line);
    }
    let tail = if scopes.is_empty() {
        format!("Current clean HEAD: {}.", short(head))
    } else {
        format!(
            "Start from current clean HEAD ({}). Only modify files within your allowed scopes: {}.",
            short(head),
            scopes.join(", ")
        )
    };
    body.push_str(&tail);
    if body.len() > MAX_HANDOFF_BYTES {
        let mut end = MAX_HANDOFF_BYTES;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    body
}
/// Atomic publish: temp file, fsync, rename, fsync the directory. One writer: the lease holder.
fn write_json<T: Serialize>(dir: &Path, name: &str, value: &T) -> Result<()> {
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    let dest = dir.join(name);
    let mut body = serde_json::to_vec_pretty(value).map_err(|e| Error::new("io", e.to_string()))?;
    body.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .map_err(io_err(&tmp))?;
    file.write_all(&body)
        .and_then(|()| file.sync_all())
        .map_err(io_err(&tmp))?;
    fs::rename(&tmp, &dest).map_err(io_err(&dest))?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_err(dir))
}

/// `Ok(None)` if the directory holds no serial run.
pub fn read_state(dir: &Path, run_id: &str) -> Result<Option<RunState>> {
    let Some(text) = state::read_bounded(&dir.join("run.json"))? else {
        return Ok(None);
    };
    let bad = |m: String| Error::new("malformed", m);
    let value: Value = serde_json::from_str(&text).map_err(|e| bad(e.to_string()))?;
    match value.get("schema_version").and_then(Value::as_u64) {
        None => return Err(bad("missing schema_version".into())),
        Some(v) if v != u64::from(RUN_SCHEMA_VERSION) => {
            return Err(Error::new(
                "unsupported_version",
                format!("run.json schema_version {v}; this build reads {RUN_SCHEMA_VERSION}"),
            ));
        }
        Some(_) => {}
    }
    let st: RunState = serde_json::from_value(value).map_err(|e| bad(e.to_string()))?;
    validate(&st.policy, st.pilot).map_err(|m| bad(format!("stored policy: {m}")))?;
    if st.run_id != run_id || st.next_task > st.policy.tasks.len() {
        return Err(bad(
            "run.json does not belong to this run or is inconsistent".into(),
        ));
    }
    Ok(Some(st))
}

struct Ctx<'a> {
    dir: &'a Path,
    log: EventLog,
    st: RunState,
}

impl Ctx<'_> {
    fn save(&mut self) -> Result<()> {
        self.st.updated_at_unix = state::now_unix();
        write_json(self.dir, "run.json", &self.st)
    }

    fn emit(&mut self, kind: &str, task: Option<&str>, data: Value) -> Result<()> {
        let id = self.st.run_id.clone();
        self.log.append(&id, kind, task, data)
    }

    /// Stop the run: nothing is committed, the worktree is left exactly as the worker left it.
    fn fail(&mut self, task: &str, reason: &str, detail: &str) -> Result<Option<Finish>> {
        self.st.phase = RunPhase::Failed;
        self.st.failure = Some(Failure {
            task: task.into(),
            reason: reason.into(),
            detail: detail.chars().take(500).collect(),
        });
        self.save()?;
        self.emit(
            "task_failed",
            Some(task),
            json!({ "reason": reason, "detail": detail.chars().take(500).collect::<String>() }),
        )?;
        Ok(Some(Finish::TaskFailed))
    }

    fn ctl(&self, name: &str) -> bool {
        fs::symlink_metadata(self.dir.join(name)).is_ok()
    }

    /// Park (`Paused`) or end (`Stopped`) the run, then archive the controls that caused it.
    /// State first: a crash between the two leaves a harmless control file, never a lost stop.
    fn halt(&mut self, phase: RunPhase, failure: Option<Failure>) -> Result<Finish> {
        let paused = phase == RunPhase::Paused;
        self.st.phase = phase;
        self.st.failure = failure;
        self.save()?;
        let names: &[&str] = if paused {
            &["PAUSE"]
        } else {
            &["STOP-NOW", "STOP"]
        };
        let archived = archive_controls(self.dir, names)?;
        let kind = if paused { "run_paused" } else { "run_stopped" };
        self.emit(
            kind,
            None,
            json!({ "archived": archived, "failure": self.st.failure }),
        )?;
        Ok(if paused {
            Finish::Paused
        } else {
            Finish::Stopped
        })
    }
}

/// STOP-NOW reached an in-flight task: nothing is committed, the worktree stays as it is.
fn stop_now(c: &mut Ctx, task: &str, at: &str) -> Result<Option<Finish>> {
    let failure = Failure {
        task: task.into(),
        reason: "stop_now".into(),
        detail: format!("cancelled during {at}; nothing committed, worktree preserved"),
    };
    c.halt(RunPhase::Stopped, Some(failure)).map(Some)
}

/// Move consumed control files into `stop-archive/` instead of deleting them.
pub fn archive_controls(dir: &Path, names: &[&str]) -> Result<Vec<String>> {
    let mut archived = Vec::new();
    for name in names {
        let p = dir.join(name);
        if fs::symlink_metadata(&p).is_err() {
            continue;
        }
        let archive = dir.join("stop-archive");
        state::ensure_private_dir(&archive, false)?;
        let dest = archive.join(format!(
            "{name}.{}-{}",
            state::now_nanos(),
            std::process::id()
        ));
        match fs::rename(&p, &dest) {
            Ok(()) => archived.push(name.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&p)(e)),
        }
    }
    Ok(archived)
}

/// Git with a scrubbed environment; for ref checks and the index sync.
fn git_raw(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .current_dir(repo)
        .env_clear()
        .envs(std::env::var_os("PATH").map(|p| ("PATH", p)))
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::new("git", format!("spawn git: {e}")))
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = git_raw(repo, args)?;
    if !out.status.success() {
        return Err(Error::new(
            "git",
            format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Guarded index sync, used on the normal path and on recovery alike: `git::sync_real_index`
/// re-validates the ref and commit and the index (exactly the old or the new tree, so staged
/// work is never overwritten) under Git's `index.lock`. Idempotent.
fn sync_index(repo: &Path, i: &Intent) -> Result<()> {
    git::sync_real_index(repo, &i.ref_name, &i.head, &i.tree, &i.commit)
}

/// Fixture failpoint (`LMLOOP_FIXTURE_CRASH=before_cas|after_cas`): die like a SIGKILL would.
fn crash_point(at: &str) {
    if std::env::var("LMLOOP_FIXTURE_CRASH").as_deref() == Ok(at) {
        std::process::exit(86);
    }
}

/// Run `argv` under the M1 supervisor with its output logged; returns the record and the
/// captured stdout itself (never re-read from the log, so no marker can be forged into it).
fn supervised(
    dir: &Path,
    argv: &[String],
    cwd: &Path,
    timeout: Option<Duration>,
    max_stream: usize,
    monitor: Option<gate::Monitor>,
) -> Result<(GateRecord, Vec<u8>)> {
    let sink = gate::open_log(dir)?;
    let cancel = dir.join("STOP-NOW");
    let mut exec = gate::run_monitored(argv, cwd, timeout, Some(&cancel), max_stream, monitor);
    gate::write_log(sink, &mut exec)?;
    let stdout = exec.take_stdout();
    Ok((exec.record, stdout))
}

fn worker_verdict(
    rec: &GateRecord,
    stdout: &[u8],
    done: &[Done],
) -> std::result::Result<Summary, String> {
    if rec.outcome != Outcome::Passed {
        return Err(format!(
            "worker {:?} (exit {:?})",
            rec.outcome, rec.exit_code
        ));
    }
    if rec.log_truncated || rec.output_incomplete {
        return Err("worker output truncated or incomplete".into());
    }
    let text = std::str::from_utf8(stdout).map_err(|_| "worker stdout is not UTF-8")?;
    let summary = protocol::parse(text)?;
    if done.iter().any(|d| d.session_id == summary.session_id) {
        return Err(format!(
            "sessionID {:?} was already used by an earlier task",
            summary.session_id
        ));
    }
    Ok(summary)
}

type Files = BTreeMap<String, (u32, u64, u64)>;

fn digest(r: &mut impl Read, budget: &mut u64, path: &Path) -> Result<(u64, u64)> {
    let (mut h, mut len, mut buf) = (DefaultHasher::new(), 0u64, [0u8; 16384]);
    loop {
        let n = r.read(&mut buf).map_err(io_err(path))?;
        if n == 0 {
            return Ok((len, h.finish()));
        }
        *budget = budget
            .checked_sub(n as u64)
            .ok_or_else(|| Error::new("export_too_large", "export exceeds the byte bound"))?;
        h.write(&buf[..n]);
        len += n as u64;
    }
}

/// path -> (mode, length, content digest or link-target digest) for everything under `dir`,
/// directories included. Bounded in entries and total bytes; reads files in a stream.
fn fingerprint(dir: &Path) -> Result<Files> {
    fingerprint_in(dir, MAX_EXPORT_ENTRIES, MAX_EXPORT_BYTES)
}

fn fingerprint_in(dir: &Path, max_entries: usize, mut budget: u64) -> Result<Files> {
    fn go(
        dir: &Path,
        prefix: &str,
        out: &mut Files,
        budget: &mut u64,
        max_entries: usize,
    ) -> Result<()> {
        for entry in fs::read_dir(dir).map_err(io_err(dir))? {
            let path = entry.map_err(io_err(dir))?.path();
            if out.len() >= max_entries {
                return Err(Error::new(
                    "export_too_large",
                    "export has too many entries",
                ));
            }
            let meta = fs::symlink_metadata(&path).map_err(io_err(&path))?;
            let name = path.file_name().unwrap_or_default();
            let name = name
                .to_str()
                .ok_or_else(|| Error::new("export_mismatch", "export path is not valid UTF-8"))?;
            let rel = format!("{prefix}{name}");
            if meta.is_dir() {
                out.insert(format!("{rel}/"), (meta.mode(), 0, 0));
                go(&path, &format!("{rel}/"), out, budget, max_entries)?;
            } else if meta.is_symlink() {
                let target = fs::read_link(&path).map_err(io_err(&path))?;
                let (len, d) = digest(&mut target.as_os_str().as_encoded_bytes(), budget, &path)?;
                out.insert(rel, (meta.mode(), len, d));
            } else if meta.is_file() {
                let f = open_regular(&path).map_err(io_err(&path))?;
                if !f.metadata().map_err(io_err(&path))?.is_file() {
                    return Err(Error::new(
                        "export_mismatch",
                        format!("export entry changed type: {}", path.display()),
                    ));
                }
                let (len, d) = digest(&mut &f, budget, &path)?;
                out.insert(rel, (meta.mode(), len, d));
            } else {
                return Err(Error::new(
                    "export_mismatch",
                    format!("export has a non-regular entry: {}", path.display()),
                ));
            }
        }
        Ok(())
    }
    let mut out = Files::new();
    go(dir, "", &mut out, &mut budget, max_entries)?;
    Ok(out)
}

/// Never follows a symlink and never blocks on a FIFO (a worker controls what is on disk).
fn open_regular(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// Open `rel` (normal components only) under `root` one component at a time with `openat`, so a
/// symlink at any level is refused (`ELOOP`/`ENOTDIR`) rather than followed, and a FIFO cannot
/// hang the open. `Ok(None)` is absent.
fn open_under(root: &Path, rel: &Path) -> std::io::Result<Option<File>> {
    let mut cur = File::open(root)?;
    let n = rel.components().count();
    for (i, c) in rel.components().enumerate() {
        let name = CString::new(c.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let dir = if i + 1 < n { libc::O_DIRECTORY } else { 0 };
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | dir;
        // SAFETY: `cur` is an open directory fd and `name` a NUL-terminated string; a
        // non-negative result is a fresh fd we own.
        let fd = unsafe { libc::openat(cur.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return if e.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(e)
            };
        }
        cur = unsafe { File::from_raw_fd(fd) };
    }
    Ok(Some(cur))
}

/// One hash of the task's literal file scopes as they are now, or `None` ("unknown": no warning
/// is made) for anything ambiguous: an odd path, a directory scope, a symlink at any level, a
/// non-regular entry, or over the small bound. Absent scopes hash as absent.
fn scoped_fingerprint(repo: &Path, scopes: &[String]) -> Option<u64> {
    if scopes.len() > WATCH_FP_SCOPES {
        return None;
    }
    let (mut all, mut budget) = (Files::new(), WATCH_FP_BYTES);
    for s in scopes {
        let rel = Path::new(s);
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return None;
        }
        let v = match open_under(repo, rel).ok()? {
            None => (0, 0, 0),
            Some(f) => {
                let m = f.metadata().ok()?;
                if !m.is_file() || m.len() > budget {
                    return None;
                }
                let (len, d) = digest(&mut &f, &mut budget, rel).ok()?;
                (m.mode(), len, d)
            }
        };
        all.insert(s.clone(), v);
    }
    let mut h = DefaultHasher::new();
    std::hash::Hash::hash(&all, &mut h);
    Some(h.finish())
}

/// The latest worker-watchdog state, persisted as `watchdog.json` (written by the lease holder,
/// at most once a second while watching, and always at the end). It survives a failed task, so
/// the evidence of why a worker was cut off is still there; a later task replaces it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchdogEvidence {
    pub schema_version: u32,
    pub task: String,
    /// `watching`, `tripped` (the watchdog ended the worker) or `ended` (anything else).
    pub state: String,
    pub startup_secs: u64,
    pub idle_secs: u64,
    pub started_unix: u64,
    pub updated_at_unix: u64,
    /// Complete `model` records seen this task, past the baseline already in the file.
    pub model_pulses: u64,
    pub baseline_pulses: u64,
    pub last_pulse_unix: Option<u64>,
    pub tools_completed: u64,
    pub last_tool: Option<String>,
    pub last_tool_unix: Option<u64>,
    /// Always `completed_only`: no in-flight tool state exists to report.
    pub tools_observed: String,
    /// Consecutive identical completed tool calls over unchanged scoped files.
    pub repeat_count: u64,
    /// Advisory only; it never stops the worker.
    pub warning: Option<String>,
    pub trip: Option<String>,
}

pub fn read_watchdog(dir: &Path) -> Result<Option<WatchdogEvidence>> {
    let Some(text) = state::read_bounded(&dir.join(WATCHDOG_FILE))? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| Error::new("malformed", format!("{WATCHDOG_FILE}: {e}")))
}

/// Per-task watchdog: fed the worker's stdout as it arrives, polls the progress file, and
/// answers `Some(reason)` when the worker must be cut off. Every clock is `Instant`.
struct Dog<'a> {
    dir: &'a Path,
    repo: &'a Path,
    scopes: &'a [String],
    progress: Progress,
    started: Instant,
    last_pulse: Instant,
    last_write: Option<Instant>,
    dirty: bool,
    line: Vec<u8>,
    skipping: bool,
    prev: Option<(String, u64, u64)>,
    /// Scoped-file fingerprint, taken lazily at most once per stdout chunk.
    fp: Option<Option<u64>>,
    ev: WatchdogEvidence,
}

impl<'a> Dog<'a> {
    /// Open the progress file (this task's baseline) before the worker exists. The first task of
    /// a run requires an empty file: a reused one could not be told from a live writer's.
    fn start(
        dir: &'a Path,
        repo: &'a Path,
        t: &'a Task,
        cfg: &Watchdog,
        first_task: bool,
    ) -> Result<Self> {
        let progress = match cfg.writer_gid {
            Some(gid) => Progress::open_shared(&cfg.progress_file, repo, gid)?,
            None => Progress::open(&cfg.progress_file, repo)?,
        };
        if first_task && !progress.was_empty_at_open() {
            return Err(Error::new(
                "progress_unsafe",
                format!(
                    "{} already holds records; the operator must create a fresh file for each run",
                    cfg.progress_file.display()
                ),
            ));
        }
        let now = state::now_unix();
        let mut d = Self {
            dir,
            repo,
            scopes: &t.scopes,
            started: Instant::now(),
            last_pulse: Instant::now(),
            last_write: None,
            dirty: true,
            line: Vec::new(),
            skipping: false,
            prev: None,
            fp: None,
            ev: WatchdogEvidence {
                schema_version: 1,
                task: t.id.clone(),
                state: "watching".into(),
                startup_secs: cfg.startup_secs,
                idle_secs: cfg.idle_secs,
                started_unix: now,
                updated_at_unix: now,
                model_pulses: 0,
                baseline_pulses: progress.baseline(),
                last_pulse_unix: None,
                tools_completed: 0,
                last_tool: None,
                last_tool_unix: None,
                tools_observed: "completed_only".into(),
                repeat_count: 0,
                warning: None,
                trip: None,
            },
            progress,
        };
        d.flush(true)?;
        Ok(d)
    }

    fn flush(&mut self, force: bool) -> Result<()> {
        let due = self
            .last_write
            .is_none_or(|t| t.elapsed() >= WATCH_WRITE_EVERY);
        if self.dirty && (force || due) {
            self.ev.updated_at_unix = state::now_unix();
            write_json(self.dir, WATCHDOG_FILE, &self.ev)?;
            (self.dirty, self.last_write) = (false, Some(Instant::now()));
        }
        Ok(())
    }

    fn ingest(&mut self, fresh: &[u8]) {
        self.fp = None;
        for part in fresh.split_inclusive(|b| *b == b'\n') {
            if !self.skipping {
                self.line.extend_from_slice(part);
                if self.line.len() > MAX_TOOL_LINE {
                    self.line.clear();
                    self.skipping = true;
                }
            }
            if part.last() == Some(&b'\n') {
                if !self.skipping
                    && let Some(t) = tool_event(&self.line)
                {
                    self.tool(t);
                }
                self.line.clear();
                self.skipping = false;
            }
        }
    }

    fn tool(&mut self, t: ToolEvent) {
        self.ev.tools_completed += 1;
        self.ev.last_tool_unix = Some(state::now_unix());
        // No tool fingerprint, nothing to compare: do not touch the files at all.
        let key = t.fingerprint.and_then(|f| {
            let sf = *self
                .fp
                .get_or_insert_with(|| scoped_fingerprint(self.repo, self.scopes));
            Some((t.name.clone(), f, sf?))
        });
        self.ev.repeat_count = match (&self.prev, &key) {
            (Some(p), Some(k)) if p == k => self.ev.repeat_count + 1,
            (_, Some(_)) => 1,
            _ => 0, // no input/output to compare, or the files could not be read: claim nothing
        };
        self.prev = key;
        if self.ev.repeat_count >= REPEAT_WARN {
            self.ev.warning = Some(format!(
                "tool {:?} completed {} times in a row with the same input and result and no change to the task's scoped files; the worker may be looping (advisory only)",
                t.name, self.ev.repeat_count
            ));
        }
        self.ev.last_tool = Some(t.name);
        self.dirty = true;
    }

    fn observe(&mut self, fresh: &[u8]) -> Option<String> {
        self.ingest(fresh);
        let pulses = match self.progress.poll() {
            Ok(n) => n,
            Err(e) => return Some(self.trip(format!("progress source failed closed: {e}"))),
        };
        if pulses > self.ev.model_pulses {
            self.ev.model_pulses = pulses;
            self.ev.last_pulse_unix = Some(state::now_unix());
            (self.last_pulse, self.dirty) = (Instant::now(), true);
        }
        let (limit, what) = if pulses == 0 {
            (self.ev.startup_secs, "startup stall: no model activity")
        } else {
            (self.ev.idle_secs, "idle: no model activity")
        };
        if self.last_pulse.elapsed() >= Duration::from_secs(limit) {
            let spent = self.started.elapsed().as_secs();
            return Some(self.trip(format!(
                "{what} for {limit}s ({pulses} model records, {spent}s into the task)"
            )));
        }
        let _ = self.flush(false); // best effort while watching; `finish` reports a failure
        None
    }

    fn trip(&mut self, why: String) -> String {
        (self.ev.state, self.ev.trip, self.dirty) = ("tripped".into(), Some(why.clone()), true);
        let _ = self.flush(true);
        why
    }

    /// The final write; returns why the watchdog ended the worker, if it did.
    fn finish(mut self) -> Result<Option<String>> {
        if self.ev.trip.is_none() {
            // Records written between the last poll and the worker's exit still count as evidence.
            if let Ok(n) = self.progress.poll()
                && n > self.ev.model_pulses
            {
                (self.ev.model_pulses, self.ev.last_pulse_unix) = (n, Some(state::now_unix()));
            }
            self.ev.state = "ended".into();
        }
        self.dirty = true;
        self.flush(true)?;
        Ok(self.ev.trip)
    }
}

fn task(c: &mut Ctx) -> Result<Option<Finish>> {
    let policy = c.st.policy.clone();
    let t = policy.tasks[c.st.next_task].clone();
    let id = t.id.as_str();
    let active_repo = c.st.active_repo().to_path_buf();
    let repo = match Repo::open(&active_repo) {
        Ok(r) => r,
        Err(e) => return c.fail(id, e.kind, &e.message),
    };
    c.st.phase = RunPhase::Worker;
    c.save()?;
    c.emit(
        "task_started",
        Some(id),
        json!({ "ref": repo.ref_name, "head": repo.head }),
    )?;

    let mut dog = match &policy.worker.watchdog {
        Some(cfg) => match Dog::start(c.dir, &active_repo, &t, cfg, c.st.next_task == 0) {
            Ok(d) => Some(d),
            Err(e) => return c.fail(id, "watchdog_unavailable", &e.message),
        },
        None => None,
    };
    let mut argv = policy.worker.argv.clone();
    let prompt = if c.st.done.is_empty() {
        t.prompt.clone()
    } else {
        format!(
            "{}{HANDOFF_HEADER}{}",
            t.prompt,
            render_handoff(&c.st.done, &repo.head, &t.scopes)
        )
    };
    argv.push(prompt);
    let watched = dog.is_some();
    let mut watch = |out: &[u8]| dog.as_mut().and_then(|d| d.observe(out));
    let (rec, stdout) = supervised(
        c.dir,
        &argv,
        &active_repo,
        policy.worker.timeout_secs.map(Duration::from_secs),
        WORKER_STREAM_BYTES,
        watched.then_some(&mut watch),
    )?;
    let tripped = dog.map(Dog::finish).transpose()?.flatten();
    if rec.outcome == Outcome::Cancelled {
        return stop_now(c, id, "the worker");
    }
    if let Some(why) = tripped {
        return c.fail(id, "worker_watchdog", &why);
    }
    let summary = match worker_verdict(&rec, &stdout, &c.st.done) {
        Ok(s) => s,
        Err(why) => return c.fail(id, "worker_rejected", &why),
    };
    c.emit("worker_finished", Some(id), json!({
        "session_id": summary.session_id, "events": summary.events, "tools": summary.tools, "log": rec.log,
    }))?;

    let scopes: Vec<&str> = t.scopes.iter().map(String::as_str).collect();
    let scratch = c.dir.join("scratch");
    state::ensure_private_dir(&scratch, false)?;
    let snap = match repo.capture(&scopes, &scratch) {
        Ok(s) => s,
        Err(e) => return c.fail(id, e.kind, &e.message),
    };
    c.st.phase = RunPhase::Gate;
    c.save()?;
    c.emit(
        "snapshot_captured",
        Some(id),
        json!({ "tree": snap.tree, "changed": snap.changed }),
    )?;

    let before = fingerprint(&snap.export)?;
    let (grec, _) = supervised(
        c.dir,
        &policy.gate.argv,
        &snap.export,
        policy.gate.timeout_secs.map(Duration::from_secs),
        gate::MAX_STREAM_BYTES,
        None,
    )?;
    c.emit(
        "gate_finished",
        Some(id),
        json!({ "outcome": grec.outcome, "exit_code": grec.exit_code, "log": grec.log }),
    )?;
    if grec.outcome == Outcome::Cancelled {
        return stop_now(c, id, "the gate");
    }
    if grec.outcome != Outcome::Passed {
        let reason = format!("gate_{:?}", grec.outcome).to_lowercase();
        return c.fail(id, &reason, "gate did not pass; nothing committed");
    }
    if grec.output_incomplete {
        return c.fail(id, "gate_output_incomplete", "a pipe outlived the gate");
    }
    if fingerprint(&snap.export).ok().as_ref() != Some(&before) {
        return c.fail(id, "export_modified", "the gate changed the exported tree");
    }
    if let Err(e) = repo.verify_snapshot_export(&snap) {
        return c.fail(id, "export_modified", &e.message);
    }
    match repo.capture(&scopes, &scratch) {
        Ok(again) if again.tree == snap.tree && again.head == snap.head => {}
        Ok(_) => {
            return c.fail(
                id,
                "candidate_changed",
                "the candidate changed while the gate ran",
            );
        }
        Err(e) => return c.fail(id, "candidate_changed", &e.message),
    }
    if c.ctl("STOP-NOW") {
        return stop_now(c, id, "verification, before the commit");
    }

    let label = if c.st.pilot { "lmloop pilot" } else { "lmloop" };
    let oid = match repo.commit_tree(&snap, &format!("{label}: {id}\n")) {
        Ok(o) => o,
        Err(e) => return c.fail(id, e.kind, &e.message),
    };
    c.st.intent = Some(Intent {
        task: id.into(),
        ref_name: snap.ref_name.clone(),
        head: snap.head.clone(),
        tree: snap.tree.clone(),
        commit: oid.clone(),
        session_id: summary.session_id,
        gate_log: grec.log.clone().unwrap_or_default(),
        changed: snap.changed.clone(),
    });
    c.st.phase = RunPhase::Committing;
    c.save()?; // durable before the ref moves
    c.emit(
        "commit_intent",
        Some(id),
        json!({ "commit": oid, "tree": snap.tree, "parent": snap.head }),
    )?;
    crash_point("before_cas");
    // On error the state stays `committing`; resume decides from the ref.
    repo.advance_ref(&snap, &oid)?;
    crash_point("after_cas");
    match finish_commit(c) {
        Ok(()) => Ok(None),
        Err(e) => unsynced(c, e).map(Some),
    }
}

/// The ref CAS landed but the guarded index sync was refused. Nothing is written or cleaned:
/// `run.json` keeps the intent (phase `committing`), and a held `index.lock` stays in place.
/// Any other error is returned unchanged.
fn unsynced(c: &Ctx, e: Error) -> Result<Finish> {
    if !["index_locked", "git_dirty", "git_hidden", "stale_head"].contains(&e.kind) {
        return Err(e);
    }
    let commit = c.st.intent.as_ref().map_or("?", |i| i.commit.as_str());
    let moved = if e.kind == "stale_head" {
        " (the branch is no longer at that commit)"
    } else {
        ""
    };
    let id = &c.st.run_id;
    let next = if c.st.pilot {
        "; a pilot run is one-shot and cannot be resumed".into()
    } else {
        format!(" before `fixture-resume {id}`")
    };
    Ok(Finish::NeedsReconciliation(format!(
        "run {id}: commit {commit} landed at the ref CAS and the intent is preserved, but the index was not synced: {}{moved}; inspect Git and any index.lock{next} (no lock is cleaned automatically)",
        e.message
    )))
}

/// The ref is at the intended commit: sync the index, record the task, move on. Idempotent.
fn finish_commit(c: &mut Ctx) -> Result<()> {
    let i =
        c.st.intent
            .clone()
            .ok_or_else(|| Error::new("state", "no commit intent"))?;
    // update-ref leaves the real index on the old tree, which would read as a dirty
    // baseline next task. Index only: the worktree already equals the tree.
    let active_repo = c.st.active_repo().to_path_buf();
    sync_index(&active_repo, &i)?;
    c.st.intent = None;
    c.st.done.push(Done {
        task: i.task.clone(),
        session_id: i.session_id,
        head: i.head,
        tree: i.tree,
        commit: i.commit.clone(),
        gate_log: i.gate_log,
        changed: i.changed.clone(),
    });
    c.st.next_task += 1;
    c.st.phase = RunPhase::Idle;
    c.save()?;
    let head =
        c.st.done
            .last()
            .map_or(i.commit.as_str(), |d| d.commit.as_str());
    let scopes =
        c.st.policy
            .tasks
            .get(c.st.next_task)
            .map_or(&[][..], |t| t.scopes.as_slice());
    write_handoff(c.dir, &render_handoff(&c.st.done, head, scopes))?;
    c.emit(
        "task_committed",
        Some(&i.task),
        json!({ "commit": i.commit, "changed": i.changed }),
    )
}

fn drive(c: &mut Ctx) -> Result<Finish> {
    while c.st.next_task < c.st.policy.tasks.len() {
        // The task boundary: the only place STOP and PAUSE act.
        if c.ctl("STOP-NOW") || c.ctl("STOP") {
            return c.halt(RunPhase::Stopped, None);
        }
        if c.ctl("PAUSE") {
            return c.halt(RunPhase::Paused, None);
        }
        if let Some(f) = task(c)? {
            return Ok(f);
        }
    }
    c.st.phase = RunPhase::Complete;
    c.save()?;
    c.emit("run_completed", None, json!({ "commits": c.st.done.len() }))?;
    Ok(Finish::Complete)
}

fn claim(dir: &Path, command: &str) -> Result<std::result::Result<lease::Lease, ()>> {
    Ok(match lease::claim(dir, command)? {
        Claim::Won(l) => Ok(l),
        Claim::Lost(_) => Err(()),
    })
}

/// Open (and with `repair`, first recover) the journal. A journal the bounded validator
/// refuses is a reconciliation case (exit 8), not an I/O failure; the log is left as found.
fn journal(
    dir: &Path,
    run_id: &str,
    repair: bool,
) -> Result<std::result::Result<EventLog, Finish>> {
    let opened = if repair {
        protocol::recover(dir, run_id)
    } else {
        Ok(())
    }
    .and_then(|()| EventLog::open(dir, run_id, !repair)); // only a new run creates it
    match opened {
        Ok(l) => Ok(Ok(l)),
        Err(e) if e.kind.starts_with("journal_") => Ok(Err(Finish::NeedsReconciliation(format!(
            "run {run_id}: {e}; events.jsonl, controls and run state were left as they are"
        )))),
        Err(e) => Err(e),
    }
}

pub fn start(store: &Store, run_id: &str, policy: Policy) -> Result<Finish> {
    begin(store, run_id, policy, false, |_| Ok(()))
}

/// One-shot native pilot of 1-3 serial tasks (a fresh worker session each, first failure stops
/// the run): same engine and gate; persisted as `pilot`, never resumable.
pub fn start_pilot(store: &Store, run_id: &str, policy: Policy) -> Result<Finish> {
    begin(store, run_id, policy, true, |_| Ok(()))
}

/// `start_pilot` with one hook, run once the checkout guard is held and before the run directory
/// exists. An `Err` ends the launch with nothing published; the guard is held until the run ends.
pub fn start_pilot_guarded(
    store: &Store,
    run_id: &str,
    policy: Policy,
    under_guard: impl FnOnce(&Policy) -> Result<()>,
) -> Result<Finish> {
    begin(store, run_id, policy, true, under_guard)
}

/// The checkout guard for `repo`, or `Err(())` when another process holds it. A lookup failure
/// is an error, never a skipped guard. Only a path git says is not a repository has nothing to
/// guard (a task then fails at `Repo::open`; a committing resume cannot see its commit land).
fn checkout(repo: &Path) -> Result<std::result::Result<Option<lease::Lease>, ()>> {
    let Some(git_dir) = git::checkout_dir(repo)? else {
        return Ok(Ok(None));
    };
    Ok(lease::claim_checkout(&git_dir)?.map(Some).ok_or(()))
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn check_worktree_bounds(
    store: &Store,
    policy: &Policy,
    wt_root: &Path,
) -> std::result::Result<(), String> {
    let state_base = store.base().parent().unwrap_or_else(|| store.base());
    let state_canon = fs::canonicalize(state_base).unwrap_or_else(|_| state_base.to_path_buf());
    if paths_overlap(wt_root, &state_canon) || paths_overlap(wt_root, store.base()) {
        return Err("worktree.root must not overlap the state root".into());
    }
    if let Some(wd) = &policy.worker.watchdog {
        let pf = fs::canonicalize(&wd.progress_file).unwrap_or_else(|_| {
            wd.progress_file
                .parent()
                .and_then(|p| fs::canonicalize(p).ok())
                .map_or_else(
                    || wd.progress_file.clone(),
                    |p| match wd.progress_file.file_name() {
                        Some(n) => p.join(n),
                        None => p,
                    },
                )
        });
        if paths_overlap(wt_root, &pf) {
            return Err("worktree.root must not overlap worker.watchdog.progress_file".into());
        }
    }
    Ok(())
}

fn begin(
    store: &Store,
    run_id: &str,
    policy: Policy,
    pilot: bool,
    under_guard: impl FnOnce(&Policy) -> Result<()>,
) -> Result<Finish> {
    // First, so a busy checkout leaves no run directory behind and worktree validation runs under guard.
    let base_checkout = match checkout(&policy.repo)? {
        Ok(g) => g,
        Err(()) => return Ok(Finish::CheckoutBusy),
    };
    // Pin and validate isolated worktree destination under the checkout guard before any one-shot hook.
    let pinned_wt_root = if let Some(w) = &policy.worktree {
        let (wt_root, _) =
            match git::precheck_worktree(&policy.repo, &w.root, run_id, &w.branch_prefix) {
                Ok(v) => v,
                Err(e) => return Ok(Finish::Usage(e.message)),
            };
        if let Err(msg) = check_worktree_bounds(store, &policy, &wt_root) {
            return Ok(Finish::Usage(msg));
        }
        Some(wt_root)
    } else {
        None
    };
    under_guard(&policy)?;
    let dir = store.run_dir(run_id, true)?.expect("created");
    let Ok(_lease) = claim(&dir, if pilot { "pilot-run" } else { "fixture-run" })? else {
        return Ok(Finish::OwnedElsewhere);
    };
    let _signals = gate::SignalScope::install();
    if state::read_bounded(&dir.join("run.json"))?.is_some() {
        // The existing run may be a pilot even when a fixture run asked: never point at resume.
        let one_shot = pilot
            || read_state(&dir, run_id)
                .ok()
                .flatten()
                .is_some_and(|st| st.pilot);
        return Ok(Finish::Usage(if one_shot {
            format!("run {run_id} already exists; a pilot run is one-shot, use a new run id")
        } else {
            format!("run {run_id} already exists; use fixture-resume")
        }));
    }
    let log = match journal(&dir, run_id, false)? {
        Ok(l) => l,
        Err(f) => return Ok(f),
    };
    let mut worktree_state = None;
    let _active_checkout = if let (Some(w), Some(wt_root)) = (&policy.worktree, pinned_wt_root) {
        let current = fs::canonicalize(&w.root).map_err(io_err(&w.root))?;
        if current != wt_root {
            return Err(Error::new(
                "worktree",
                "worktree root changed before creation",
            ));
        }
        check_worktree_bounds(store, &policy, &wt_root)
            .map_err(|msg| Error::new("worktree", msg))?;
        let created = git::create_worktree(&policy.repo, &wt_root, run_id, &w.branch_prefix)?;
        check_worktree_bounds(store, &policy, &created.path)
            .map_err(|msg| Error::new("worktree", msg))?;
        let wt_guard = match checkout(&created.path)? {
            Ok(g) => g,
            Err(()) => return Ok(Finish::CheckoutBusy),
        };
        // Release the base checkout guard immediately; all mutations stay in the linked worktree.
        drop(base_checkout);
        worktree_state = Some(WorktreeState {
            path: created.path,
            branch: created.branch,
            base_ref: created.base_ref,
            base_head: created.base_head,
        });
        wt_guard
    } else {
        base_checkout
    };
    let started_data = match &worktree_state {
        Some(w) => json!({
            "tasks": policy.tasks.len(),
            "pilot": pilot,
            "worktree_branch": w.branch,
            "base_head": w.base_head,
        }),
        None => json!({ "tasks": policy.tasks.len(), "pilot": pilot }),
    };
    let mut c = Ctx {
        dir: &dir,
        log,
        st: RunState {
            schema_version: RUN_SCHEMA_VERSION,
            run_id: run_id.into(),
            policy,
            phase: RunPhase::Idle,
            next_task: 0,
            done: Vec::new(),
            intent: None,
            failure: None,
            updated_at_unix: 0,
            pilot,
            worktree: worktree_state,
        },
    };
    c.save()?;
    c.emit("run_started", None, started_data)?;
    drive(&mut c)
}

pub fn resume(store: &Store, run_id: &str) -> Result<Finish> {
    let Some(dir) = store.run_dir(run_id, false)? else {
        return Ok(Finish::NoRun);
    };
    let Ok(_lease) = claim(&dir, "fixture-resume")? else {
        return Ok(Finish::OwnedElsewhere);
    };
    let _signals = gate::SignalScope::install();
    let Some(st) = read_state(&dir, run_id)? else {
        return Ok(Finish::NoRun);
    };
    if st.pilot {
        return Ok(Finish::NeedsReconciliation(format!(
            "run {run_id}: a pilot run is one-shot and cannot be resumed (its worktree is preserved); nothing was changed"
        )));
    }
    // Only a resume that may go on to write takes the checkout; complete/failed/stopped/lost
    // runs stay read-only refusals even while another process holds it.
    let _checkout = if matches!(
        st.phase,
        RunPhase::Paused | RunPhase::Idle | RunPhase::Committing
    ) {
        match checkout(st.active_repo())? {
            Ok(Some(g)) => Some(g),
            Ok(None) => {
                // A resume that may commit never goes on unguarded.
                return Ok(Finish::NeedsReconciliation(format!(
                    "run {run_id}: the run's repo is not a Git repository, so its checkout cannot be guarded; the journal, run.json and Git were not changed"
                )));
            }
            Err(()) => return Ok(Finish::CheckoutBusy),
        }
    } else {
        None
    };
    let refuse = |why: &str| {
        Ok(Finish::NeedsReconciliation(format!(
            "run {run_id}: {why}; nothing was changed"
        )))
    };
    // Only resumable phases reach the journal; every refusal below writes nothing.
    match st.phase {
        RunPhase::Complete => return Ok(Finish::Complete),
        RunPhase::Failed => return refuse("the run already failed (its worktree is preserved)"),
        RunPhase::Stopped => return refuse("the run was stopped (its worktree is preserved)"),
        RunPhase::Worker | RunPhase::Gate => {
            return refuse(
                "the previous owner was lost mid-task; the worktree may hold unverified work",
            );
        }
        RunPhase::Paused | RunPhase::Idle => {}
        RunPhase::Committing => {
            let i = st
                .intent
                .clone()
                .ok_or_else(|| Error::new("malformed", "committing without intent"))?;
            let repo = st.active_repo();
            let rev = |spec: &str| git(repo, &["rev-parse", "--verify", "-q", spec]).ok();
            let attached = git(repo, &["symbolic-ref", "-q", "HEAD"]).ok().as_deref()
                == Some(i.ref_name.as_str());
            let landed = attached
                && rev(&i.ref_name).as_deref() == Some(i.commit.as_str())
                && rev(&format!("{}^", i.commit)).as_deref() == Some(i.head.as_str())
                && rev(&format!("{}^{{tree}}", i.commit)).as_deref() == Some(i.tree.as_str());
            if !landed {
                // Nothing is written: run.json keeps the intent, and the commit object stays in Git.
                return refuse(&format!(
                    "a commit ({}) was intended but the ref is not at it (CAS did not complete, or the ref moved)",
                    i.commit
                ));
            }
        }
    }
    // Under the lease, before controls or the engine change: repair a torn journal tail.
    let log = match journal(&dir, run_id, true)? {
        Ok(l) => l,
        Err(f) => return Ok(f),
    };
    let phase = st.phase;
    let mut c = Ctx { dir: &dir, log, st };
    match phase {
        RunPhase::Paused => {
            // Resuming is the operator's answer to PAUSE; a leftover file must not re-pause us.
            archive_controls(&dir, &["PAUSE"])?;
            c.st.phase = RunPhase::Idle;
            c.st.failure = None;
            c.save()?;
            c.emit("run_resumed", None, json!({ "next_task": c.st.next_task }))?;
        }
        RunPhase::Committing => {
            let i =
                c.st.intent
                    .clone()
                    .ok_or_else(|| Error::new("malformed", "committing without intent"))?;
            match finish_commit(&mut c) {
                Ok(()) => {}
                Err(e) => return unsynced(&c, e),
            }
            c.emit(
                "cas_recovered",
                Some(&i.task),
                json!({ "commit": i.commit }),
            )?;
        }
        _ => {}
    }
    drive(&mut c)
}

/// Read-only status for a serial run; `None` if the directory holds none.
pub fn status(dir: &Path, run_id: &str) -> Result<Option<(Value, String)>> {
    let Some(st) = read_state(dir, run_id)? else {
        return Ok(None);
    };
    let lease = lease::peek(dir)?;
    let owner_lost = matches!(
        st.phase,
        RunPhase::Worker | RunPhase::Gate | RunPhase::Committing
    ) && lease.state != lease::LeaseState::Held;
    let text = format!(
        "run: {run_id} ({}; not production evidence)\nphase: {:?}{}\ntasks committed: {}/{}\nlast commit: {}\nfailure: {}",
        if st.pilot {
            "serial pilot, gate-checked commit; not production verification"
        } else {
            "serial fixture runner"
        },
        st.phase,
        if owner_lost && st.pilot {
            " (owner lost: needs reconciliation; a pilot cannot be resumed)"
        } else if owner_lost {
            " (owner lost: needs reconciliation or resume)"
        } else {
            ""
        },
        st.done.len(),
        st.policy.tasks.len(),
        st.done.last().map_or("none", |d| d.commit.as_str()),
        st.failure.as_ref().map_or("none".into(), |f| format!(
            "{}: {} ({})",
            f.task, f.reason, f.detail
        )),
    );
    let watchdog = read_watchdog(dir).ok().flatten();
    let text = match &watchdog {
        Some(w) => format!("{text}\n{}", watchdog_line(w)),
        None => text,
    };
    let extra = json!({
        "run_id": run_id,
        "watchdog": watchdog,
        "kind": if st.pilot { "serial_pilot" } else { "serial_fixture" },
        "run": st,
        "lease": lease,
        "owner_lost": owner_lost,
    });
    Ok(Some((extra, text)))
}

/// One plain line for the CLI and TUI. Times are wall-clock facts the writer recorded.
pub fn watchdog_line(w: &WatchdogEvidence) -> String {
    let at = |t: Option<u64>| t.map_or("never".into(), |t| format!("unix {t}"));
    format!(
        "watchdog: {} task {} model records {} (last {}); completed tools {} (last {:?}, {}); startup {}s idle {}s{}{}",
        w.state,
        w.task,
        w.model_pulses,
        at(w.last_pulse_unix),
        w.tools_completed,
        w.last_tool,
        w.tools_observed,
        w.startup_secs,
        w.idle_secs,
        w.trip
            .as_ref()
            .map_or(String::new(), |t| format!("; TRIPPED: {t}")),
        w.warning
            .as_ref()
            .map_or(String::new(), |t| format!("; WARNING: {t}")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lmloop-serial-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn mkfifo(p: &Path) {
        let c = CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn scoped_fingerprint_is_unknown_for_anything_ambiguous_and_never_blocks() {
        let root = tmp("fp");
        let (repo, outside) = (root.join("repo"), root.join("outside"));
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(repo.join("src/a.txt"), "a").unwrap();
        fs::write(outside.join("secret"), "s").unwrap();
        let fp = |s: &[&str]| {
            let v: Vec<String> = s.iter().map(|s| s.to_string()).collect();
            scoped_fingerprint(&repo, &v)
        };
        let known = fp(&["src/a.txt", "src/absent.txt"]);
        assert!(known.is_some());
        fs::write(repo.join("src/a.txt"), "b").unwrap();
        assert_ne!(
            fp(&["src/a.txt", "src/absent.txt"]),
            known,
            "content change is seen"
        );
        // An intermediate symlink escaping the checkout, a leaf symlink, a directory scope, a
        // FIFO (the open must neither follow nor wait), and odd paths are all "unknown".
        symlink(&outside, repo.join("esc")).unwrap();
        symlink(outside.join("secret"), repo.join("src/leaf")).unwrap();
        mkfifo(&repo.join("src/fifo"));
        for bad in [
            "esc/secret",
            "src/leaf",
            "src",
            "src/fifo",
            "../x",
            "/etc/passwd",
        ] {
            assert_eq!(fp(&[bad]), None, "{bad}");
        }
        // Too many scopes or too many bytes: bounded, so unknown.
        let many = vec!["src/a.txt"; WATCH_FP_SCOPES + 1];
        assert_eq!(fp(&many), None);
        fs::write(repo.join("big"), vec![0u8; WATCH_FP_BYTES as usize + 1]).unwrap();
        assert_eq!(fp(&["big"]), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn export_fingerprint_does_not_follow_or_block_on_a_worker_made_entry() {
        let root = tmp("export");
        fs::write(root.join("f"), "x").unwrap();
        assert!(fingerprint_in(&root, 10, 1 << 10).is_ok());
        mkfifo(&root.join("pipe"));
        assert_eq!(
            fingerprint_in(&root, 10, 1 << 10).unwrap_err().kind,
            "export_mismatch"
        );
        let _ = fs::remove_dir_all(&root);
    }

    fn record() -> GateRecord {
        GateRecord {
            outcome: Outcome::Passed,
            program: None,
            argv_len: 1,
            cwd: String::new(),
            exit_code: Some(0),
            signal: None,
            timeout_secs: 0,
            duration_ms: 0,
            log: None,
            log_truncated: false,
            output_incomplete: false,
            detail: None,
        }
    }

    #[test]
    fn worker_stdout_is_parsed_directly_so_log_markers_cannot_forge_or_hide_it() {
        let ev = |k: &str, r: &str| {
            format!(
                "{{\"type\":\"{k}\",\"sessionID\":\"ses_a\",\"part\":{{\"reason\":\"{r}\"}}}}\n"
            )
        };
        // A marker-looking line inside the stream makes it unparseable, never a different stream.
        let ok = ev("step_start", "") + &ev("step_finish", "stop");
        assert!(worker_verdict(&record(), ok.as_bytes(), &[]).is_ok());
        let noisy = format!("--- stdout ---\n{ok}\n--- stderr ---\n");
        assert!(worker_verdict(&record(), noisy.as_bytes(), &[]).is_err());
        assert!(worker_verdict(&record(), &[0xff, 0xfe], &[]).is_err());
        let mut cut = record();
        cut.log_truncated = true;
        assert!(worker_verdict(&cut, ok.as_bytes(), &[]).is_err());
    }
}
