//! Run ownership: an OS-held advisory lock on `owner.lock`. The PID metadata
//! written into it is for diagnostics only; the lock decides who owns the run.

use crate::state::{Error, Result, check_file, io_err, now_unix, read_bounded};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const FILE: &str = "owner.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// Some process holds the OS lock right now.
    Held,
    /// Nobody holds it, but a previous owner left a PID marker behind.
    Stale,
    /// No lock file, or one that never recorded an owner.
    Free,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaseInfo {
    pub state: LeaseState,
    pub pid: Option<u32>,
    pub command: Option<String>,
    pub started_at_unix: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    pid: u32,
    command: String,
    started_at_unix: u64,
}

fn info(state: LeaseState, text: &str) -> LeaseInfo {
    let meta: Option<Meta> = serde_json::from_str(text).ok();
    LeaseInfo {
        state,
        pid: meta.as_ref().map(|m| m.pid),
        command: meta.as_ref().map(|m| m.command.clone()),
        started_at_unix: meta.map(|m| m.started_at_unix),
    }
}

/// Held for as long as the value lives; dropping it (or dying) releases it.
#[derive(Debug)]
pub struct Lease {
    _file: File,
}

pub enum Claim {
    Won(Lease),
    Lost(LeaseInfo),
}

/// Open (creating if absent, never truncating) a private lock file: no symlink, no FIFO wait.
fn open_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(io_err(path))?;
    check_file(&file, path)?;
    Ok(file)
}

/// One lmloop writer per Git checkout, whatever state root it uses. `git_dir` is the
/// worktree-specific Git directory, so linked worktrees are independent. Nonblocking; a lock
/// file is never unlinked or truncated, and one that is hard-linked elsewhere is refused.
/// `None` means another process holds it. The OS drops it when the holder dies, so it cannot
/// outlive the process (a worker orphaned by a killed owner is no longer excluded).
pub fn claim_checkout(git_dir: &Path) -> Result<Option<Lease>> {
    let path = git_dir.join("lmloop-checkout.lock");
    let file = open_lock(&path)?;
    if file.metadata().map_err(io_err(&path))?.nlink() != 1 {
        return Err(Error::new(
            "unsafe_path",
            format!("{} has more than one hard link", path.display()),
        ));
    }
    match file.try_lock() {
        Ok(()) => Ok(Some(Lease { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(io_err(&path)(e)),
    }
}

/// Try to become the owner of `run_dir`. A loser changes nothing.
pub fn claim(run_dir: &Path, command: &str) -> Result<Claim> {
    let path = run_dir.join(FILE);
    let mut file = open_lock(&path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let mut text = String::new();
            let _ = file.read_to_string(&mut text);
            return Ok(Claim::Lost(info(LeaseState::Held, &text)));
        }
        Err(TryLockError::Error(e)) => return Err(io_err(&path)(e)),
    }
    let meta = Meta {
        pid: std::process::id(),
        command: command.into(),
        started_at_unix: now_unix(),
    };
    let body = serde_json::to_string(&meta).map_err(|e| Error::new("io", e.to_string()))?;
    file.set_len(0)
        .and_then(|()| file.write_all(body.as_bytes()))
        .and_then(|()| file.sync_all())
        .map_err(io_err(&path))?;
    Ok(Claim::Won(Lease { _file: file }))
}

/// Read-only look at ownership: never creates the file, never opens it for
/// writing. It probes with a shared lock, held for an instant, so a claimer
/// racing this exact instant can lose spuriously and must just retry.
pub fn peek(run_dir: &Path) -> Result<LeaseInfo> {
    let path = run_dir.join(FILE);
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(info(LeaseState::Free, ""));
        }
        Err(e) => return Err(io_err(&path)(e)),
    };
    check_file(&file, &path)?;
    let text = read_bounded(&path)?.unwrap_or_default();
    match file.try_lock_shared() {
        Ok(()) => {
            let i = info(LeaseState::Free, &text);
            Ok(if i.pid.is_some() {
                LeaseInfo {
                    state: LeaseState::Stale,
                    ..i
                }
            } else {
                i
            })
        }
        Err(TryLockError::WouldBlock) => Ok(info(LeaseState::Held, &text)),
        Err(TryLockError::Error(e)) => Err(io_err(&path)(e)),
    }
}
