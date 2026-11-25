//! Opt-in launch registry for the web adapter's "New run". The operator names projects in a
//! private (0600) registry file; each points at a private one-task pilot manifest that is the
//! POLICY TEMPLATE: worker, gate, scopes, timeouts and the task's own prompt are fixed there.
//! A browser may choose only a project id and an objective; the objective is appended to the
//! template prompt, never to argv. Templates are re-read on every options/launch request, and
//! a launch must quote the revision of the policy that was reviewed.
//!
//! An entry may opt in with `"one_shot": true`: that project then launches at most once per state
//! root. The claim is an exclusive (`O_EXCL`), private file `v1/one-shot/<project id>`, taken
//! after every other check (objective, revision, eligibility) and again under the engine's checkout
//! guard, which stays held through the run, before the run is published or a worker starts. Nothing here ever removes it: a launch that fails later,
//! or whose outcome is unknown, keeps it, and only the operator may clear it by hand. A claim that
//! cannot be read or written refuses the launch (fixed text, detail on stderr).
//!
//! shortcut: the revision is std's SipHash (128 bits from two salts), a staleness check
//! rather than a cryptographic seal; the template's owner is the operator. The registry file
//! itself is read once at startup.

use crate::cli::pilot_prepare;
use crate::git::{Repo, check_scope};
use crate::operator::clean;
use crate::protocol::Progress;
use crate::serial::{self, Policy};
use crate::state::{self, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const MAX_OBJECTIVE_BYTES: usize = 4 * 1024; // under web.rs's 8 KiB body cap, so it is ours that answers
const MAX_PROJECTS: usize = 16;
const UNUSABLE: &str = "the project template is not usable";
const SPENT: &str = "the one-shot launch was already used";
const UNCHECKED: &str = "the one-shot claim could not be checked";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    registry_version: u32,
    projects: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    label: String,
    manifest: PathBuf,
    #[serde(default)]
    one_shot: bool,
}

pub struct Registry {
    projects: Vec<Entry>,
}

impl Registry {
    /// `Err` is a `usage` error naming what the operator must fix.
    pub fn load(path: &Path) -> Result<Self> {
        let usage = |m: String| Error::new("usage", format!("launch registry: {m}"));
        let text = state::read_bounded(path)
            .map_err(|e| usage(e.to_string()))?
            .ok_or_else(|| usage(format!("{} not found", path.display())))?;
        let f: File = serde_json::from_str(&text).map_err(|e| usage(e.to_string()))?;
        if f.registry_version != 1 {
            return Err(usage("registry_version must be 1".into()));
        }
        if f.projects.is_empty() || f.projects.len() > MAX_PROJECTS {
            return Err(usage(format!("projects must number 1-{MAX_PROJECTS}")));
        }
        for (i, e) in f.projects.iter().enumerate() {
            state::validate_run_id(&e.id)
                .map_err(|_| usage(format!("bad project id {:?}", e.id)))?;
            if f.projects[..i].iter().any(|o| o.id == e.id) {
                return Err(usage(format!("duplicate project id {:?}", e.id)));
            }
            if e.label.is_empty()
                || e.label.chars().count() > 80
                || e.label.chars().any(char::is_control)
            {
                return Err(usage(format!(
                    "project {}: label must be 1-80 printable characters",
                    e.id
                )));
            }
            if !e.manifest.is_absolute() {
                return Err(usage(format!(
                    "project {}: manifest must be an absolute path",
                    e.id
                )));
            }
        }
        Ok(Self {
            projects: f.projects,
        })
    }

    /// Browser review data: project, allowed scopes and budget only. No argv, paths or prompt.
    pub fn options(&self, root: &Path) -> Value {
        let projects: Vec<Value> = self
            .projects
            .iter()
            .map(|e| {
                let mut v =
                    json!({ "id": e.id, "label": e.label, "eligible": false, "reason": UNUSABLE });
                let Ok(p) = template(root, e) else { return v };
                let (w, wd) = (&p.worker, p.worker.watchdog.as_ref());
                v["scopes"] = json!(p.tasks[0].scopes);
                v["budget"] = json!({
                    "tasks": 1,
                    "worker_timeout_secs": w.timeout_secs,
                    "worker_startup_secs": wd.map(|d| d.startup_secs),
                    "worker_idle_secs": wd.map(|d| d.idle_secs),
                    "gate_timeout_secs": p.gate.timeout_secs,
                });
                v["revision"] = json!(revision_of(e, &p));
                match ready(root, e, &p, false) {
                    Ok(()) => {
                        v["eligible"] = json!(true);
                        v.as_object_mut().unwrap().remove("reason");
                    }
                    Err(r) => v["reason"] = json!(r),
                }
                v
            })
            .collect();
        json!({ "enabled": true, "objective_max_bytes": MAX_OBJECTIVE_BYTES, "projects": projects })
    }

    /// The policy to launch: the template as it reads now, only if its revision is the reviewed
    /// one and the checkout is eligible, with the objective appended to its single task prompt,
    /// and the `Shot` to take under the engine's checkout guard (see `Shot::claim`).
    /// Error kinds: `no_project`, `usage`, `ineligible`, `stale_revision`.
    pub fn prepare(
        &self,
        root: &Path,
        project: &str,
        objective: &str,
        revision: &str,
    ) -> Result<(Policy, Shot)> {
        let e = self
            .projects
            .iter()
            .find(|e| e.id == project)
            .ok_or_else(|| Error::new("no_project", "no such project"))?;
        let objective = objective.trim();
        let usage = |m: &str| Error::new("usage", m);
        if objective.is_empty() {
            return Err(usage("objective must not be empty"));
        }
        if objective.len() > MAX_OBJECTIVE_BYTES {
            return Err(usage("objective is too large"));
        }
        if objective
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        {
            return Err(usage("objective contains control characters"));
        }
        let policy = template(root, e)?;
        if revision_of(e, &policy) != revision {
            return Err(Error::new(
                "stale_revision",
                "the project's policy changed since it was reviewed; reload and review again",
            ));
        }
        ready(root, e, &policy, true).map_err(|r| Error::new("ineligible", r))?;
        let policy = serial::with_objective(policy, objective).map_err(|m| usage(&m))?;
        let shot = Shot {
            root: root.to_path_buf(),
            id: e.id.clone(),
            one_shot: e.one_shot,
        };
        Ok((policy, shot))
    }
}

/// The template through the TUI's pilot guards (private file, explicit root, no repo/root
/// overlap), outside its own repo, with exactly one task; `repo` canonical. Detail goes to stderr.
fn template(root: &Path, e: &Entry) -> Result<Policy> {
    let bad = |why: String| {
        eprintln!(
            "lmloop web: project {}: {}",
            clean(&e.id, false),
            clean(&why, false)
        );
        Error::new("ineligible", UNUSABLE)
    };
    let manifest = fs::canonicalize(&e.manifest).map_err(|x| bad(x.to_string()))?;
    let (_, _, mut policy) =
        pilot_prepare(Some(root.to_path_buf()), true, "probe", &manifest, true)
            .map_err(|x| bad(x.message))?;
    let repo = fs::canonicalize(&policy.repo).map_err(|x| bad(x.to_string()))?;
    if manifest.starts_with(&repo) {
        return Err(bad("manifest lives inside its repository".into()));
    }
    if policy.tasks.len() != 1 {
        return Err(bad("a launch template must have exactly one task".into()));
    }
    if let Some(s) = policy.tasks[0]
        .scopes
        .iter()
        .find(|s| check_scope(s).is_err())
    {
        return Err(bad(format!("unsafe scope {s:?}")));
    }
    policy.repo = repo;
    Ok(policy)
}

/// The one-shot claim a launch must take under the engine's checkout guard (a no-op for a
/// project that did not opt in). Carried from `prepare` to the engine's guarded hook.
pub struct Shot {
    root: PathBuf,
    id: String,
    one_shot: bool,
}

impl Shot {
    /// Runs with the checkout guard held, before the run is published: re-check the claim and the
    /// checkout (it may have changed since `prepare`), then take the claim. A refusal spends
    /// nothing; once taken, the claim is kept whatever follows. Typed `ineligible` errors.
    pub fn claim(&self, p: &Policy) -> Result<()> {
        if !self.one_shot {
            return Ok(());
        }
        let refuse = |r: &'static str| Error::new("ineligible", r);
        match spent(&self.root, &self.id) {
            Ok(false) => {}
            Ok(true) => return Err(refuse(SPENT)),
            Err(x) => return Err(refuse(unchecked(&self.id, &x))),
        }
        checkout_ready(p).map_err(refuse)?;
        match create_claim(&self.root, &self.id) {
            Ok(true) => Ok(()),
            Ok(false) => Err(refuse(SPENT)),
            Err(x) => Err(refuse(unchecked(&self.id, &x))),
        }
    }
}

/// Eligibility, after the one-shot claim: a spent or unverifiable project says so whatever its
/// checkout looks like. Reasons are fixed text. Only a launch (`log`) writes the detail to stderr:
/// options are polled and must not repeat it.
fn ready(root: &Path, e: &Entry, p: &Policy, log: bool) -> std::result::Result<(), &'static str> {
    if e.one_shot {
        match spent(root, &e.id) {
            Ok(false) => {}
            Ok(true) => return Err(SPENT),
            Err(x) if log => return Err(unchecked(&e.id, &x)),
            Err(_) => return Err(UNCHECKED),
        }
    }
    eligibility(p)
}

/// Whether the project's claim exists; `Err` when it cannot be trusted (malformed, foreign,
/// unreadable), which refuses just like a spent claim.
fn spent(root: &Path, id: &str) -> Result<bool> {
    let Some(dir) = shot_dir(root)? else {
        return Ok(false);
    };
    let Some(text) = state::read_bounded(&dir.join(id))? else {
        return Ok(false);
    };
    match serde_json::from_str::<ShotClaim>(&text) {
        Ok(c) if c.claim_version == 1 && c.project == id => Ok(true),
        _ => Err(Error::new(
            "malformed",
            "one-shot claim is not a valid claim file",
        )),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShotClaim {
    claim_version: u32,
    project: String,
    claimed_at_unix: u64,
}

/// The private claim directory, or `None` while neither it nor its parent exists yet.
fn shot_dir(root: &Path) -> Result<Option<PathBuf>> {
    let base = state::Store::new(root).base().to_path_buf();
    let dir = base.join("one-shot");
    Ok((state::private_dir(&base)? && state::private_dir(&dir)?).then_some(dir))
}

/// Fixed text for the client; the detail goes to stderr only.
fn unchecked(id: &str, e: &Error) -> &'static str {
    eprintln!(
        "lmloop web: project {}: one-shot claim: {}",
        clean(id, false),
        clean(&e.to_string(), false)
    );
    UNCHECKED
}

/// `Ok(false)` if the claim already exists. Once the file exists it is kept whatever follows.
/// Returns only after the file, its directory, `v1` and the state root are fsynced, so a crash
/// right after a published run cannot lose the claim.
fn create_claim(root: &Path, id: &str) -> Result<bool> {
    let base = state::Store::new(root).base().to_path_buf();
    let dir = base.join("one-shot");
    state::ensure_private_dir(&base, true)?;
    state::ensure_private_dir(&dir, false)?;
    let path = dir.join(id);
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(state::io_err(&path)(e)),
    };
    let body = serde_json::to_string(&ShotClaim {
        claim_version: 1,
        project: id.into(),
        claimed_at_unix: state::now_unix(),
    })
    .map_err(|e| Error::new("io", e.to_string()))?;
    file.write_all(body.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(state::io_err(&path))?;
    for d in [dir.as_path(), base.as_path(), root] {
        fs::File::open(d)
            .and_then(|f| f.sync_all())
            .map_err(state::io_err(d))?;
    }
    Ok(true)
}

fn revision_of(e: &Entry, p: &Policy) -> String {
    let id = &e.id;
    let mut body = serde_json::to_string(p).unwrap_or_default();
    if e.one_shot {
        body.push_str("\none_shot"); // only one-shot projects change; existing revisions stay put
    }
    let h = |salt: u8| {
        let mut h = DefaultHasher::new();
        (salt, id, &body).hash(&mut h);
        h.finish()
    };
    format!("{:016x}{:016x}", h(0), h(1))
}

/// The engine's own clean-baseline and fresh-watchdog checks, run early (the engine checks again
/// at start); reasons are fixed text (no paths).
fn eligibility(p: &Policy) -> std::result::Result<(), &'static str> {
    checkout_ready(p)?;
    // Probe with a maximal ASCII objective so a template whose prompt leaves no room for one is
    // not advertised as launchable.
    serial::with_objective(p.clone(), &"x".repeat(MAX_OBJECTIVE_BYTES))
        .map(|_| ())
        .map_err(|_| UNUSABLE)
}

/// Clean attached checkout and fresh watchdog file; safe to call on a policy that already
/// carries its objective.
fn checkout_ready(p: &Policy) -> std::result::Result<(), &'static str> {
    Repo::open(&p.repo).map(|_| ()).map_err(|e| match e.kind {
        "git_dirty" => "the checkout has uncommitted changes",
        "git_detached" => "the checkout is not on a branch",
        "git_busy" => "a Git operation is in progress in the checkout",
        "git_hidden" => "the checkout hides changes (assume-unchanged or skip-worktree)",
        _ => "the checkout is not ready",
    })?;
    if let Some(w) = &p.worker.watchdog
        && !match w.writer_gid {
            Some(gid) => Progress::open_shared(&w.progress_file, &p.repo, gid),
            None => Progress::open(&w.progress_file, &p.repo),
        }
        .is_ok_and(|g| g.was_empty_at_open())
    {
        return Err("the worker progress file is not fresh or usable");
    }
    Ok(())
}
