//! Worker protocol (M2): the OpenCode `run --format json` stream as far as the
//! serial runner needs it, plus the versioned event envelope the runner writes.
//!
//! The stream is evidence of *how the worker ended*, never of what it did:
//! Git is still the only witness for the work. A worker is accepted only if
//! every line is a JSON event carrying one consistent `sessionID`, none is an
//! `error`, and the last event is a `step_finish` whose `reason` is `stop`.

use crate::state::{self, Error, Result, check_file, io_err};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const EVENT_VERSION: u32 = 1;

#[derive(Debug, PartialEq, Eq)]
pub struct Summary {
    pub session_id: String,
    pub events: usize,
    pub tools: usize,
}

fn valid_session(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `Err` carries the reason the stream is not acceptable evidence of a clean stop.
pub fn parse(stdout: &str) -> std::result::Result<Summary, String> {
    let mut session: Option<String> = None;
    let (mut events, mut tools) = (0, 0);
    let (mut last_type, mut last_reason) = (String::new(), String::new());
    for (n, line) in stdout
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let n = n + 1;
        let v: Value =
            serde_json::from_str(line).map_err(|e| format!("line {n}: not JSON ({e})"))?;
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .ok_or(format!("line {n}: no type"))?;
        let sid = v
            .get("sessionID")
            .and_then(Value::as_str)
            .filter(|s| valid_session(s))
            .ok_or(format!("line {n}: missing or invalid sessionID"))?;
        match &session {
            None => session = Some(sid.to_string()),
            Some(s) if s != sid => return Err(format!("line {n}: sessionID changed mid-stream")),
            Some(_) => {}
        }
        match kind {
            "error" => return Err(format!("line {n}: error event")),
            "tool_use" => tools += 1,
            "step_finish" => {
                last_reason = v
                    .pointer("/part/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
            }
            _ => {}
        }
        last_type = kind.to_string();
        events += 1;
    }
    let Some(session_id) = session else {
        return Err("no events: nothing shows the worker ran".into());
    };
    if last_type != "step_finish" || last_reason != "stop" {
        return Err(format!(
            "no clean final stop (last event {last_type:?}, reason {last_reason:?})"
        ));
    }
    Ok(Summary {
        session_id,
        events,
        tools,
    })
}

/// A completed tool call read from one stdout line. OpenCode's CLI emits `tool_use` only once a
/// call has finished, so nothing here can say a tool is *in flight*; a line that does carry a
/// non-final `state.status` is ignored rather than guessed at.
#[derive(Debug, PartialEq, Eq)]
pub struct ToolEvent {
    /// Charset-limited, at most 64 bytes; the raw name is worker text.
    pub name: String,
    /// Hash of `state.input` + `state.output`; `None` unless both are present.
    pub fingerprint: Option<u64>,
}

pub fn tool_event(line: &[u8]) -> Option<ToolEvent> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let v: Value = serde_json::from_slice(line).ok()?;
    if v.get("type")?.as_str()? != "tool_use" || !valid_session(v.get("sessionID")?.as_str()?) {
        return None;
    }
    let state = v.pointer("/part/state");
    let status = state.and_then(|s| s.get("status")).and_then(Value::as_str);
    if status.is_some_and(|s| s != "completed") {
        return None;
    }
    let name: String = v
        .pointer("/part/tool")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.:-".contains(c) {
                c
            } else {
                '?'
            }
        })
        .take(64)
        .collect();
    let fingerprint = state
        .and_then(|s| Some((s.get("input").filter(|v| !v.is_null())?, s.get("output")?)))
        .map(|(i, o)| {
            let mut h = DefaultHasher::new();
            (i.to_string(), o.to_string()).hash(&mut h);
            h.finish()
        });
    Some(ToolEvent { name, fingerprint })
}

/// The bridge wire: an external trusted writer appends the literal record `model\n` to a private
/// file the operator created beforehand, after each substantive coalesced model delta. No
/// timestamps and no content, so nothing in it can describe what the model said.
const PROGRESS_RECORD: &[u8; 6] = b"model\n";
const MAX_PROGRESS_BYTES: u64 = 4 << 20;

/// Reader for that file. Liveness is never inferred from its length alone: every poll that sees
/// growth validates the newly appended bytes as whole `model\n` records (a partial final record,
/// as a writer mid-append, is not counted yet), and the path must still name the inode opened. A
/// shrink, a replacement, a hard link, any other byte, or growth past the bound is an error, and
/// the watchdog fails closed. Records present at `open` (earlier tasks) are the baseline.
///
/// shortcut: a same-inode truncate-and-rewrite that is longer again by the next poll looks like
/// honest growth, because every valid byte is identical; the writer is trusted not to do that.
pub struct Progress {
    file: File,
    path: PathBuf,
    ident: (u64, u64),
    /// `Some(writer_gid)` only for the opt-in shared file (see `open_shared`).
    shared_gid: Option<u32>,
    /// Bytes validated as whole records, and the file length last seen (whole records + tail).
    valid: u64,
    seen_len: u64,
    base: u64,
}

/// Opt-in shared-file trust: a regular file, exactly 0620, owned by this (backend) uid with the
/// dedicated writer gid, one link. Group write is not kernel append-only, so this trusts the relay
/// in that group; the worker must not see the path or belong to the group.
fn shared_trusted(m: &fs::Metadata, gid: u32) -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    m.is_file() && m.nlink() == 1 && m.mode() & 0o7777 == 0o620 && m.uid() == euid && m.gid() == gid
}

/// Walk to the file's parent using directory descriptors, refusing symlinks and ancestry
/// replacement. The immediate parent must be private; sticky writable ancestors are accepted only
/// when root-owned (for example /tmp), since sticky prevents other users replacing our private dir.
fn protected_parent(path: &Path, ident: (u64, u64), gid: u32) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(name) = path.file_name() else {
        return false;
    };
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let root = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
    {
        Ok(f) => f,
        Err(_) => return false,
    };
    let Ok(root_meta) = root.metadata() else {
        return false;
    };
    if !root_meta.is_dir() || root_meta.uid() != 0 || root_meta.mode() & 0o022 != 0 {
        return false;
    }
    let mut dir = root;
    let components: Vec<_> = parent
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n),
            std::path::Component::RootDir => None,
            _ => Some(std::ffi::OsStr::new("..")),
        })
        .collect();
    for (i, component) in components.iter().enumerate() {
        let c = match std::ffi::CString::new(component.as_bytes()) {
            Ok(c) => c,
            Err(_) => return false,
        };
        // SAFETY: dir is an open directory and c is NUL terminated.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return false;
        }
        // SAFETY: openat returned a fresh owned descriptor.
        dir = unsafe { File::from_raw_fd(fd) };
        let Ok(m) = dir.metadata() else { return false };
        let mode = m.mode();
        if !m.is_dir() || (m.uid() != euid && m.uid() != 0) {
            return false;
        }
        let immediate = i + 1 == components.len();
        if immediate {
            if mode & 0o022 != 0 {
                return false;
            }
        } else if mode & 0o022 != 0 && !(m.uid() == 0 && mode & 0o1000 != 0) {
            return false;
        }
    }
    let c = match std::ffi::CString::new(name.as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // SAFETY: dir is the opened parent and c is NUL terminated; no-follow pins the leaf.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return false;
    }
    // SAFETY: openat returned a fresh owned descriptor.
    let leaf = unsafe { File::from_raw_fd(fd) };
    leaf.metadata()
        .is_ok_and(|m| (m.dev(), m.ino()) == ident && shared_trusted(&m, gid))
}

impl Progress {
    /// `path` must be absolute, already normal (no symlink in it), outside `repo`, and a regular
    /// file we own with mode 0600 and a single link. The operator creates it beforehand.
    pub fn open(path: &Path, repo: &Path) -> Result<Self> {
        Self::open_with(path, repo, None)
    }

    /// Opt-in for a relay under another account: as `open`, but the file must instead be exactly
    /// 0620, owned by this uid, group `writer_gid`, with only protected ancestors. `poll` rechecks
    /// all of it, so a chmod, chown, relink or loosened parent fails closed.
    pub fn open_shared(path: &Path, repo: &Path, writer_gid: u32) -> Result<Self> {
        Self::open_with(path, repo, Some(writer_gid))
    }

    fn open_with(path: &Path, repo: &Path, shared_gid: Option<u32>) -> Result<Self> {
        let bad = |m: &str| Error::new("progress_unsafe", format!("{}: {m}", path.display()));
        if !path.is_absolute() || fs::canonicalize(path).map_err(io_err(path))? != path {
            return Err(bad(
                "must be an absolute, normal path with no symlink in it",
            ));
        }
        if path.starts_with(fs::canonicalize(repo).map_err(io_err(repo))?) {
            return Err(bad("must live outside the repository checkout"));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(io_err(path))?;
        let m = file.metadata().map_err(io_err(path))?;
        if let Some(gid) = shared_gid {
            if !shared_trusted(&m, gid) {
                return Err(bad(
                    "shared file must be a regular, single-link 0620 file owned by this uid and the writer gid",
                ));
            }
            if !protected_parent(path, (m.dev(), m.ino()), gid) {
                return Err(bad(
                    "shared file's parents must be owned by us or root and not group/other writable",
                ));
            }
        } else {
            check_file(&file, path)?;
            if m.nlink() != 1 {
                return Err(bad("has other hard links"));
            }
        }
        let mut p = Self {
            file,
            path: path.into(),
            ident: (m.dev(), m.ino()),
            shared_gid,
            valid: 0,
            seen_len: 0,
            base: 0,
        };
        p.grow(m.len())?;
        p.base = p.valid / PROGRESS_RECORD.len() as u64;
        Ok(p)
    }

    /// Records already in the file when it was opened (earlier tasks of the run).
    pub fn baseline(&self) -> u64 {
        self.base
    }

    /// True only for a zero-byte file at `open`: a partial `model` prefix has baseline 0 but is
    /// not empty, and could not be told from a live writer's.
    pub fn was_empty_at_open(&self) -> bool {
        self.seen_len == 0
    }

    fn bad(&self, m: &str) -> Error {
        Error::new("progress_invalid", format!("{}: {m}", self.path.display()))
    }

    /// Validate bytes `valid..len` and advance. Only a prefix of `model\n` may trail.
    fn grow(&mut self, len: u64) -> Result<()> {
        if len > MAX_PROGRESS_BYTES {
            return Err(self.bad("exceeds the size bound"));
        }
        let mut buf = vec![0u8; (len - self.valid) as usize];
        self.file
            .read_exact_at(&mut buf, self.valid)
            .map_err(|_| self.bad("shrank while being read"))?;
        let rec = PROGRESS_RECORD.len();
        let whole = buf.len() / rec * rec;
        let (body, tail) = buf.split_at(whole);
        if body.chunks(rec).any(|r| r != PROGRESS_RECORD) || !PROGRESS_RECORD.starts_with(tail) {
            return Err(self.bad("holds bytes that are not whole `model` records"));
        }
        self.valid += whole as u64;
        self.seen_len = len;
        Ok(())
    }

    /// Complete valid records appended since `open`.
    pub fn poll(&mut self) -> Result<u64> {
        let disk = fs::symlink_metadata(&self.path).map_err(io_err(&self.path))?;
        if !disk.is_file() || (disk.dev(), disk.ino()) != self.ident {
            return Err(self.bad("was replaced"));
        }
        let m = self.file.metadata().map_err(io_err(&self.path))?;
        if m.nlink() != 1 {
            return Err(self.bad("gained a hard link"));
        }
        if let Some(gid) = self.shared_gid
            && (!shared_trusted(&m, gid) || !protected_parent(&self.path, self.ident, gid))
        {
            return Err(self.bad("shared file's mode, owner, group or parents changed"));
        }
        if m.len() < self.seen_len {
            return Err(self.bad("was truncated"));
        }
        if m.len() > self.seen_len {
            self.grow(m.len())?;
        }
        Ok(self.valid / PROGRESS_RECORD.len() as u64 - self.base)
    }
}

/// Journal bounds. A run has at most 16 tasks and about 6 records per task, so
/// 1024 records leaves room for hundreds of pause/resume cycles; a record is far
/// below 4 MiB (the widest is `snapshot_captured`'s changed-path list). Both are
/// enforced before an append and while scanning, so recovery never reads more
/// than `MAX_JOURNAL_BYTES`. A run that outgrows them stops with `journal_too_large`.
const MAX_EVENTS: u64 = 1024;
const MAX_LINE_BYTES: usize = 4 << 20;
const MAX_JOURNAL_BYTES: u64 = 32 << 20;
const ARCHIVE_DIR: &str = "journal-recovery";
const MAX_ARCHIVES_PER_OFFSET: u32 = 8;
const RECORD_KEYS: [&str; 7] = ["v", "seq", "at_unix", "run_id", "kind", "task", "data"];

/// Journal problems are `journal_*` errors: a refusal that wrote nothing to the log.
fn journal_unsafe(e: Error) -> Error {
    if e.kind == "unsafe_path" {
        Error::new("journal_unsafe", e.message)
    } else {
        e
    }
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_err(dir))
}

/// What a scan accepted: `seq` complete records filling the first `len` bytes,
/// then `tail`, the unterminated final line (if any).
#[derive(Debug, PartialEq, Eq)]
struct Journal {
    seq: u64,
    len: u64,
    tail: Vec<u8>,
}

fn check_record(line: &[u8], seq: u64, run_id: &str) -> Result<()> {
    let bad = |k: &'static str, m: String| Error::new(k, format!("events.jsonl record {seq}: {m}"));
    let v: Value = serde_json::from_slice(line)
        .map_err(|e| bad("journal_malformed", format!("not JSON ({e})")))?;
    let o = v
        .as_object()
        .filter(|o| o.len() == RECORD_KEYS.len() && RECORD_KEYS.iter().all(|k| o.contains_key(*k)))
        .ok_or_else(|| bad("journal_malformed", "not a v1 event object".into()))?;
    let shape = o["at_unix"].is_u64()
        && o["kind"].as_str().is_some_and(|k| !k.is_empty())
        && (o["task"].is_null() || o["task"].is_string());
    match (o["v"].as_u64(), o["run_id"].as_str(), shape) {
        (Some(v), Some(_), true) if v != u64::from(EVENT_VERSION) => Err(bad(
            "journal_version",
            format!("version {v}; this build reads {EVENT_VERSION}"),
        )),
        (Some(_), Some(r), true) if r != run_id => {
            Err(bad("journal_foreign", "belongs to another run".into()))
        }
        (Some(_), Some(_), true) if o["seq"].as_u64() != Some(seq) => Err(bad(
            "journal_sequence",
            format!("seq is {}, expected {seq}", o["seq"]),
        )),
        (Some(_), Some(_), true) => Ok(()),
        _ => Err(bad("journal_malformed", "wrong field types".into())),
    }
}

/// Policy: every `\n`-terminated line must be a valid, sequential v1 record of
/// `run_id` or the scan refuses. Only bytes after the last `\n` are a torn tail,
/// whatever they hold: a complete-looking JSON record without its delimiter was
/// never acknowledged, so it is torn too.
fn scan(r: impl Read, run_id: &str) -> Result<Journal> {
    let mut r = BufReader::new(r.take(MAX_JOURNAL_BYTES + 1));
    let (mut seq, mut len, mut buf) = (0u64, 0u64, Vec::new());
    let too_large = |m: &str| Error::new("journal_too_large", format!("events.jsonl {m}"));
    loop {
        buf.clear();
        let n = r
            .read_until(b'\n', &mut buf)
            .map_err(|e| Error::new("io", format!("events.jsonl: {e}")))?;
        if n == 0 {
            return Ok(Journal {
                seq,
                len,
                tail: buf,
            });
        }
        if n > MAX_LINE_BYTES || len + n as u64 > MAX_JOURNAL_BYTES {
            return Err(too_large("exceeds the line or file bound"));
        }
        if buf.last() != Some(&b'\n') {
            return Ok(Journal {
                seq,
                len,
                tail: buf,
            });
        }
        seq += 1;
        if seq > MAX_EVENTS {
            return Err(too_large("holds more records than the bound"));
        }
        check_record(&buf[..n - 1], seq, run_id)?;
        len += n as u64;
    }
}

/// `None` if absent (and not `create`). Refuses a symlink, a foreign or shared file, and a hard link.
/// A journal made here is created exclusively, and its parent directory is fsynced.
fn open_journal(path: &Path, write: bool, create: bool) -> Result<Option<File>> {
    let open = |new: bool| {
        let mut o = OpenOptions::new();
        o.read(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        o.append(write).create_new(create && new);
        o.open(path)
    };
    let mut created = false;
    let opened = match (create, open(create)) {
        (true, Err(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => open(false),
        (_, r) => {
            created = create && r.is_ok();
            r
        }
    };
    let file = match opened {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !create => return Ok(None),
        Err(e) => return Err(journal_unsafe(io_err(path)(e))),
    };
    check_file(&file, path).map_err(journal_unsafe)?;
    reject_hard_link(&file, path)?;
    if created {
        sync_dir(path.parent().unwrap_or(Path::new(".")))?;
    }
    Ok(Some(file))
}

fn reject_hard_link(file: &File, path: &Path) -> Result<()> {
    if file.metadata().map_err(io_err(path))?.nlink() != 1 {
        return Err(Error::new(
            "journal_unsafe",
            format!("{} has other hard links", path.display()),
        ));
    }
    Ok(())
}

/// Atomic rename that never replaces `to`: Linux `renameat2(RENAME_NOREPLACE)`. Where the
/// kernel or filesystem lacks it (ENOSYS/EINVAL), plain `rename` is used; that is only safe
/// because the caller holds the exclusive run lease and has just seen `to` absent.
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes());
    let (from_c, to_c) = (c(from)?, c(to)?);
    // SAFETY: both pointers are valid NUL-terminated strings for the duration of the call.
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOSYS | libc::EINVAL) => fs::rename(from, to),
        _ => Err(e),
    }
}

/// Keep `tail` byte for byte in `journal-recovery/events-tail-<offset>.<k>`; returns once a
/// durable copy exists. The name is deterministic, so a retry after an interruption finds
/// and reuses a matching archive (fsynced before the directory); a different one at the same
/// name is never touched (next `k`), and an unsafe one (not private, or hard-linked) is refused.
/// The copy is written to a unique `create_new` temp, fsynced, then atomically renamed to its
/// name without replacing anything: a partial file never holds an archive name, no interruption
/// leaves the archive hard-linked, and no existing file is ever truncated or reused.
fn archive_tail(run_dir: &Path, offset: u64, tail: &[u8]) -> Result<()> {
    let dir = run_dir.join(ARCHIVE_DIR);
    state::ensure_private_dir(&dir, false).map_err(journal_unsafe)?;
    sync_dir(run_dir)?;
    for k in 0..MAX_ARCHIVES_PER_OFFSET {
        let dest = dir.join(format!("events-tail-{offset}.{k}"));
        match fs::symlink_metadata(&dest) {
            Ok(_) => {
                let mut f = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&dest)
                    .map_err(|e| journal_unsafe(io_err(&dest)(e)))?;
                check_file(&f, &dest).map_err(journal_unsafe)?;
                reject_hard_link(&f, &dest)?;
                let mut have = Vec::new();
                (&mut f)
                    .take(tail.len() as u64 + 1)
                    .read_to_end(&mut have)
                    .map_err(io_err(&dest))?;
                if have == tail {
                    f.sync_all().map_err(io_err(&dest))?;
                    return sync_dir(&dir);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let tmp = dir.join(format!(
                    "events-tail-{offset}.{k}.{}-{}.tmp",
                    std::process::id(),
                    state::now_nanos()
                ));
                let mut f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&tmp)
                    .map_err(|e| journal_unsafe(io_err(&tmp)(e)))?;
                f.write_all(tail)
                    .and_then(|()| f.sync_all())
                    .map_err(io_err(&tmp))?;
                rename_noreplace(&tmp, &dest).map_err(io_err(&dest))?;
                return sync_dir(&dir);
            }
            Err(e) => return Err(io_err(&dest)(e)),
        }
    }
    Err(Error::new(
        "journal_archive_full",
        format!(
            "{ARCHIVE_DIR} already holds {MAX_ARCHIVES_PER_OFFSET} different tails at offset {offset}"
        ),
    ))
}

/// Under the run lease, make `events.jsonl` safe to append to. Scans first and refuses
/// (`journal_*`) on any malformed, foreign, wrong-version or misnumbered complete record
/// before writing anything. A torn final line is archived durably, then only that suffix
/// is truncated; complete records are never dropped or rewritten. Idempotent.
pub fn recover(run_dir: &Path, run_id: &str) -> Result<()> {
    let path = run_dir.join("events.jsonl");
    let Some(file) = open_journal(&path, true, false)? else {
        return Ok(());
    };
    let j = scan(&file, run_id)?;
    if j.tail.is_empty() {
        return Ok(());
    }
    archive_tail(run_dir, j.len, &j.tail)?;
    let size = file.metadata().map_err(io_err(&path))?.len();
    if size != j.len + j.tail.len() as u64 {
        return Err(Error::new(
            "journal_malformed",
            "events.jsonl changed during recovery",
        ));
    }
    file.set_len(j.len)
        .and_then(|()| file.sync_all())
        .map_err(io_err(&path))
}

/// Append-only `events.jsonl`: one `{v, seq, at_unix, run_id, kind, task, data}`
/// object per line, fsynced. Callers persist state first and emit after, so a
/// missing event never hides a state change. Payloads are ours, never worker text.
pub struct EventLog {
    file: File,
    seq: u64,
    len: u64,
}

impl EventLog {
    /// Validates the whole journal; an unrecovered torn tail is refused (`journal_torn`),
    /// since appending after it would glue two records together. Call `recover` first.
    /// Only a new run (`create`) may make the journal; otherwise a missing one is refused
    /// (`journal_missing`), because history is never invented for a run that has state.
    pub fn open(run_dir: &Path, run_id: &str, create: bool) -> Result<Self> {
        let path = run_dir.join("events.jsonl");
        let file = open_journal(&path, true, create)?.ok_or_else(|| {
            Error::new(
                "journal_missing",
                format!(
                    "{} is missing; run state exists without its history",
                    path.display()
                ),
            )
        })?;
        let j = scan(&file, run_id)?;
        if !j.tail.is_empty() {
            return Err(Error::new(
                "journal_torn",
                format!(
                    "events.jsonl ends with {} unterminated bytes at offset {}",
                    j.tail.len(),
                    j.len
                ),
            ));
        }
        Ok(Self {
            file,
            seq: j.seq,
            len: j.len,
        })
    }

    pub fn append(
        &mut self,
        run_id: &str,
        kind: &str,
        task: Option<&str>,
        data: Value,
    ) -> Result<()> {
        let mut line = serde_json::to_vec(&json!({
            "v": EVENT_VERSION,
            "seq": self.seq + 1,
            "at_unix": crate::state::now_unix(),
            "run_id": run_id,
            "kind": kind,
            "task": task,
            "data": data,
        }))
        .map_err(|e| Error::new("io", e.to_string()))?;
        line.push(b'\n');
        if self.seq >= MAX_EVENTS
            || line.len() > MAX_LINE_BYTES
            || self.len + line.len() as u64 > MAX_JOURNAL_BYTES
        {
            return Err(Error::new(
                "journal_too_large",
                "events.jsonl is at its record or size bound; nothing was appended",
            ));
        }
        self.file
            .write_all(&line)
            .and_then(|()| self.file.sync_data())
            .map_err(|e| Error::new("io", format!("events.jsonl: {e}")))?;
        self.seq += 1;
        self.len += line.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(seq: u64) -> String {
        format!(
            "{{\"v\":1,\"seq\":{seq},\"at_unix\":1,\"run_id\":\"r\",\"kind\":\"k\",\"task\":null,\"data\":{{}}}}\n"
        )
    }

    #[test]
    fn scan_treats_only_the_unterminated_suffix_as_torn_and_bounds_records() {
        let two = rec(1) + &rec(2);
        let j = scan(two.as_bytes(), "r").unwrap();
        assert_eq!((j.seq, j.len, j.tail.len()), (2, two.len() as u64, 0));
        // A valid record without its delimiter is torn, not counted.
        let torn = rec(3);
        let j = scan(format!("{two}{}", torn.trim_end()).as_bytes(), "r").unwrap();
        assert_eq!(
            (j.seq, j.len, j.tail.len()),
            (2, two.len() as u64, torn.len() - 1)
        );
        // Complete bad records are refused, torn tail or not.
        for (bad, kind) in [
            (format!("{two}junk\n"), "journal_malformed"),
            (format!("{two}{}", rec(4)), "journal_sequence"),
            (rec(1).replace("\"r\"", "\"x\""), "journal_foreign"),
            (rec(1).replace("\"v\":1", "\"v\":2"), "journal_version"),
        ] {
            assert_eq!(scan(bad.as_bytes(), "r").unwrap_err().kind, kind, "{bad}");
        }
        let many: String = (1..=MAX_EVENTS + 1).map(rec).collect();
        assert_eq!(
            scan(many.as_bytes(), "r").unwrap_err().kind,
            "journal_too_large"
        );
    }

    fn ev(kind: &str, sid: &str, reason: &str) -> String {
        format!(
            "{{\"type\":\"{kind}\",\"sessionID\":\"{sid}\",\"part\":{{\"reason\":\"{reason}\"}}}}\n"
        )
    }

    #[test]
    fn accepts_only_a_clean_final_stop() {
        let ok = ev("step_start", "ses_a", "")
            + &ev("tool_use", "ses_a", "")
            + &ev("step_finish", "ses_a", "stop");
        let s = parse(&ok).unwrap();
        assert_eq!((s.session_id.as_str(), s.events, s.tools), ("ses_a", 3, 1));
        for bad in [
            String::new(),
            "not json\n".into(),
            ev("step_finish", "ses_a", "tool-calls"),
            ev("step_finish", "ses_a", "stop") + &ev("text", "ses_a", ""),
            ev("step_finish", "ses_a", "stop") + &ev("error", "ses_a", ""),
            ev("text", "ses_a", "") + &ev("step_finish", "ses_b", "stop"),
            ev("step_finish", "", "stop"),
            ev("step_finish", "ses/../x", "stop"),
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn progress_counts_whole_records_past_its_baseline_and_fails_closed() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lmloop-progress-{}", std::process::id()));
        let (repo, other) = (dir.join("repo"), dir.join("out"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&other).unwrap();
        let (repo, other) = (
            fs::canonicalize(repo).unwrap(),
            fs::canonicalize(other).unwrap(),
        );
        let mk = |p: &Path, body: &str| {
            fs::write(p, body).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o600)).unwrap();
        };
        // Inside the checkout: refused.
        mk(&repo.join("p"), "");
        assert_eq!(
            Progress::open(&repo.join("p"), &repo).err().unwrap().kind,
            "progress_unsafe"
        );

        let p = other.join("p");
        mk(&p, "model\nmodel\n");
        let mut r = Progress::open(&p, &repo).unwrap();
        assert_eq!((r.baseline(), r.poll().unwrap()), (2, 0));
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"model\nmod").unwrap(); // a writer mid-append: only the whole record counts
        assert_eq!(r.poll().unwrap(), 1);
        f.write_all(b"el\n").unwrap();
        assert_eq!(r.poll().unwrap(), 2);
        f.write_all(b"junk\n").unwrap();
        assert_eq!(r.poll().unwrap_err().kind, "progress_invalid");

        // Shorter than seen: truncation. Same length, other inode: replacement.
        mk(&p, "model\nmodel\n");
        let mut r = Progress::open(&p, &repo).unwrap();
        fs::write(&p, "").unwrap();
        assert!(r.poll().unwrap_err().message.contains("truncated"));
        mk(&p, "model\nmodel\n");
        let mut r = Progress::open(&p, &repo).unwrap();
        mk(&other.join("new"), "model\nmodel\n");
        fs::rename(other.join("new"), &p).unwrap();
        assert!(r.poll().unwrap_err().message.contains("replaced"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_progress_is_opt_in_and_rechecks_trust_at_every_poll() {
        use std::io::Write;
        use std::os::unix::fs::{PermissionsExt, chown};
        let dir = std::env::temp_dir().join(format!("lmloop-shared-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        fs::create_dir_all(dir.join("out")).unwrap();
        for d in [&dir, &dir.join("repo"), &dir.join("out")] {
            fs::set_permissions(d, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let (repo, out) = (
            fs::canonicalize(dir.join("repo")).unwrap(),
            fs::canonicalize(dir.join("out")).unwrap(),
        );
        let p = out.join("p");
        let gid = fs::metadata(&out).unwrap().gid(); // a group we belong to: no privilege needed
        let mk = |p: &Path, body: &str, mode: u32| {
            let _ = fs::remove_file(p);
            fs::write(p, body).unwrap();
            chown(p, None, Some(gid)).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
        };
        let kind = |r: Result<Progress>| r.err().map(|e| e.kind);

        // Default stays owner-only: a 0620 file is refused by `open`, a 0600 file by `open_shared`.
        mk(&p, "model\n", 0o620);
        assert_eq!(kind(Progress::open(&p, &repo)), Some("unsafe_path"));
        mk(&p, "model\n", 0o600);
        assert!(Progress::open(&p, &repo).is_ok());
        assert_eq!(
            kind(Progress::open_shared(&p, &repo, gid)),
            Some("progress_unsafe")
        );

        // Same checks at open and at poll for the shared file: wrong gid, other modes, extra link.
        mk(&p, "model\n", 0o620);
        assert_eq!(
            kind(Progress::open_shared(&p, &repo, gid.wrapping_add(1))),
            Some("progress_unsafe")
        );
        for mode in [0o660, 0o621, 0o720, 0o2620] {
            fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                kind(Progress::open_shared(&p, &repo, gid)),
                Some("progress_unsafe"),
                "{mode:o}"
            );
        }
        mk(&p, "model\n", 0o620);
        let mut r = Progress::open_shared(&p, &repo, gid).unwrap();
        assert_eq!((r.baseline(), r.poll().unwrap()), (1, 0));
        OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"model\n")
            .unwrap();
        assert_eq!(r.poll().unwrap(), 1);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o660)).unwrap();
        assert!(r.poll().unwrap_err().message.contains("changed"));
        fs::set_permissions(&p, fs::Permissions::from_mode(0o620)).unwrap();
        assert_eq!(r.poll().unwrap(), 1); // restoring the exact mode restores trust
        fs::hard_link(&p, out.join("link")).unwrap();
        assert!(r.poll().unwrap_err().message.contains("hard link"));
        fs::remove_file(out.join("link")).unwrap();
        fs::set_permissions(&out, fs::Permissions::from_mode(0o770)).unwrap(); // group-writable parent
        assert!(r.poll().unwrap_err().message.contains("parents"));
        assert_eq!(
            kind(Progress::open_shared(&p, &repo, gid)),
            Some("progress_unsafe")
        );
        fs::set_permissions(&out, fs::Permissions::from_mode(0o700)).unwrap();

        // A replacement inode, even one with identical metadata and content, is refused.
        let mut r = Progress::open_shared(&p, &repo, gid).unwrap();
        mk(&out.join("new"), "model\n", 0o620);
        fs::rename(out.join("new"), &p).unwrap();
        assert!(r.poll().unwrap_err().message.contains("replaced"));

        // Outside the checkout still applies.
        mk(&repo.join("p"), "", 0o620);
        assert_eq!(
            kind(Progress::open_shared(&repo.join("p"), &repo, gid)),
            Some("progress_unsafe")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_events_are_completed_only_and_bounded() {
        let line = |status: &str| {
            format!(
                "{{\"type\":\"tool_use\",\"sessionID\":\"s1\",\"part\":{{\"tool\":\"re ad\\u001b\",\"state\":{{\"status\":\"{status}\",\"input\":{{\"a\":1}},\"output\":\"x\"}}}}}}"
            )
        };
        let t = tool_event(line("completed").as_bytes()).unwrap();
        assert_eq!(t.name, "re?ad?");
        assert!(t.fingerprint.is_some());
        assert_eq!(
            t.fingerprint,
            tool_event(line("completed").as_bytes())
                .unwrap()
                .fingerprint
        );
        for not_done in ["running", "pending"] {
            assert!(tool_event(line(not_done).as_bytes()).is_none());
        }
        assert!(tool_event(b"{\"type\":\"text\",\"sessionID\":\"s1\"}").is_none());
        assert!(tool_event(b"not json").is_none());
        // No state at all: still a completed call (that is what the CLI emits), but nothing to compare.
        let bare = br#"{"type":"tool_use","sessionID":"s1","part":{"tool":"bash"}}"#;
        assert_eq!(tool_event(bare).unwrap().fingerprint, None);
    }
}
