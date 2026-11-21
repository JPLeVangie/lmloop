//! Command line. Exit codes are part of the contract; see `after_help`.

use crate::gate::{self, Outcome};
use crate::lease::{self, Claim, LeaseState};
use crate::serial::{self, Finish};
use crate::state::{self, Error, Phase, Snapshot, Store};
use crate::tui::Scope;
use clap::parser::ValueSource;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};
use std::fs;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const EXIT_ERROR: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_GATE_ABSENT: u8 = 3;
const EXIT_GATE_FAILED: u8 = 4;
const EXIT_GATE_MISSING: u8 = 5;
const EXIT_GATE_TIMEOUT: u8 = 6;
const EXIT_OWNED_ELSEWHERE: u8 = 7;
const EXIT_NEEDS_RECONCILIATION: u8 = 8;
const EXIT_INTERRUPTED: u8 = 9;
const EXIT_TASK_FAILED: u8 = 10;
const EXIT_STOPPED: u8 = 11;
const EXIT_PAUSED: u8 = 12;
const EXIT_NO_RUN: u8 = 3;
const EXIT_INVALID: u8 = 4;
const EXIT_UNAVAILABLE: u8 = 69;
const MAX_OBJECTIVE_BYTES: u64 = 256 * 1024;

#[derive(Parser)]
#[command(
    name = "lmloop",
    version,
    about = "lmloop (Rust rewrite, milestone 1): native status, dry-run and a diagnostic gate",
    after_help = "\
Native state lives under <state-root>/v1/runs/<RUN_ID>; Python lmloop state is never read.\n\
The state root must not overlap ~/.local/state/lmloop or a Python lmloop checkout; beyond that a custom root is the operator's responsibility.\n\
\n\
EXIT CODES\n  \
run --dry-run: 0 valid | 2 invalid input or flag conflict\n  \
run (no --dry-run): 69 without --manifest (no legacy config; refuses) | with --manifest --run-id: the one-shot pilot exit codes below\n  \
run --manifest: 0 committed | 2 usage | 7 owned elsewhere | 8 needs reconciliation | 10 task failed | 11 stopped | 12 paused (one-shot, not resumable)\n  \
status: 0 ok | 3 no such run | 4 run exists but its snapshot is invalid or unsupported\n  \
diagnostic-gate: 0 passed | 3 absent | 4 failed | 5 missing | 6 timed out | 7 run owned elsewhere |\n  \
                 8 previous owner was lost mid-gate (needs manual reconciliation; nothing changed) |\n  \
                 9 interrupted by SIGTERM/SIGINT/SIGHUP (gate torn down, snapshot says interrupted) | 1 error\n  \
fixture-run/fixture-resume: 0 complete | 2 usage | 7 owned elsewhere | 8 needs reconciliation | 10 task failed |\n  \
                 11 stopped (STOP at a task boundary, or STOP-NOW) | 12 paused (PAUSE; fixture-resume continues)\n  \
any command: 2 usage error"
)]
struct Cli {
    /// Native state root (default: $XDG_STATE_HOME/lmloop-rust, else ~/.local/state/lmloop-rust)
    #[arg(
        long,
        global = true,
        env = "LMLOOP_RUST_STATE_ROOT",
        value_name = "DIR"
    )]
    state_root: Option<PathBuf>,
    /// No subcommand on a terminal: the console scoped to the current Git checkout.
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Validate an objective; with --dry-run print the plan and create nothing. Live only with --manifest (one-shot native pilot).
    Run(RunArgs),
    /// Read-only: show a run's native status snapshot and ownership.
    Status(StatusArgs),
    /// Diagnostic only: claim a run, clear stale STOP/STOP-NOW, run one operator argv. Verifies nothing.
    DiagnosticGate(GateArgs),
    /// FIXTURE ONLY: run a versioned operator manifest's tasks serially with an executable fake worker.
    FixtureRun(FixtureRunArgs),
    /// FIXTURE ONLY: continue a fixture run; recovers a completed commit, refuses ambiguous states.
    FixtureResume(FixtureResumeArgs),
    /// PILOT: one operator-manifest task through the serial gate-checked commit; needs an explicit --state-root. Not production.
    PilotRun(FixtureRunArgs),
    /// Interactive operator console over the native run store (press ? for keys). Quitting never stops a worker.
    Tui,
    /// Local JSON/HTTP operator API and UI over the same run store (127.0.0.1 only; fixture evidence).
    Web(WebArgs),
}

#[derive(Args)]
struct WebArgs {
    /// Loopback TCP port; 0 picks a free one (the bound address is printed)
    #[arg(long, default_value_t = crate::web::DEFAULT_PORT)]
    port: u16,
    /// Optional owner-only 0600 file containing `username:password`
    #[arg(long, value_name = "FILE")]
    auth_file: Option<PathBuf>,
    /// HTTPS origin served by a same-host TLS proxy (requires --auth-file)
    #[arg(long, value_name = "ORIGIN")]
    public_origin: Option<String>,
}

#[derive(Args)]
struct FixtureRunArgs {
    /// New run id: [A-Za-z0-9_-], 1-64 chars
    run_id: String,
    /// Operator manifest JSON (manifest_version 1)
    #[arg(long, value_name = "FILE")]
    manifest: PathBuf,
}

#[derive(Args)]
struct FixtureResumeArgs {
    /// Existing fixture run id
    run_id: String,
}

#[derive(Args)]
struct RunArgs {
    /// What to work on; '-' reads stdin
    objective: String,
    /// Print the plan and create nothing
    #[arg(long)]
    dry_run: bool,
    /// Start in the background (not available with --dry-run)
    #[arg(long, conflicts_with = "dry_run")]
    detach: bool,
    /// Repository the run would work in (default: current directory)
    #[arg(long, value_name = "DIR")]
    repo: Option<PathBuf>,
    /// Iteration cap
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    max_iterations: Option<u32>,
    /// Trusted private (0600, absolute, outside the repo) operator manifest: runs it as a one-shot
    /// native pilot (1-3 tasks, no resume); needs --run-id and an explicit --state-root
    #[arg(
        long,
        value_name = "FILE",
        requires = "run_id",
        conflicts_with_all = ["dry_run", "detach", "max_iterations"]
    )]
    manifest: Option<PathBuf>,
    /// New run id for --manifest: [A-Za-z0-9_-], 1-64 chars
    #[arg(long, value_name = "ID", requires = "manifest")]
    run_id: Option<String>,
}

#[derive(Args)]
struct StatusArgs {
    /// Which run; defaults to the most recently modified
    run_id: Option<String>,
    /// Emit one JSON document
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct GateArgs {
    /// Run id (created if absent): [A-Za-z0-9_-], 1-64 chars
    run_id: String,
    /// Wall-clock cap in seconds
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=86400))]
    timeout_secs: u64,
    /// Working directory for the gate (default: current directory)
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// Print the published snapshot as JSON
    #[arg(long)]
    json: bool,
    /// Gate argv, executed directly (no shell), after `--`. Empty means no gate configured.
    #[arg(last = true, value_name = "ARGV")]
    argv: Vec<String>,
}

pub fn main() -> ExitCode {
    let matches = Cli::command().get_matches();
    let explicit_root = matches.value_source("state_root") == Some(ValueSource::CommandLine);
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let code = match dispatch(cli, explicit_root) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("lmloop: {e}");
            EXIT_ERROR
        }
    };
    ExitCode::from(code)
}

fn dispatch(cli: Cli, explicit_root: bool) -> Result<u8, Error> {
    let Some(command) = cli.command else {
        if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
            let _ = Cli::command().write_help(&mut std::io::stderr());
            return Ok(EXIT_USAGE);
        }
        let root = state::resolve_root(cli.state_root)?;
        let cwd = std::env::current_dir().map_err(|e| Error::new("io", e.to_string()))?;
        let scope = crate::git::toplevel(&cwd).map_or(Scope::NoRepo, Scope::Repo);
        crate::tui::run(&root, scope, explicit_root)
            .map_err(|e| Error::new("io", format!("terminal: {e}")))?;
        return Ok(0);
    };
    match command {
        Cmd::Run(a) => run(a, cli.state_root, explicit_root),
        Cmd::Status(a) => status(&Store::new(&state::resolve_root(cli.state_root)?), a),
        Cmd::DiagnosticGate(a) => {
            diagnostic_gate(&Store::new(&state::resolve_root(cli.state_root)?), a)
        }
        Cmd::FixtureRun(a) => {
            let store = Store::new(&state::resolve_root(cli.state_root)?);
            let id = match state::validate_run_id(&a.run_id) {
                Ok(id) => id,
                Err(m) => return Ok(usage_error(&m)),
            };
            match serial::load_manifest(&a.manifest) {
                Ok(policy) => finish(&id, serial::start(&store, &id, policy)?),
                Err(m) => Ok(usage_error(&m)),
            }
        }
        Cmd::PilotRun(a) => pilot_run(cli.state_root, explicit_root, a),
        Cmd::Tui => {
            let root = state::resolve_root(cli.state_root)?;
            crate::tui::run(&root, Scope::All, explicit_root)
                .map_err(|e| Error::new("io", format!("terminal: {e}")))?;
            Ok(0)
        }
        Cmd::Web(a) => {
            crate::web::run(
                &state::resolve_root(cli.state_root)?,
                a.port,
                a.auth_file.as_deref(),
                a.public_origin.as_deref(),
            )?;
            Ok(0)
        }
        Cmd::FixtureResume(a) => {
            let store = Store::new(&state::resolve_root(cli.state_root)?);
            match state::validate_run_id(&a.run_id) {
                Ok(id) => finish(&id, serial::resume(&store, &id)?),
                Err(m) => Ok(usage_error(&m)),
            }
        }
    }
}

/// Pilot guards, shared by `pilot-run` and the TUI: the state root is the flag (not the
/// environment or a default) and must not overlap the repository the worker edits. The manifest
/// is read once here; errors of kind `usage` are the operator's to fix.
pub fn pilot_prepare(
    root: Option<PathBuf>,
    explicit: bool,
    run_id: &str,
    manifest: &Path,
    private: bool,
) -> Result<(PathBuf, String, serial::Policy), Error> {
    let usage = |m: &str| Error::new("usage", m);
    if !explicit {
        return Err(usage("pilot-run requires an explicit --state-root"));
    }
    let id = state::validate_run_id(run_id).map_err(|m| Error::new("usage", m))?;
    // `private` (the TUI) also demands an owner-only regular file; `pilot-run` keeps its contract.
    let policy = if private {
        serial::load_private_pilot_manifest(manifest)
    } else {
        serial::load_pilot_manifest(manifest)
    }
    .map_err(|m| Error::new("usage", m))?;
    let root = state::resolve_root(root)?;
    let repo = fs::canonicalize(&policy.repo).map_err(|_| usage("manifest repo does not exist"))?;
    if root.starts_with(&repo) || repo.starts_with(&root) {
        return Err(usage("--state-root must not overlap the manifest repo"));
    }
    Ok((root, id, policy))
}

fn pilot_run(root: Option<PathBuf>, explicit: bool, a: FixtureRunArgs) -> Result<u8, Error> {
    launch_pilot(pilot_prepare(root, explicit, &a.run_id, &a.manifest, false))
}

fn launch_pilot(prepared: Result<(PathBuf, String, serial::Policy), Error>) -> Result<u8, Error> {
    let (root, id, policy) = match prepared {
        Ok(t) => t,
        Err(e) if e.kind == "usage" => return Ok(usage_error(&e.message)),
        Err(e) => return Err(e),
    };
    pilot_finish(&id, serial::start_pilot(&Store::new(&root), &id, policy)?)
}

/// `run --manifest`: the same guards and engine as the TUI's pilot launch. The manifest must be a
/// private file outside the repo and name exactly this repo; the policy is the manifest's alone.
fn manifest_prepare(
    root: Option<PathBuf>,
    explicit: bool,
    run_id: &str,
    manifest: &Path,
    repo: &Path,
) -> Result<(PathBuf, String, serial::Policy), Error> {
    let usage = |m: &str| Error::new("usage", m);
    if !manifest.is_absolute() {
        return Err(usage("manifest must be an absolute path"));
    }
    let manifest = fs::canonicalize(manifest)
        .map_err(|e| usage(&format!("manifest {}: {e}", manifest.display())))?;
    let repo = fs::canonicalize(repo).map_err(|_| usage("repo does not exist"))?;
    if manifest.starts_with(&repo) {
        return Err(usage(
            "manifest must live outside the repository it launches in",
        ));
    }
    let (root, id, mut policy) = pilot_prepare(root, explicit, run_id, &manifest, true)?;
    if fs::canonicalize(&policy.repo).ok().as_ref() != Some(&repo) {
        return Err(usage(&format!(
            "manifest repo {} is not the repo to run in {}",
            policy.repo.display(),
            repo.display()
        )));
    }
    policy.repo = repo;
    Ok((root, id, policy))
}

fn pilot_finish(id: &str, f: Finish) -> Result<u8, Error> {
    if matches!(f, Finish::Complete) {
        println!(
            "run {id}: pilot task committed after the operator gate passed (pilot evidence; not production verification)"
        );
        return Ok(0);
    }
    if matches!(f, Finish::Paused) {
        eprintln!("lmloop: pilot run {id} paused; a pilot run is one-shot and cannot be resumed");
        return Ok(EXIT_PAUSED);
    }
    finish(id, f)
}

fn finish(id: &str, f: Finish) -> Result<u8, Error> {
    Ok(match f {
        Finish::Complete => {
            println!("run {id}: all tasks committed (fixture runner)");
            0
        }
        Finish::TaskFailed => {
            eprintln!(
                "lmloop: run {id}: a task failed; nothing was committed for it (see `lmloop status {id}`)"
            );
            EXIT_TASK_FAILED
        }
        Finish::Stopped => {
            eprintln!("lmloop: run {id} stopped; nothing further committed, worktree preserved");
            EXIT_STOPPED
        }
        Finish::Paused => {
            eprintln!(
                "lmloop: run {id} paused at a task boundary; `fixture-resume {id}` continues"
            );
            EXIT_PAUSED
        }
        Finish::OwnedElsewhere => {
            eprintln!("lmloop: run {id} is owned by another process; nothing was changed");
            EXIT_OWNED_ELSEWHERE
        }
        Finish::NeedsReconciliation(m) => {
            eprintln!("lmloop: {m}");
            EXIT_NEEDS_RECONCILIATION
        }
        Finish::NoRun => {
            eprintln!("lmloop: no such fixture run: {id}");
            EXIT_NO_RUN
        }
        Finish::Usage(m) => usage_error(&m),
    })
}

fn usage_error(msg: &str) -> u8 {
    eprintln!("lmloop: {msg}");
    EXIT_USAGE
}

fn run(a: RunArgs, root: Option<PathBuf>, explicit: bool) -> Result<u8, Error> {
    let objective = if a.objective == "-" {
        let mut text = String::new();
        std::io::stdin()
            .take(MAX_OBJECTIVE_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|e| Error::new("io", format!("stdin: {e}")))?;
        text
    } else {
        a.objective
    };
    if objective.trim().is_empty() {
        return Ok(usage_error("objective must not be empty"));
    }
    if objective.len() as u64 > MAX_OBJECTIVE_BYTES {
        return Ok(usage_error("objective is too large"));
    }
    let repo = match a.repo {
        Some(r) => r,
        None => std::env::current_dir().map_err(|e| Error::new("io", e.to_string()))?,
    };
    if !git_marker_shape_ok(&repo) {
        return Ok(usage_error(&format!(
            "{repo:?} does not look like a git repository (structural check only)"
        )));
    }
    if let (Some(manifest), Some(run_id)) = (&a.manifest, &a.run_id) {
        // The manifest's task prompts are the work; the objective is never sent to the worker.
        eprintln!("lmloop: objective is not sent to the worker; the manifest's task prompts are");
        return launch_pilot(manifest_prepare(root, explicit, run_id, manifest, &repo));
    }
    if !a.dry_run {
        eprintln!(
            "lmloop: a live run needs --manifest and --run-id (a trusted operator manifest) and an explicit --state-root; use --dry-run to plan"
        );
        return Ok(EXIT_UNAVAILABLE);
    }
    println!("DRY RUN: no worker, run state or lease is created");
    println!("repo: {repo:?} (structural .git check only; not proof of a valid HEAD)");
    println!("objective: {}", display_objective(&objective));
    println!(
        "max iterations: {}",
        a.max_iterations
            .map_or("(default)".into(), |n| n.to_string())
    );
    Ok(0)
}

/// Read-only and subprocess-free: `.git` is a directory holding a `HEAD` file,
/// or a `gitdir:` pointer file whose target is a directory. It does not prove
/// the repository has a usable HEAD.
fn git_marker_shape_ok(repo: &Path) -> bool {
    let marker = repo.join(".git");
    let Ok(meta) = fs::symlink_metadata(&marker) else {
        return false;
    };
    if meta.is_dir() {
        return fs::metadata(marker.join("HEAD")).is_ok_and(|m| m.is_file());
    }
    if !meta.is_file() || meta.len() > 4096 {
        return false;
    }
    let Ok(text) = fs::read_to_string(&marker) else {
        return false;
    };
    text.strip_prefix("gitdir: ")
        .is_some_and(|t| fs::metadata(repo.join(t.trim())).is_ok_and(|m| m.is_dir()))
}

/// Operator text is untrusted terminal input: no control characters, bounded.
fn display_objective(text: &str) -> String {
    let flat: String = text
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    match flat.char_indices().nth(200) {
        Some((i, _)) => format!("{}... (truncated)", &flat[..i]),
        None => flat,
    }
}

fn status(store: &Store, a: StatusArgs) -> Result<u8, Error> {
    let report = |state: &str, code: u8, extra: Value, text: String| -> u8 {
        if a.json {
            let mut doc = json!({ "state": state });
            doc.as_object_mut()
                .unwrap()
                .extend(extra.as_object().cloned().unwrap_or_default());
            println!("{doc}");
        } else {
            println!("{text}");
        }
        code
    };
    let id = match &a.run_id {
        Some(id) => match state::validate_run_id(id) {
            Ok(id) => id,
            Err(m) => return Ok(usage_error(&m)),
        },
        None => match store.latest_run() {
            Ok(Some(id)) => id,
            Ok(None) => return Ok(report("no_run", EXIT_NO_RUN, json!({}), "no runs".into())),
            Err(e) => return Ok(invalid(&report, None, &e)),
        },
    };
    let dir = match store.run_dir(&id, false) {
        Ok(Some(d)) => d,
        Ok(None) => {
            return Ok(report(
                "no_run",
                EXIT_NO_RUN,
                json!({ "run_id": id }),
                format!("no such run: {id}"),
            ));
        }
        Err(e) => return Ok(invalid(&report, Some(&id), &e)),
    };
    match serial::status(&dir, &id) {
        Ok(Some((extra, text))) => return Ok(report("ok", 0, extra, text)),
        Ok(None) => {}
        Err(e) => return Ok(invalid(&report, Some(&id), &e)),
    }
    let result =
        state::read_snapshot(&dir, &id).and_then(|snap| lease::peek(&dir).map(|l| (snap, l)));
    match result {
        Ok((snap, lease)) => {
            let owner_lost = snap.phase == Phase::GateRunning && lease.state != LeaseState::Held;
            let text = format!(
                "run: {id}\nphase: {:?}{}\nownership: {:?}{}\ngate: {}\nverified completion: no (diagnostic gate only, not bound to a Git snapshot)",
                snap.phase,
                if owner_lost {
                    " (owner lost: treat as interrupted; manual reconciliation required)"
                } else {
                    ""
                },
                lease.state,
                lease.pid.map_or(String::new(), |p| format!(" (pid {p})")),
                snap.gate
                    .as_ref()
                    .map_or("none".into(), |g| format!("{:?}", g.outcome)),
            );
            Ok(report(
                "ok",
                0,
                json!({
                    "run_id": id,
                    "snapshot": snap,
                    "lease": lease,
                    "effective_phase": if owner_lost { json!("interrupted") } else { json!(snap.phase) },
                    "needs_reconciliation": owner_lost,
                }),
                text,
            ))
        }
        Err(e) => Ok(invalid(&report, Some(&id), &e)),
    }
}

fn invalid(report: &dyn Fn(&str, u8, Value, String) -> u8, id: Option<&str>, e: &Error) -> u8 {
    report(
        "invalid",
        EXIT_INVALID,
        json!({ "run_id": id, "error": { "kind": e.kind, "message": e.message } }),
        format!("invalid: {e}"),
    )
}

/// Move consumed STOP/STOP-NOW into `stop-archive/` instead of deleting them.
fn archive_stop(dir: &Path) -> Result<Vec<String>, Error> {
    serial::archive_controls(dir, &["STOP", "STOP-NOW"])
}

fn diagnostic_gate(store: &Store, a: GateArgs) -> Result<u8, Error> {
    let id = match state::validate_run_id(&a.run_id) {
        Ok(id) => id,
        Err(m) => return Ok(usage_error(&m)),
    };
    let cwd = match a.cwd {
        Some(c) => c,
        None => std::env::current_dir().map_err(|e| Error::new("io", e.to_string()))?,
    };
    if !fs::metadata(&cwd).is_ok_and(|m| m.is_dir()) {
        return Ok(usage_error(&format!("--cwd {cwd:?} is not a directory")));
    }
    let dir = store.run_dir(&id, true)?.expect("created");
    let _lease = match lease::claim(&dir, "diagnostic-gate")? {
        Claim::Won(l) => l,
        Claim::Lost(info) => {
            eprintln!(
                "lmloop: run {id} is owned by another process (pid {:?}); nothing was changed",
                info.pid
            );
            return Ok(EXIT_OWNED_ELSEWHERE);
        }
    };
    // Declared after the lease, so it drops first: the gate is gone before ownership is.
    let _signals = gate::SignalScope::install();
    // Validate the previous snapshot before touching anything. Missing is fine;
    // unsupported or malformed is preserved; an owner that died mid-gate needs a human.
    match state::read_snapshot(&dir, &id) {
        Ok(prev) if prev.phase == Phase::GateRunning => {
            eprintln!(
                "lmloop: run {id} has a gate_running snapshot but no owner (previous owner was lost); \
                 refusing to retry until it is reconciled manually; nothing was changed"
            );
            return Ok(EXIT_NEEDS_RECONCILIATION);
        }
        Ok(_) => {}
        Err(e) if e.kind == "missing_snapshot" => {}
        Err(e) => return Err(e),
    }
    let sink = if a.argv.is_empty() {
        None
    } else {
        Some(gate::open_log(&dir)?)
    };
    let cleared = archive_stop(&dir)?;
    state::write_snapshot(
        &dir,
        &Snapshot::new(&id, Phase::GateRunning, None, cleared.clone()),
    )?;
    let mut exec = gate::run(&a.argv, &cwd, Duration::from_secs(a.timeout_secs));
    let log_error = sink.and_then(|sink| gate::write_log(sink, &mut exec).err());
    if let Some(e) = &log_error {
        exec.record.detail = Some(format!("log write failed: {e}"));
    }
    let record = exec.record;
    let outcome = record.outcome;
    let phase = if outcome == Outcome::Interrupted {
        Phase::Interrupted
    } else {
        Phase::GateFinished
    };
    let snap = Snapshot::new(&id, phase, Some(record), cleared);
    state::write_snapshot(&dir, &snap)?;
    if a.json {
        println!(
            "{}",
            serde_json::to_string(&snap).map_err(|e| Error::new("io", e.to_string()))?
        );
    } else {
        println!(
            "gate {outcome:?} (diagnostic only; not verified completion, not reusable gate evidence)"
        );
    }
    let code = match outcome {
        Outcome::Passed => 0,
        Outcome::Absent => EXIT_GATE_ABSENT,
        Outcome::Failed => EXIT_GATE_FAILED,
        Outcome::Missing => EXIT_GATE_MISSING,
        Outcome::TimedOut => EXIT_GATE_TIMEOUT,
        Outcome::Interrupted | Outcome::Cancelled => EXIT_INTERRUPTED,
    };
    // The finished result is already published; a failed log write is still an error.
    match log_error {
        Some(e) => Err(e),
        None => Ok(code),
    }
}
