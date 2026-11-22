//! Opt-in launch registry for the web adapter's "New run". The operator names projects in a
//! private (0600) registry file; each points at a private one-task pilot manifest that is the
//! POLICY TEMPLATE: worker, gate, scopes, timeouts and the task's own prompt are fixed there.
//! A browser may choose only a project id and an objective; the objective is appended to the
//! template prompt, never to argv. Templates are re-read on every options/launch request, and
//! a launch must quote the revision of the policy that was reviewed.
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
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

pub const MAX_OBJECTIVE_BYTES: usize = 4 * 1024; // under web.rs's 8 KiB body cap, so it is ours that answers
const MAX_PROJECTS: usize = 16;
const UNUSABLE: &str = "the project template is not usable";

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
                v["revision"] = json!(revision_of(&e.id, &p));
                match eligibility(&p) {
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
    /// one and the checkout is eligible, with the objective appended to its single task prompt.
    /// Error kinds: `no_project`, `usage`, `ineligible`, `stale_revision`.
    pub fn prepare(
        &self,
        root: &Path,
        project: &str,
        objective: &str,
        revision: &str,
    ) -> Result<Policy> {
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
        if revision_of(&e.id, &policy) != revision {
            return Err(Error::new(
                "stale_revision",
                "the project's policy changed since it was reviewed; reload and review again",
            ));
        }
        eligibility(&policy).map_err(|r| Error::new("ineligible", r))?;
        serial::with_objective(policy, objective).map_err(|m| usage(&m))
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

fn revision_of(id: &str, p: &Policy) -> String {
    let body = serde_json::to_string(p).unwrap_or_default();
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
    Repo::open(&p.repo).map(|_| ()).map_err(|e| match e.kind {
        "git_dirty" => "the checkout has uncommitted changes",
        "git_detached" => "the checkout is not on a branch",
        "git_busy" => "a Git operation is in progress in the checkout",
        "git_hidden" => "the checkout hides changes (assume-unchanged or skip-worktree)",
        _ => "the checkout is not ready",
    })?;
    if let Some(w) = &p.worker.watchdog
        && !Progress::open(&w.progress_file, &p.repo).is_ok_and(|g| g.was_empty_at_open())
    {
        return Err("the worker progress file is not fresh or usable");
    }
    // Probe with a maximal ASCII objective so a template whose prompt leaves no room for one is
    // not advertised as launchable.
    serial::with_objective(p.clone(), &"x".repeat(MAX_OBJECTIVE_BYTES))
        .map(|_| ())
        .map_err(|_| UNUSABLE)
}
