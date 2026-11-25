//! Operator control API (M3 vertical slice): the one local seam a TUI and a web
//! server both call. It reads the same native run store, lease, status and
//! control files the engine uses and adds no second source of truth.
//!
//! * Views are plain serde structs. Anything that came from a worker, a log or
//!   a note is stripped of control characters and bounded before it is exposed.
//! * Availability is computed from the persisted state and the lease, and the
//!   same computation gates every action, so a UI cannot offer what the API
//!   would refuse. An unavailable action fails with kind `unavailable` and
//!   carries the reason.
//! * `pause`, `stop` and `stop_now` only drop the control file the engine
//!   already honours; the lease holder acts on it and archives it.
//! * `resume` calls the engine's `serial::resume`. It blocks for as long as the
//!   resumed run takes: call it from a worker thread (`spawn_blocking`), never
//!   from a UI or async executor thread. While it runs, SIGTERM/SIGINT/SIGHUP in
//!   this process are the engine's (they tear down the worker), not the host's.
//! * `approve` / `request_changes` record `decision.json` and nothing else: no
//!   Git change, no run-state change, no completion claim. A decision is final
//!   per run. Approval is offered only for a finished run with every task
//!   committed, and a fixture run is never production evidence.
//!
//! shortcut: a control written just as the owner exits stays in the run directory
//! and is honoured by the next owner (STOP/STOP-NOW end the resumed run at its first
//! boundary); upgrade by clearing stale controls at resume if that surprises anyone.
//! Bidi override characters are not stripped from text views.

use crate::lease::{self, Claim, LeaseInfo, LeaseState};
use crate::serial::{self, Finish, RunPhase, RunState};
use crate::state::{self, Error, Phase, Result, Snapshot, Store, io_err};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const MAX_NOTE_BYTES: usize = 2000;
/// Hard cap on one `read_log` answer, whatever the caller asks for.
pub const MAX_LOG_TAIL: usize = 64 * 1024;
const MAX_LOGS: usize = 50;
const MAX_RUNS: usize = 100;
const EVENT_TAIL_BYTES: u64 = 256 * 1024;
const EVENT_TAIL_LINES: usize = 50;
const EVENT_DATA_CHARS: usize = 300;
const DECISION_FILE: &str = "decision.json";
const DECISION_EFFECT: &str = "recorded_only_no_git_change";
/// What a finished serial run is worth: operator-gated and committed, but fixture-only.
pub const SERIAL_EVIDENCE: &str = "fixture_runner_not_production_evidence";
/// A one-shot `pilot-run`: gate-checked commit, still not production verification.
pub const PILOT_EVIDENCE: &str = "serial_pilot_not_production_verification";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Pause,
    Stop,
    StopNow,
}

impl Control {
    fn file(self) -> &'static str {
        match self {
            Control::Pause => "PAUSE",
            Control::Stop => "STOP",
            Control::StopNow => "STOP-NOW",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approved,
    ChangesRequested,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRecord {
    pub schema_version: u32,
    pub run_id: String,
    pub decision: Decision,
    pub note: String,
    pub at_unix: u64,
    /// Always `recorded_only_no_git_change`.
    pub effect: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Available {
    pub available: bool,
    /// Why not, or what the action will do.
    pub reason: String,
}

impl Available {
    fn yes(reason: &str) -> Self {
        Self {
            available: true,
            reason: reason.into(),
        }
    }
    fn no(reason: &str) -> Self {
        Self {
            available: false,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Actions {
    pub pause: Available,
    pub stop: Available,
    pub stop_now: Available,
    pub resume: Available,
    pub approve: Available,
    pub request_changes: Available,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    SerialFixture,
    /// One-shot `pilot-run`: never resumable; only Stop now interrupts it.
    SerialPilot,
    DiagnosticGate,
    /// The directory exists but its state could not be read; see `problem`.
    Invalid,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub run_id: String,
    pub kind: RunKind,
    /// Snake-case engine phase; a diagnostic run whose owner died reads `interrupted`.
    pub phase: String,
    pub lease: Option<LeaseInfo>,
    pub owner_lost: bool,
    pub tasks_done: usize,
    pub tasks_total: usize,
    pub failure: Option<String>,
    /// Control files waiting for the owner: PAUSE, STOP, STOP-NOW.
    pub pending_controls: Vec<String>,
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Committed,
    Running,
    Failed,
    Pending,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    pub id: String,
    pub state: TaskState,
    pub commit: Option<String>,
    /// Name for `read_log`; only committed tasks have one.
    pub gate_log: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventView {
    pub seq: u64,
    pub at_unix: u64,
    pub kind: String,
    pub task: Option<String>,
    /// Compact JSON of the event payload, clipped.
    pub data: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    #[serde(flatten)]
    pub summary: RunSummary,
    pub actions: Actions,
    pub tasks: Vec<TaskView>,
    /// True only for a serial run in `complete` with every task committed.
    pub complete: bool,
    pub evidence: String,
    pub decision: Option<DecisionRecord>,
    /// Log file names, oldest first, at most the newest 50.
    pub logs: Vec<String>,
    /// The last 50 events, oldest first.
    pub events: Vec<EventView>,
    /// Latest worker-watchdog evidence (kept after a failure), if the run used one.
    pub watchdog: Option<serial::WatchdogEvidence>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogTail {
    pub name: String,
    pub text: String,
    pub size: u64,
    /// Earlier bytes of the file were left out.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Receipt {
    pub run_id: String,
    pub action: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResumeOutcome {
    pub run_id: String,
    /// complete | task_failed | stopped | paused | owned_elsewhere | checkout_busy | needs_reconciliation | no_run | usage
    pub outcome: &'static str,
    pub message: String,
}

#[allow(clippy::large_enum_variant)] // one short-lived value per inspect; boxing only moves the size
enum Loaded {
    Serial(RunState),
    Diagnostic(Snapshot),
}

struct Inspected {
    loaded: Loaded,
    lease: LeaseInfo,
    pending: Vec<String>,
    decision: Option<DecisionRecord>,
    warnings: Vec<String>,
    actions: Actions,
    owner_lost: bool,
}

pub struct Operator {
    store: Store,
}

impl Operator {
    /// `root` is a state root as returned by `state::resolve_root`.
    pub fn new(root: &Path) -> Self {
        Self {
            store: Store::new(root),
        }
    }

    /// Newest-modified runs first, at most 100. An unreadable run is listed as `invalid`.
    pub fn list_runs(&self) -> Result<Vec<RunSummary>> {
        self.list(None)
    }

    /// As `list_runs`, but only serial runs whose canonical repo is exactly `repo` (already
    /// canonical). Invalid and diagnostic runs name no repo, so they are left out.
    /// shortcut: filters the newest 100 runs of the root; a dedicated pilot root is expected.
    pub fn list_runs_in(&self, repo: &Path) -> Result<Vec<RunSummary>> {
        self.list(Some(repo))
    }

    fn list(&self, repo: Option<&Path>) -> Result<Vec<RunSummary>> {
        let runs = self.store.base().join("runs");
        if !state::private_dir(self.store.base())? || !state::private_dir(&runs)? {
            return Ok(Vec::new());
        }
        let mut found = Vec::new();
        for entry in fs::read_dir(&runs).map_err(io_err(&runs))? {
            let entry = entry.map_err(io_err(&runs))?;
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if state::validate_run_id(&name).is_ok() && meta.is_dir() {
                found.push((meta.modified().ok(), name));
            }
        }
        found.sort_by(|a, b| b.cmp(a));
        found.truncate(MAX_RUNS);
        Ok(found
            .into_iter()
            .filter_map(|(_, id)| match (self.inspect_run(&id), repo) {
                (Ok((_, ins)), None) => Some(summarize(&id, &ins)),
                (Ok((_, ins)), Some(r)) => match &ins.loaded {
                    Loaded::Serial(st)
                        if fs::canonicalize(&st.policy.repo).is_ok_and(|c| c == r) =>
                    {
                        Some(summarize(&id, &ins))
                    }
                    _ => None,
                },
                (Err(e), None) => Some(invalid_summary(&id, &e)),
                (Err(_), Some(_)) => None,
            })
            .collect())
    }

    /// Full read-only view of one run: state, availability, tasks, evidence, logs, recent events.
    pub fn view(&self, run_id: &str) -> Result<RunView> {
        let (id, dir) = self.dir(run_id)?;
        let lease = lease::peek(&dir)?;
        let mut ins = self.inspect(&id, &dir, lease)?;
        let mut warnings = std::mem::take(&mut ins.warnings);
        let logs = list_logs(&dir).unwrap_or_else(|e| {
            warnings.push(clean(&e.to_string(), false));
            Vec::new()
        });
        let events = recent_events(&dir).unwrap_or_else(|e| {
            warnings.push(clean(&e.to_string(), false));
            Vec::new()
        });
        let watchdog = serial::read_watchdog(&dir).unwrap_or_else(|e| {
            warnings.push(clean(&e.to_string(), false));
            None
        });
        let (tasks, complete, evidence) = match &ins.loaded {
            Loaded::Serial(st) => (
                tasks(st),
                st.phase == RunPhase::Complete && st.done.len() == st.policy.tasks.len(),
                if st.pilot {
                    PILOT_EVIDENCE
                } else {
                    SERIAL_EVIDENCE
                },
            ),
            Loaded::Diagnostic(_) => (Vec::new(), false, state::EVIDENCE),
        };
        Ok(RunView {
            summary: summarize(&id, &ins),
            actions: ins.actions,
            tasks,
            complete,
            evidence: evidence.into(),
            decision: ins.decision,
            logs,
            events,
            watchdog,
            warnings,
        })
    }

    /// The last `max_bytes` (capped at `MAX_LOG_TAIL`) of one file under the run's `logs/`.
    /// `name` is a listed log name, or a task's `gate_log` (`logs/<name>`).
    pub fn read_log(&self, run_id: &str, name: &str, max_bytes: usize) -> Result<LogTail> {
        let (_, dir) = self.dir(run_id)?;
        let name = name.strip_prefix("logs/").unwrap_or(name);
        let simple = !name.is_empty()
            && name.len() <= 128
            && !name.starts_with('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !simple {
            return Err(Error::new("usage", "log name must be a simple file name"));
        }
        let logs = dir.join("logs");
        let path = logs.join(name);
        let tail = if state::private_dir(&logs)? {
            tail_bytes(&path, max_bytes.min(MAX_LOG_TAIL) as u64)?
        } else {
            None
        };
        let (bytes, size) = tail.ok_or_else(|| Error::new("no_log", format!("no log {name}")))?;
        Ok(LogTail {
            name: name.into(),
            text: clean(&String::from_utf8_lossy(&bytes), true),
            size,
            truncated: size > bytes.len() as u64,
        })
    }

    /// Ask the lease holder to pause, stop or stop-now. Writes the control file only.
    pub fn control(&self, run_id: &str, control: Control) -> Result<Receipt> {
        let (id, dir) = self.dir(run_id)?;
        let ins = self.inspect(&id, &dir, lease::peek(&dir)?)?;
        let a = match control {
            Control::Pause => &ins.actions.pause,
            Control::Stop => &ins.actions.stop,
            Control::StopNow => &ins.actions.stop_now,
        };
        if !a.available {
            return Err(Error::new("unavailable", a.reason.clone()));
        }
        let path = dir.join(control.file());
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(_) => {}
            // A concurrent operator asked for the same thing; the request stands.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(&path)(e)),
        }
        Ok(Receipt {
            run_id: id,
            action: control.file(),
            message: format!("{}: {}", control.file(), a.reason),
        })
    }

    /// Record an explicit operator decision. Takes the run lease while it checks and
    /// writes, so it cannot race the engine; changes nothing but `decision.json`.
    pub fn decide(&self, run_id: &str, decision: Decision, note: &str) -> Result<Receipt> {
        if note.len() > MAX_NOTE_BYTES {
            return Err(Error::new(
                "usage",
                format!("note is over {MAX_NOTE_BYTES} bytes"),
            ));
        }
        let note = clean(note.trim(), true);
        if decision == Decision::ChangesRequested && note.is_empty() {
            return Err(Error::new(
                "note_required",
                "say what needs to change; an empty request is not recorded",
            ));
        }
        let pick = |a: &Actions| match decision {
            Decision::Approved => a.approve.clone(),
            Decision::ChangesRequested => a.request_changes.clone(),
        };
        let refuse = |a: Available| Error::new("unavailable", a.reason);
        let (id, dir) = self.dir(run_id)?;
        let first = pick(&self.inspect(&id, &dir, lease::peek(&dir)?)?.actions);
        if !first.available {
            return Err(refuse(first));
        }
        let _lease = match lease::claim(&dir, "operator-decision")? {
            Claim::Won(l) => l,
            Claim::Lost(_) => {
                return Err(Error::new("unavailable", "run is owned by a live process"));
            }
        };
        // We hold the only lock now, so the lease reads as free; recheck under it.
        let free = LeaseInfo {
            state: LeaseState::Free,
            pid: None,
            command: None,
            started_at_unix: None,
        };
        let again = pick(&self.inspect(&id, &dir, free)?.actions);
        if !again.available {
            return Err(refuse(again));
        }
        let record = DecisionRecord {
            schema_version: 1,
            run_id: id.clone(),
            decision,
            note,
            at_unix: state::now_unix(),
            effect: DECISION_EFFECT.into(),
        };
        publish_decision(&dir, &record)?;
        Ok(Receipt {
            run_id: id,
            action: match decision {
                Decision::Approved => "approve",
                Decision::ChangesRequested => "request_changes",
            },
            message: "decision recorded; no Git change was made".into(),
        })
    }

    /// Continue a paused (or owner-lost between tasks) fixture run through the engine.
    /// BLOCKING for the whole resumed run; see the module note.
    pub fn resume(&self, run_id: &str) -> Result<ResumeOutcome> {
        let (id, dir) = self.dir(run_id)?;
        let ins = self.inspect(&id, &dir, lease::peek(&dir)?)?;
        if !ins.actions.resume.available {
            return Err(Error::new("unavailable", ins.actions.resume.reason));
        }
        Ok(outcome(
            id.clone(),
            serial::resume(&self.store, &id)?,
            false,
        ))
    }

    /// Run one pilot task to the end through the engine, as `pilot-run` does. Blocks like
    /// `resume`: call it from a worker thread. The caller owns the guards (explicit state root,
    /// repo overlap) and passes a policy it loaded once; nothing here rereads a manifest.
    pub fn launch_pilot(&self, run_id: &str, policy: serial::Policy) -> Result<ResumeOutcome> {
        let id = state::validate_run_id(run_id).map_err(|m| Error::new("usage", m))?;
        Ok(outcome(
            id.clone(),
            serial::start_pilot(&self.store, &id, policy)?,
            true,
        ))
    }

    /// `launch_pilot` with a hook that runs under the engine's checkout guard, before the run
    /// directory exists (see `serial::start_pilot_guarded`).
    pub fn launch_pilot_guarded(
        &self,
        run_id: &str,
        policy: serial::Policy,
        under_guard: impl FnOnce(&serial::Policy) -> Result<()>,
    ) -> Result<ResumeOutcome> {
        let id = state::validate_run_id(run_id).map_err(|m| Error::new("usage", m))?;
        Ok(outcome(
            id.clone(),
            serial::start_pilot_guarded(&self.store, &id, policy, under_guard)?,
            true,
        ))
    }

    fn dir(&self, run_id: &str) -> Result<(String, PathBuf)> {
        let id = state::validate_run_id(run_id).map_err(|m| Error::new("usage", m))?;
        match self.store.run_dir(&id, false)? {
            Some(dir) => Ok((id, dir)),
            None => Err(Error::new("no_run", format!("no such run: {id}"))),
        }
    }

    fn inspect_run(&self, id: &str) -> Result<(PathBuf, Inspected)> {
        let (id, dir) = self.dir(id)?;
        let lease = lease::peek(&dir)?;
        let ins = self.inspect(&id, &dir, lease)?;
        Ok((dir, ins))
    }

    fn inspect(&self, id: &str, dir: &Path, lease: LeaseInfo) -> Result<Inspected> {
        let loaded = match serial::read_state(dir, id)? {
            Some(st) => Loaded::Serial(st),
            None => Loaded::Diagnostic(state::read_snapshot(dir, id)?),
        };
        let held = lease.state == LeaseState::Held;
        let pending: Vec<String> = ["PAUSE", "STOP", "STOP-NOW"]
            .iter()
            .filter(|n| fs::symlink_metadata(dir.join(n)).is_ok())
            .map(|n| n.to_string())
            .collect();
        let mut warnings = Vec::new();
        // An unreadable decision file still counts as a decision: never record over it.
        let (decision, decided) = match read_decision(dir, id) {
            Ok(d) => (d.clone(), d.is_some()),
            Err(e) => {
                warnings.push(clean(&e.to_string(), false));
                (None, true)
            }
        };
        let owner_lost = match &loaded {
            Loaded::Serial(st) => {
                matches!(
                    st.phase,
                    RunPhase::Worker | RunPhase::Gate | RunPhase::Committing
                ) && !held
            }
            Loaded::Diagnostic(s) => s.phase == Phase::GateRunning && !held,
        };
        let actions = actions(&loaded, held, &pending, decided);
        Ok(Inspected {
            loaded,
            lease,
            pending,
            decision,
            warnings,
            actions,
            owner_lost,
        })
    }
}

fn actions(loaded: &Loaded, held: bool, pending: &[String], decided: bool) -> Actions {
    let Loaded::Serial(st) = loaded else {
        let no = Available::no(
            "diagnostic-gate run: it honours no controls, resumes nothing and verifies nothing (not bound to a Git snapshot)",
        );
        return Actions {
            pause: no.clone(),
            stop: no.clone(),
            stop_now: no.clone(),
            resume: no.clone(),
            approve: no.clone(),
            request_changes: no,
        };
    };
    use RunPhase::*;
    let phase = st.phase;
    let active = matches!(phase, Idle | Worker | Gate | Committing);
    let has = |n: &str| pending.iter().any(|p| p == n);
    let live = |blocked: Option<&str>, will: &str| {
        if !held {
            Available::no(
                "not owned by a live process; controls are only honoured by the lease holder",
            )
        } else if !active {
            Available::no(&format!("run is {phase:?}").to_lowercase())
        } else if let Some(b) = blocked {
            Available::no(b)
        } else {
            Available::yes(will)
        }
    };
    let stop_pending = has("STOP") || has("STOP-NOW");
    let pause = live(
        if has("PAUSE") {
            Some("PAUSE already requested")
        } else if stop_pending {
            Some("a stop is already requested")
        } else {
            None
        },
        "parks the run at the next task boundary; resume continues it",
    );
    let stop = live(
        stop_pending.then_some("a stop is already requested"),
        "finishes the current task, then stops; any commits already made are preserved",
    );
    let stop_now = live(
        has("STOP-NOW").then_some("STOP-NOW already requested"),
        "interrupts only the in-flight worker or gate (not the commit window); any commits already made are preserved",
    );
    let resume = if held {
        Available::no("owned by a live process")
    } else {
        match phase {
            Paused => Available::yes("continues from the persisted manifest"),
            Idle => Available::yes("the owner exited between tasks; the engine continues"),
            Committing => Available::yes(
                "the engine completes a landed commit, or refuses without changing anything",
            ),
            Worker | Gate => Available::no(
                "the owner was lost mid-task and the worktree may hold unverified work; needs manual reconciliation",
            ),
            Failed | Stopped => Available::no("terminal; the worktree is preserved for inspection"),
            Complete => Available::no("already complete"),
        }
    };
    let blocked = if held {
        Some("run is owned by a live process")
    } else if decided {
        Some("a decision is already recorded")
    } else {
        None
    };
    let approve = if let Some(b) = blocked {
        Available::no(b)
    } else if phase != Complete || st.done.len() != st.policy.tasks.len() || st.failure.is_some() {
        Available::no("run is not complete: approval needs every task committed")
    } else {
        Available::yes(if st.pilot {
            "records approval only: no Git change; pilot evidence is not production verification"
        } else {
            "records approval only: no Git change; fixture evidence is not production verification"
        })
    };
    let request_changes = if let Some(b) = blocked {
        Available::no(b)
    } else if !matches!(phase, Complete | Failed | Stopped) {
        Available::no("run is not finished; pause or stop it first")
    } else {
        Available::yes("records the request only; nothing is changed, discarded or relaunched")
    };
    // A pilot has one task and is never resumable: no boundary to pause or stop at, nothing to
    // resume. STOP-NOW (interrupts the in-flight worker or gate) is its only control.
    let one_shot = |a: Available| {
        if st.pilot && a.available {
            Available::no(
                "one-shot pilot run: it cannot be paused, stopped at a boundary or resumed; Stop now interrupts it",
            )
        } else {
            a
        }
    };
    Actions {
        pause: one_shot(pause),
        stop: one_shot(stop),
        stop_now,
        resume: one_shot(resume),
        approve,
        request_changes,
    }
}

fn outcome(run_id: String, f: Finish, pilot: bool) -> ResumeOutcome {
    let (outcome, message) = match f {
        Finish::Complete if pilot => (
            "complete",
            "pilot task committed after the operator gate passed (pilot evidence; not production verification)".into(),
        ),
        Finish::Complete => ("complete", "all tasks committed (fixture runner)".into()),
        Finish::TaskFailed => (
            "task_failed",
            "a task failed; nothing was committed for it".into(),
        ),
        Finish::Stopped => (
            "stopped",
            "stopped; nothing further committed, worktree preserved".into(),
        ),
        Finish::Paused => ("paused", "paused again at a task boundary".into()),
        Finish::OwnedElsewhere => (
            "owned_elsewhere",
            "owned by another process; nothing was changed".into(),
        ),
        Finish::CheckoutBusy => (
            "checkout_busy",
            "another lmloop process holds this Git checkout; the journal, run.json and Git were not changed".into(),
        ),
        Finish::NeedsReconciliation(m) => ("needs_reconciliation", clean(&m, false)),
        Finish::NoRun => ("no_run", "no such fixture run".into()),
        Finish::Usage(m) => ("usage", clean(&m, false)),
    };
    ResumeOutcome {
        run_id,
        outcome,
        message,
    }
}

fn snake<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// Worker, log and operator text is untrusted terminal input: no control characters.
pub fn clean(s: &str, multiline: bool) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() && !(multiline && matches!(c, '\n' | '\t')) {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

fn invalid_summary(id: &str, e: &Error) -> RunSummary {
    RunSummary {
        run_id: id.into(),
        kind: RunKind::Invalid,
        phase: "invalid".into(),
        lease: None,
        owner_lost: false,
        tasks_done: 0,
        tasks_total: 0,
        failure: None,
        pending_controls: Vec::new(),
        problem: Some(clean(&e.to_string(), false)),
    }
}

fn summarize(id: &str, ins: &Inspected) -> RunSummary {
    let (kind, phase, done, total, failure) = match &ins.loaded {
        Loaded::Serial(st) => (
            if st.pilot {
                RunKind::SerialPilot
            } else {
                RunKind::SerialFixture
            },
            snake(&st.phase),
            st.done.len(),
            st.policy.tasks.len(),
            st.failure
                .as_ref()
                .map(|f| clean(&format!("{}: {} ({})", f.task, f.reason, f.detail), false)),
        ),
        Loaded::Diagnostic(s) => (
            RunKind::DiagnosticGate,
            if ins.owner_lost {
                "interrupted".into()
            } else {
                snake(&s.phase)
            },
            0,
            0,
            None,
        ),
    };
    RunSummary {
        run_id: id.into(),
        kind,
        phase,
        lease: Some(ins.lease.clone()),
        owner_lost: ins.owner_lost,
        tasks_done: done,
        tasks_total: total,
        failure,
        pending_controls: ins.pending.clone(),
        problem: None,
    }
}

fn tasks(st: &RunState) -> Vec<TaskView> {
    st.policy
        .tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let done = st.done.get(i);
            let state = if done.is_some() {
                TaskState::Committed
            } else if i == st.next_task {
                match st.phase {
                    RunPhase::Worker | RunPhase::Gate | RunPhase::Committing => TaskState::Running,
                    RunPhase::Failed => TaskState::Failed,
                    _ => TaskState::Pending,
                }
            } else {
                TaskState::Pending
            };
            TaskView {
                id: t.id.clone(),
                state,
                commit: done.map(|d| d.commit.clone()),
                gate_log: done.map(|d| clean(&d.gate_log, false)),
            }
        })
        .collect()
}

/// Newest `MAX_LOGS` regular `*.log` files with simple names, oldest first.
fn list_logs(dir: &Path) -> Result<Vec<String>> {
    let logs = dir.join("logs");
    if !state::private_dir(&logs)? {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&logs).map_err(io_err(&logs))? {
        let entry = entry.map_err(io_err(&logs))?;
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        let simple = name.len() <= 128
            && name.ends_with(".log")
            && !name.starts_with('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if simple && fs::symlink_metadata(entry.path()).is_ok_and(|m| m.is_file()) {
            names.push(name);
        }
    }
    names.sort();
    names.drain(..names.len().saturating_sub(MAX_LOGS));
    Ok(names)
}

/// The last `max` bytes of a private regular file, with the file's full size; `None` if absent.
fn tail_bytes(path: &Path, max: u64) -> Result<Option<(Vec<u8>, u64)>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(path)(e)),
    };
    state::check_file(&file, path)?;
    let size = file.metadata().map_err(io_err(path))?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(max)))
        .map_err(io_err(path))?;
    let mut buf = Vec::new();
    file.take(max).read_to_end(&mut buf).map_err(io_err(path))?;
    Ok(Some((buf, size)))
}

fn recent_events(dir: &Path) -> Result<Vec<EventView>> {
    let Some((bytes, size)) = tail_bytes(&dir.join("events.jsonl"), EVENT_TAIL_BYTES)? else {
        return Ok(Vec::new());
    };
    let text = String::from_utf8_lossy(&bytes);
    // A tail that starts mid-file starts mid-line: drop that fragment.
    let skip = usize::from(size > bytes.len() as u64);
    let lines: Vec<&str> = text.lines().skip(skip).collect();
    let from = lines.len().saturating_sub(EVENT_TAIL_LINES);
    Ok(lines[from..]
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| EventView {
            seq: v["seq"].as_u64().unwrap_or(0),
            at_unix: v["at_unix"].as_u64().unwrap_or(0),
            kind: clean(v["kind"].as_str().unwrap_or(""), false),
            task: v["task"].as_str().map(|t| clean(t, false)),
            data: v["data"]
                .to_string()
                .chars()
                .take(EVENT_DATA_CHARS)
                .collect(),
        })
        .collect())
}

fn read_decision(dir: &Path, id: &str) -> Result<Option<DecisionRecord>> {
    let Some(text) = state::read_bounded(&dir.join(DECISION_FILE))? else {
        return Ok(None);
    };
    let rec: DecisionRecord = serde_json::from_str(&text)
        .map_err(|e| Error::new("malformed", format!("{DECISION_FILE}: {e}")))?;
    if rec.schema_version != 1 || rec.run_id != id || rec.effect != DECISION_EFFECT {
        return Err(Error::new(
            "malformed",
            format!("{DECISION_FILE} is not a decision for this run"),
        ));
    }
    Ok(Some(rec))
}

/// Exclusive atomic publish: write a temp file, then hard-link it into place, which
/// fails if a decision already exists.
fn publish_decision(dir: &Path, rec: &DecisionRecord) -> Result<()> {
    let tmp = dir.join(format!("{DECISION_FILE}.{}.tmp", std::process::id()));
    let dest = dir.join(DECISION_FILE);
    let mut body = serde_json::to_vec_pretty(rec).map_err(|e| Error::new("io", e.to_string()))?;
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
    let linked = fs::hard_link(&tmp, &dest);
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::new("unavailable", "a decision is already recorded"));
        }
        Err(e) => return Err(io_err(&dest)(e)),
    }
    fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_err(dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::Command;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::var_os("LMLOOP_TEST_TMP")
                .map_or_else(|| PathBuf::from("/tmp/opencode"), PathBuf::from);
            let dir = base.join(format!("lmloop-operator-{name}-{}", std::process::id()));
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

        fn op(&self) -> Operator {
            Operator::new(&self.0.join("state"))
        }

        fn run_dir(&self, id: &str) -> PathBuf {
            self.0.join(format!("state/v1/runs/{id}"))
        }

        /// Two-task fixture run whose first worker drops PAUSE: ends `paused` after task one.
        fn start_paused(&self, id: &str) {
            let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
            let pause = self.run_dir(id).join("PAUSE");
            let store = Store::new(&self.0.join("state"));
            store.run_dir(id, true).unwrap();
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
            let path = self.0.join(format!("manifest-{id}.json"));
            fs::write(&path, manifest.to_string()).unwrap();
            let policy = serial::load_manifest(&path).unwrap();
            assert!(matches!(
                serial::start(&store, id, policy).unwrap(),
                Finish::Paused
            ));
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn kind(r: Result<impl std::fmt::Debug>) -> &'static str {
        r.unwrap_err().kind
    }

    // One test drives the engine so the process-wide signal scope is never raced.
    #[test]
    fn serial_run_controls_resume_and_decisions() {
        let s = Scratch::new("serial");
        let op = s.op();
        let base = s.git(&["rev-parse", "HEAD"]);
        s.start_paused("r1");

        let v = op.view("r1").unwrap();
        assert_eq!(v.summary.phase, "paused");
        assert_eq!(v.summary.kind, RunKind::SerialFixture);
        assert_eq!((v.summary.tasks_done, v.summary.tasks_total), (1, 2));
        assert!(v.actions.resume.available);
        for a in [&v.actions.pause, &v.actions.stop, &v.actions.stop_now] {
            assert!(!a.available, "{}", a.reason);
        }
        assert!(!v.actions.approve.available && !v.actions.request_changes.available);
        assert!(!v.complete);
        assert_eq!(v.evidence, SERIAL_EVIDENCE);
        assert_eq!(v.tasks[0].state, TaskState::Committed);
        assert_eq!(v.tasks[1].state, TaskState::Pending);
        assert!(v.events.iter().any(|e| e.kind == "run_paused"));
        let gate_log = v.tasks[0].gate_log.clone().unwrap();
        assert!(v.logs.iter().any(|l| gate_log.ends_with(l.as_str())));
        let tail = op.read_log("r1", &gate_log, 10).unwrap();
        assert!(tail.truncated && tail.text.len() <= 10 && tail.size > 10);
        assert_eq!(kind(op.read_log("r1", "../state.json", 10)), "usage");
        assert_eq!(kind(op.read_log("r1", "nope.log", 10)), "no_log");

        assert_eq!(kind(op.control("r1", Control::Pause)), "unavailable");
        assert_eq!(kind(op.decide("r1", Decision::Approved, "")), "unavailable");
        assert_eq!(
            kind(op.decide("r1", Decision::ChangesRequested, "x")),
            "unavailable"
        );
        assert_eq!(
            kind(op.decide("r1", Decision::ChangesRequested, "  ")),
            "note_required"
        );
        assert_eq!(kind(op.view("../x")), "usage");
        assert_eq!(kind(op.view("zzz")), "no_run");

        let out = op.resume("r1").unwrap();
        assert_eq!(out.outcome, "complete", "{}", out.message);
        let v = op.view("r1").unwrap();
        assert!(v.complete && v.summary.phase == "complete");
        assert!(v.actions.approve.available && v.actions.request_changes.available);
        assert!(!v.actions.resume.available);

        let head = s.git(&["rev-parse", "HEAD"]);
        let before = fs::read_to_string(s.run_dir("r1").join("run.json")).unwrap();
        op.decide("r1", Decision::Approved, "ship\u{1b}[31m it")
            .unwrap();
        assert_eq!(s.git(&["rev-parse", "HEAD"]), head, "approval changed Git");
        assert_ne!(head, base);
        assert_eq!(s.git(&["status", "--porcelain"]), "");
        assert_eq!(
            fs::read_to_string(s.run_dir("r1").join("run.json")).unwrap(),
            before,
            "approval changed run state"
        );
        let v = op.view("r1").unwrap();
        let d = v.decision.unwrap();
        assert_eq!(d.decision, Decision::Approved);
        assert_eq!(d.note, "ship\u{fffd}[31m it");
        assert!(!v.actions.approve.available && !v.actions.request_changes.available);
        assert_eq!(
            kind(op.decide("r1", Decision::ChangesRequested, "no")),
            "unavailable"
        );
        assert_eq!(op.list_runs().unwrap()[0].run_id, "r1");

        // A live owner: controls are offered once, never twice, and decisions are refused.
        s.start_paused("r2");
        let path = s.run_dir("r2").join("run.json");
        let text = fs::read_to_string(&path).unwrap();
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        f.write_all(
            text.replace("\"phase\": \"paused\"", "\"phase\": \"worker\"")
                .as_bytes(),
        )
        .unwrap();
        let Claim::Won(owner) = lease::claim(&s.run_dir("r2"), "test").unwrap() else {
            panic!("lease");
        };
        let v = op.view("r2").unwrap();
        assert!(
            v.actions.pause.available && v.actions.stop.available && v.actions.stop_now.available
        );
        assert!(!v.actions.resume.available && !v.actions.approve.available);
        assert_eq!(
            kind(op.decide("r2", Decision::ChangesRequested, "x")),
            "unavailable"
        );
        assert_eq!(kind(op.resume("r2")), "unavailable");
        op.control("r2", Control::Pause).unwrap();
        assert!(s.run_dir("r2").join("PAUSE").exists());
        let v = op.view("r2").unwrap();
        assert_eq!(v.summary.pending_controls, ["PAUSE"]);
        assert!(!v.actions.pause.available && v.actions.stop.available);
        op.control("r2", Control::StopNow).unwrap();
        assert!(s.run_dir("r2").join("STOP-NOW").exists());
        assert!(!op.view("r2").unwrap().actions.stop.available);
        drop(owner);
        let v = op.view("r2").unwrap();
        assert!(v.summary.owner_lost && !v.actions.resume.available);
        assert!(!v.actions.pause.available);

        // A pilot, live and between tasks' worth of state: honest label, Stop now only.
        s.start_paused("r3");
        let path = s.run_dir("r3").join("run.json");
        let mut st: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        st["phase"] = "worker".into();
        st["pilot"] = true.into();
        st["policy"]["tasks"].as_array_mut().unwrap().truncate(1);
        st["done"].as_array_mut().unwrap().clear();
        st["next_task"] = 0.into();
        fs::write(&path, st.to_string()).unwrap();
        let v = op.view("r3").unwrap();
        assert_eq!(v.summary.kind, RunKind::SerialPilot);
        assert_eq!(v.evidence, PILOT_EVIDENCE);
        assert!(!v.actions.resume.available, "{}", v.actions.resume.reason);
        let Claim::Won(owner) = lease::claim(&s.run_dir("r3"), "test").unwrap() else {
            panic!("lease");
        };
        let v = op.view("r3").unwrap();
        assert!(!v.actions.pause.available && !v.actions.stop.available);
        assert!(v.actions.stop_now.available && !v.actions.resume.available);
        assert_eq!(kind(op.control("r3", Control::Stop)), "unavailable");
        assert_eq!(kind(op.control("r3", Control::Pause)), "unavailable");
        op.control("r3", Control::StopNow).unwrap();
        drop(owner);
        assert_eq!(kind(op.resume("r3")), "unavailable");
    }

    #[test]
    fn diagnostic_runs_offer_nothing() {
        let s = Scratch::new("diag");
        let store = Store::new(&s.0.join("state"));
        let dir = store.run_dir("d1", true).unwrap().unwrap();
        state::write_snapshot(
            &dir,
            &Snapshot::new("d1", Phase::GateFinished, None, vec![]),
        )
        .unwrap();
        let op = s.op();
        let v = op.view("d1").unwrap();
        assert_eq!(v.summary.kind, RunKind::DiagnosticGate);
        assert_eq!(v.evidence, state::EVIDENCE);
        assert!(!v.complete);
        for a in [
            &v.actions.pause,
            &v.actions.stop,
            &v.actions.stop_now,
            &v.actions.resume,
            &v.actions.approve,
            &v.actions.request_changes,
        ] {
            assert!(!a.available);
        }
        assert_eq!(kind(op.decide("d1", Decision::Approved, "")), "unavailable");
        assert!(!dir.join(DECISION_FILE).exists());
        // An unreadable run is listed, not hidden.
        store.run_dir("bad", true).unwrap();
        let list = op.list_runs().unwrap();
        assert!(
            list.iter()
                .any(|r| r.run_id == "bad" && r.kind == RunKind::Invalid && r.problem.is_some())
        );
    }

    #[test]
    fn clean_strips_terminal_controls() {
        assert_eq!(clean("a\u{1b}[2Jb\nc", false), "a\u{fffd}[2Jb\u{fffd}c");
        assert_eq!(clean("a\nb\tc\u{7}", true), "a\nb\tc\u{fffd}");
    }
}
