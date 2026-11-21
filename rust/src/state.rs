//! Native run store: a distinct, versioned state root that never reads or
//! writes Python lmloop state. Nothing here follows a symlink.

use crate::gate::GateRecord;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: u32 = 1;
/// Every snapshot of this version says this: the gate ran, but nothing ties
/// it to an exact Git snapshot, so it is not evidence anyone may reuse.
pub const EVIDENCE: &str = "diagnostic_only_not_bound_to_git_snapshot";
const STATE_SUBDIR: &str = "v1";
const MAX_JSON_BYTES: u64 = 1 << 20;

#[derive(Debug)]
pub struct Error {
    pub kind: &'static str,
    pub message: String,
}

impl Error {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Map an I/O error on `path`; ELOOP from `O_NOFOLLOW` means a symlink.
pub fn io_err(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    move |e| {
        if e.raw_os_error() == Some(libc::ELOOP) {
            Error::new("unsafe_path", format!("{} is a symlink", path.display()))
        } else {
            Error::new("io", format!("{}: {e}", path.display()))
        }
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Run IDs are one simple path component: no separators, no dots.
pub fn validate_run_id(id: &str) -> std::result::Result<String, String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(id.to_string())
    } else {
        Err("run id must be 1-64 characters of [A-Za-z0-9_-], starting alphanumeric".into())
    }
}

/// `--state-root`/`LMLOOP_RUST_STATE_ROOT`, else `$XDG_STATE_HOME/lmloop-rust`,
/// else `~/.local/state/lmloop-rust`. Always absolute, normal and with every
/// existing ancestor symlink resolved, so the overlap guard compares real paths.
///
/// Operator boundary: the guard refuses the Python default state directory
/// (`~/.local/state/lmloop`) and any directory that looks like an lmloop
/// checkout (`lmloop.py` + `rundir.py`) or a `.lmloop` directory. It cannot see
/// bind mounts or hard-link aliases, and it says nothing about another process
/// of the same user changing the tree concurrently; a custom root is the
/// operator's responsibility beyond that.
pub fn resolve_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let root = match explicit {
        Some(p) => p,
        None => match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            Some(x) => PathBuf::from(x).join("lmloop-rust"),
            None => match std::env::var_os("HOME").filter(|v| !v.is_empty()) {
                Some(h) => PathBuf::from(h).join(".local/state/lmloop-rust"),
                None => {
                    return Err(Error::new(
                        "config",
                        "no HOME or XDG_STATE_HOME; pass --state-root",
                    ));
                }
            },
        },
    };
    if !root.is_absolute() {
        return Err(Error::new("config", "state root must be an absolute path"));
    }
    let root = canonical(&root)?;
    refuse_legacy_overlap(&root)?;
    // Ancestors may be root's or ours, and world-writable only if sticky (so /tmp is fine).
    // Group-writable ancestors are tolerated: group membership is the operator's trust boundary.
    for dir in root.ancestors().skip(1) {
        if let Ok(m) = fs::metadata(dir) {
            let owner_ok = m.uid() == euid() || m.uid() == 0;
            if !owner_ok || (m.mode() & 0o002 != 0 && m.mode() & 0o1000 == 0) {
                return Err(Error::new(
                    "unsafe_path",
                    format!(
                        "ancestor {} is owned by another user or world-writable without the sticky bit",
                        dir.display()
                    ),
                ));
            }
        }
    }
    // An existing root must be ours and closed to group/other writes.
    match fs::metadata(&root) {
        Ok(m) if m.is_dir() && m.uid() == euid() && m.mode() & 0o022 == 0 => {}
        Ok(_) => {
            return Err(Error::new(
                "unsafe_path",
                format!(
                    "{} is not a directory owned by this user and closed to group/other writes",
                    root.display()
                ),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(&root)(e)),
    }
    Ok(root)
}

/// Reject `..`, then resolve the longest existing prefix and re-append the rest.
fn canonical(path: &Path) -> Result<PathBuf> {
    if !path
        .components()
        .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err(Error::new(
            "config",
            "state root must not contain '.' or '..' components",
        ));
    }
    let (mut head, mut tail) = (path, Vec::new());
    let mut resolved = loop {
        match fs::canonicalize(head) {
            Ok(p) => break p,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(head).is_ok() {
                    return Err(Error::new(
                        "unsafe_path",
                        format!("{} is a dangling symlink", head.display()),
                    ));
                }
                match (head.parent(), head.file_name()) {
                    (Some(parent), Some(name)) => {
                        tail.push(name);
                        head = parent;
                    }
                    _ => return Err(io_err(head)(e)),
                }
            }
            Err(e) => return Err(io_err(head)(e)),
        }
    };
    resolved.extend(tail.iter().rev());
    Ok(resolved)
}

fn refuse_legacy_overlap(root: &Path) -> Result<()> {
    let overlap = |why: &str| Err(Error::new("config", format!("state root {why}")));
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        let legacy = PathBuf::from(home).join(".local/state/lmloop");
        let legacy = canonical(&legacy).unwrap_or(legacy);
        if root.starts_with(&legacy) || legacy.starts_with(root) {
            return overlap("overlaps the Python lmloop state directory");
        }
    }
    for dir in root.ancestors() {
        let checkout = dir.join("lmloop.py").is_file() && dir.join("rundir.py").is_file();
        if checkout || dir.file_name().is_some_and(|n| n == ".lmloop") {
            return overlap("is inside a Python lmloop checkout or .lmloop directory");
        }
    }
    Ok(())
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// `Ok(true)` if `path` is a real directory owned by us with mode 0700,
/// `Ok(false)` if absent; a symlink, non-directory, foreign or shared
/// directory is refused.
pub fn private_dir(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && m.uid() == euid() && m.mode() & 0o077 == 0 => Ok(true),
        Ok(_) => Err(Error::new(
            "unsafe_path",
            format!(
                "{} is not a real directory owned by this user with mode 0700",
                path.display()
            ),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_err(path)(e)),
    }
}

/// Create a private directory if absent. EEXIST from a concurrent creator is
/// accepted only because the recheck then demands a safe directory.
pub fn ensure_private_dir(path: &Path, recursive: bool) -> Result<()> {
    if private_dir(path)? {
        return Ok(());
    }
    match DirBuilder::new()
        .recursive(recursive)
        .mode(0o700)
        .create(path)
    {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io_err(path)(e)),
    }
    if private_dir(path)? {
        Ok(())
    } else {
        Err(Error::new(
            "io",
            format!("{} vanished during create", path.display()),
        ))
    }
}

pub fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// An opened state file must be a regular file we own that nobody else can
/// access. Callers open with `O_NONBLOCK` so a FIFO cannot hang the open.
pub fn check_file(file: &File, path: &Path) -> Result<()> {
    let m = file.metadata().map_err(io_err(path))?;
    if m.is_file() && m.uid() == euid() && m.mode() & 0o077 == 0 {
        Ok(())
    } else {
        Err(Error::new(
            "unsafe_path",
            format!(
                "{} is not a regular file owned by this user with mode 0600",
                path.display()
            ),
        ))
    }
}

pub struct Store {
    base: PathBuf,
}

impl Store {
    pub fn new(root: &Path) -> Self {
        Self {
            base: root.join(STATE_SUBDIR),
        }
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    fn runs(&self) -> PathBuf {
        self.base.join("runs")
    }

    /// The run directory, or `None` if it does not exist and `create` is false.
    pub fn run_dir(&self, id: &str, create: bool) -> Result<Option<PathBuf>> {
        let runs = self.runs();
        let dir = runs.join(id);
        for p in [&self.base, &runs, &dir] {
            if create {
                ensure_private_dir(p, p == &self.base)?;
            } else if !private_dir(p)? {
                return Ok(None);
            }
        }
        Ok(Some(dir))
    }

    /// Most recently modified valid run directory, if any.
    pub fn latest_run(&self) -> Result<Option<String>> {
        let runs = self.runs();
        if !private_dir(&self.base)? || !private_dir(&runs)? {
            return Ok(None);
        }
        let mut best: Option<(SystemTime, String)> = None;
        for entry in fs::read_dir(&runs).map_err(io_err(&runs))? {
            let entry = entry.map_err(io_err(&runs))?;
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if validate_run_id(&name).is_err() || !meta.is_dir() {
                continue;
            }
            let modified = meta.modified().unwrap_or(UNIX_EPOCH);
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, name));
            }
        }
        Ok(best.map(|(_, n)| n))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    GateRunning,
    GateFinished,
    /// A signal stopped the owner, which reaped its process group first.
    Interrupted,
}

/// The persisted, versioned status document (`status.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema_version: u32,
    pub run_id: String,
    pub phase: Phase,
    pub updated_at_unix: u64,
    pub gate: Option<GateRecord>,
    /// Stale control files this process removed after taking ownership.
    pub cleared_stop: Vec<String>,
    /// Always false in this version: a diagnostic gate verifies nothing.
    pub verified_completion: bool,
    pub gate_evidence_reusable: bool,
    pub evidence: String,
}

impl Snapshot {
    pub fn new(
        run_id: &str,
        phase: Phase,
        gate: Option<GateRecord>,
        cleared_stop: Vec<String>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            run_id: run_id.into(),
            phase,
            updated_at_unix: now_unix(),
            gate,
            cleared_stop,
            verified_completion: false,
            gate_evidence_reusable: false,
            evidence: EVIDENCE.into(),
        }
    }
}

/// Read a private regular file without following a symlink, bounded; `None` if absent.
pub fn read_bounded(path: &Path) -> Result<Option<String>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(path)(e)),
    };
    check_file(&file, path)?;
    let mut text = String::new();
    file.take(MAX_JSON_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(io_err(path))?;
    if text.len() as u64 > MAX_JSON_BYTES {
        return Err(Error::new(
            "malformed",
            format!("{} exceeds {MAX_JSON_BYTES} bytes", path.display()),
        ));
    }
    Ok(Some(text))
}

pub fn read_snapshot(run_dir: &Path, run_id: &str) -> Result<Snapshot> {
    let path = run_dir.join("status.json");
    let text = read_bounded(&path)?.ok_or_else(|| {
        Error::new(
            "missing_snapshot",
            format!("{} does not exist", path.display()),
        )
    })?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| Error::new("malformed", e.to_string()))?;
    match value.get("schema_version").and_then(|v| v.as_u64()) {
        None => return Err(Error::new("malformed", "missing schema_version")),
        Some(v) if v != u64::from(SCHEMA_VERSION) => {
            return Err(Error::new(
                "unsupported_version",
                format!("schema_version {v}; this build reads {SCHEMA_VERSION}"),
            ));
        }
        Some(_) => {}
    }
    let snap: Snapshot =
        serde_json::from_value(value).map_err(|e| Error::new("malformed", e.to_string()))?;
    if snap.run_id != run_id {
        return Err(Error::new(
            "malformed",
            format!("snapshot is for run {:?}, not {run_id:?}", snap.run_id),
        ));
    }
    if snap.verified_completion || snap.gate_evidence_reusable || snap.evidence != EVIDENCE {
        return Err(Error::new(
            "malformed",
            "snapshot claims verified or reusable evidence this version cannot produce",
        ));
    }
    Ok(snap)
}

/// Atomic publish: temp file, fsync, rename, fsync the directory. Callers hold
/// the run lease, so there is one writer.
pub fn write_snapshot(run_dir: &Path, snap: &Snapshot) -> Result<()> {
    let tmp = run_dir.join(format!("status.json.{}.tmp", std::process::id()));
    let dest = run_dir.join("status.json");
    let mut body = serde_json::to_vec_pretty(snap).map_err(|e| Error::new("io", e.to_string()))?;
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
    File::open(run_dir)
        .and_then(|d| d.sync_all())
        .map_err(io_err(run_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_are_simple_components() {
        for ok in ["a", "run-1", "Run_2", "0abc"] {
            assert!(validate_run_id(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            ".hidden",
            "-x",
            "a b",
            "a.b",
            &"x".repeat(65),
        ] {
            assert!(validate_run_id(bad).is_err(), "{bad}");
        }
    }
}
