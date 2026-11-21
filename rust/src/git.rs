//! Git snapshot helper (M2): bind a gate to an exact Git tree.
//!
//! Flow: `Repo::open` (attached branch, clean baseline) -> `Repo::capture`
//! (temporary index, literal scopes, exact tree OID, verified clean export for
//! the gate) -> the caller runs and verifies the gate -> only then
//! `Repo::commit_tree` and `Repo::advance_ref`. Nothing before those two
//! creates a commit, and nothing here resets, cleans, removes or pushes.
//!
//! Every `git` runs with a cleared environment (no `GIT_*` injection, no user
//! or system config), hooks pointed at `/dev/null`, and conversions off. Only
//! the temporary index is ever written by `capture`; the real index is never
//! touched, so a failed capture leaves the repository as it was. Objects it
//! wrote (blobs, trees) stay in the object store, unreferenced.
//!
//! A configured `filter.*` driver is refused at `open` and again at `capture`, and the raw
//! worktree bytes of every changed file must hash to the blob in the tree (so attribute-driven
//! eol/ident/encoding conversions are refused rather than silently gated against other bytes).
//! `sync_real_index` is the only real-index write, and is guarded by `index.lock`.
//!
//! shortcut: `Command` is trusted to find the right `git`/`tar` on PATH, and the
//! commit identity is fixed (`lmloop <lmloop@localhost>`); upgrade if either matters.

use crate::state::{Error, Result, io_err};
use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

const OPTIONS: &[&str] = &[
    "--no-replace-objects",
    "--literal-pathspecs",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.safecrlf=false",
    "-c",
    "core.eol=lf",
    "-c",
    "core.fileMode=true",
    "-c",
    "core.symlinks=true",
    "-c",
    "core.ignoreCase=false",
    "-c",
    "commit.gpgsign=false",
];

/// An attached-branch worktree whose baseline was clean when opened.
#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
    git_dir: PathBuf,
    /// Full ref name, e.g. `refs/heads/main`.
    pub ref_name: String,
    /// Commit the branch pointed at when opened; the CAS expectation.
    pub head: String,
}

/// An exact tree plus a clean export of it. Holding one proves nothing about
/// the gate; the caller must run and verify the gate against `export`.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub ref_name: String,
    pub head: String,
    /// Exact Git tree OID of baseline + scoped changes.
    pub tree: String,
    /// Paths changed relative to `head`, all inside the scopes.
    pub changed: Vec<String>,
    /// Directory holding exactly `tree`'s files; run the gate here.
    pub export: PathBuf,
}

fn err(kind: &'static str, message: impl Into<String>) -> Error {
    Error::new(kind, message)
}

fn command(program: &str) -> Command {
    let mut c = Command::new(program);
    c.env_clear();
    if let Some(p) = std::env::var_os("PATH") {
        c.env("PATH", p);
    }
    c
}

fn git_command(cwd: &Path, index: Option<&Path>, args: &[&str]) -> Command {
    let mut c = command("git");
    c.current_dir(cwd)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_NAME", "lmloop")
        .env("GIT_AUTHOR_EMAIL", "lmloop@localhost")
        .env("GIT_COMMITTER_NAME", "lmloop")
        .env("GIT_COMMITTER_EMAIL", "lmloop@localhost")
        .args(OPTIONS)
        .args(args);
    if let Some(i) = index {
        c.env("GIT_INDEX_FILE", i);
    }
    c
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

/// Error text only: git's stderr, capped.
fn brief(bytes: &[u8]) -> String {
    text(&bytes[..bytes.len().min(4096)])
}

/// Read a child's stderr on a thread, keeping the first 4 KiB and discarding the rest, so a
/// chatty child can neither fill the pipe and block nor grow our memory.
fn drain(mut r: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let _ = (&mut r).take(4096).read_to_end(&mut kept);
        let _ = std::io::copy(&mut r, &mut std::io::sink());
        kept
    })
}

/// Run git, returning its output whatever the exit status.
fn run(cwd: &Path, index: Option<&Path>, args: &[&str], stdin: Option<Vec<u8>>) -> Result<Output> {
    let mut c = git_command(cwd, index, args);
    c.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = c
        .spawn()
        .map_err(|e| err("git", format!("spawn git: {e}")))?;
    // A writer thread, so a large stdin and a large stdout cannot deadlock.
    let writer = stdin.map(|data| {
        let mut si = child.stdin.take().expect("piped stdin");
        thread::spawn(move || {
            let _ = si.write_all(&data);
        })
    });
    let out = child
        .wait_with_output()
        .map_err(|e| err("git", format!("wait git: {e}")))?;
    if let Some(w) = writer {
        let _ = w.join();
    }
    Ok(out)
}

fn ok(cwd: &Path, index: Option<&Path>, args: &[&str], stdin: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let out = run(cwd, index, args, stdin)?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(err(
            "git",
            format!("git {}: {}", args.join(" "), brief(&out.stderr)),
        ))
    }
}

fn utf8(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| err("git_path", "path is not valid UTF-8"))
}

/// Literal, relative, normalised, outside `.git`; never a glob or a pathspec magic.
fn check_scope(s: &str) -> Result<()> {
    let bad = s.is_empty()
        || s.starts_with('/')
        || s.ends_with('/')
        || s.contains('\0')
        || s.split('/')
            .any(|c| c.is_empty() || c == "." || c == ".." || c.eq_ignore_ascii_case(".git"));
    if bad {
        return Err(err("scope", format!("unsafe scope path {s:?}")));
    }
    Ok(())
}

fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Porcelain v1 `-z` status as (index flag, worktree flag, path), renames off.
fn status(cwd: &Path, index: Option<&Path>) -> Result<Vec<(u8, u8, String)>> {
    let out = ok(
        cwd,
        index,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
        None,
    )?;
    out.split(|b| *b == 0)
        .filter(|e| !e.is_empty())
        .map(|e| Ok((e[0], e[1], utf8(&e[3..])?)))
        .collect()
}

/// path -> owner-executable, for regular files only; anything else is refused.
fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, bool>) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|e| err("io", format!("{}: {e}", dir.display())))? {
        let entry = entry.map_err(|e| err("io", e.to_string()))?;
        let meta = fs::symlink_metadata(entry.path()).map_err(|e| err("io", e.to_string()))?;
        let name = utf8(entry.file_name().as_encoded_bytes())?;
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if meta.is_dir() {
            walk(&entry.path(), &rel, out)?;
        } else if meta.is_file() {
            out.insert(rel, meta.permissions().mode() & 0o100 != 0);
        } else {
            return Err(err(
                "export_mismatch",
                format!("export has a non-regular entry {rel:?}"),
            ));
        }
    }
    Ok(())
}

/// A configured clean/smudge/process filter would run arbitrary commands during
/// status/add/archive. `git config --get-regexp` exits 1 for "no match"; anything else odd fails.
fn reject_filters(cwd: &Path) -> Result<()> {
    let o = run(cwd, None, &["config", "--get-regexp", r"^filter\."], None)?;
    match o.status.code() {
        Some(1) => Ok(()),
        Some(0) => Err(err("git_filters", "repository configures filter drivers")),
        _ => Err(err("git", format!("git config: {}", brief(&o.stderr)))),
    }
}

/// Raw (`--no-filters`) blob ids of `paths` under `cwd`, in order.
fn raw_hashes(cwd: &Path, paths: &[&str]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    // `--stdin-paths` is newline-framed and C-unquotes a leading `"`.
    if paths.iter().any(|p| p.contains('\n') || p.starts_with('"')) {
        return Err(err("git_path", "path cannot be hashed safely"));
    }
    let input = paths.iter().map(|p| format!("{p}\n")).collect::<String>();
    let out = ok(
        cwd,
        None,
        &["hash-object", "--no-filters", "--stdin-paths"],
        Some(input.into_bytes()),
    )?;
    let hashes: Vec<String> = text(&out).lines().map(str::to_string).collect();
    if hashes.len() != paths.len() {
        return Err(err("git", "hash-object returned the wrong number of ids"));
    }
    Ok(hashes)
}

/// Canonical work-tree root containing `cwd`, read-only. Needs no clean or attached state; a
/// linked worktree is its own root. `None` for no repository, a bare repo or `.git` itself.
pub fn toplevel(cwd: &Path) -> Option<PathBuf> {
    let out = run(cwd, None, &["rev-parse", "--show-toplevel"], None).ok()?;
    let root = out.status.success().then(|| text(&out.stdout))?;
    fs::canonicalize(root).ok()
}

impl Repo {
    /// Validate an ordinary attached branch with a clean, fully-visible baseline.
    pub fn open(path: &Path) -> Result<Self> {
        let probe = text(&ok(
            path,
            None,
            &["rev-parse", "--show-toplevel", "--absolute-git-dir"],
            None,
        )?);
        let mut lines = probe.lines();
        let (root, git_dir) = match (lines.next(), lines.next()) {
            (Some(r), Some(g)) => (PathBuf::from(r), PathBuf::from(g)),
            _ => return Err(err("git", "unexpected rev-parse output")),
        };
        let canon =
            |p: &Path| fs::canonicalize(p).map_err(|e| err("io", format!("{}: {e}", p.display())));
        if canon(path)? != canon(&root)? {
            return Err(err(
                "git_not_toplevel",
                format!("{} is not the worktree root", path.display()),
            ));
        }
        reject_filters(&root)?;
        let ref_name = run(&root, None, &["symbolic-ref", "-q", "HEAD"], None)?;
        let ref_name = text(&ref_name.stdout);
        if !ref_name.starts_with("refs/heads/") {
            return Err(err("git_detached", "HEAD is not attached to a branch"));
        }
        let head = text(&ok(
            &root,
            None,
            &["rev-parse", "--verify", "-q", "HEAD^{commit}"],
            None,
        )?);
        for state in [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_LOG",
            "rebase-merge",
            "rebase-apply",
            "sequencer",
        ] {
            if git_dir.join(state).exists() {
                return Err(err("git_busy", format!("operation in progress: {state}")));
            }
        }
        if let Some((x, y, p)) = status(&root, None)?.first() {
            return Err(err(
                "git_dirty",
                format!("baseline is not clean: {}{} {p}", *x as char, *y as char),
            ));
        }
        // assume-unchanged / skip-worktree / sparse entries hide changes from status.
        let tags = ok(&root, None, &["ls-files", "-v", "-z"], None)?;
        if tags
            .split(|b| *b == 0)
            .any(|e| !e.is_empty() && !e.starts_with(b"H "))
        {
            return Err(err(
                "git_hidden",
                "index has assume-unchanged or skip-worktree entries",
            ));
        }
        Ok(Self {
            root,
            git_dir,
            ref_name,
            head,
        })
    }

    fn git(&self, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>> {
        ok(&self.root, index, args, None)
    }

    fn verify_attached(&self) -> Result<()> {
        let r = self
            .git(None, &["symbolic-ref", "-q", "HEAD"])
            .map(|o| text(&o));
        let h = self
            .git(None, &["rev-parse", "--verify", "-q", "HEAD^{commit}"])
            .map(|o| text(&o));
        if r.as_deref().ok() != Some(&self.ref_name) || h.as_deref().ok() != Some(&self.head) {
            return Err(err(
                "stale_head",
                "branch or HEAD moved since the baseline was opened",
            ));
        }
        Ok(())
    }

    /// Snapshot baseline + changes under `scopes` into a fresh `scratch/snap-*/export`, via a
    /// temporary index beside it. `scratch` must exist and lie outside the worktree. Creates
    /// no commit and never writes the real index; leftovers from failures stay in `scratch`.
    pub fn capture(&self, scopes: &[&str], scratch: &Path) -> Result<Snapshot> {
        if scopes.is_empty() {
            return Err(err("scope", "no scopes given"));
        }
        scopes.iter().try_for_each(|s| check_scope(s))?;
        let scratch = fs::canonicalize(scratch)
            .map_err(|e| err("scratch", format!("{}: {e}", scratch.display())))?;
        let canon_root = fs::canonicalize(&self.root).map_err(|e| err("io", e.to_string()))?;
        if scratch.starts_with(&canon_root) {
            return Err(err("scratch", "scratch must be outside the worktree"));
        }
        reject_filters(&self.root)?;
        let private = fs::metadata(&scratch).map_err(|e| err("scratch", e.to_string()))?;
        if private.permissions().mode() & 0o077 != 0 {
            return Err(err("scratch", "scratch must not be group/world accessible"));
        }
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let work = scratch.join(format!(
            "snap-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        DirBuilder::new()
            .mode(0o700)
            .create(&work)
            .map_err(|e| err("scratch", format!("{}: {e}", work.display())))?;
        let (index, export) = (work.join("index"), work.join("export"));
        self.verify_attached()?;

        let ix = Some(index.as_path());
        self.git(ix, &["read-tree", &self.head])?;
        let mut add = vec!["add", "-A", "--"];
        add.extend(scopes);
        self.git(ix, &add)?;

        let (mut changed, mut live, mut outside) = (Vec::new(), Vec::new(), Vec::new());
        for (x, y, p) in status(&self.root, ix)? {
            if y != b' ' {
                outside.push(p)
            } else if x != b' ' {
                if x != b'D' {
                    live.push(p.clone());
                }
                changed.push(p)
            }
        }
        if !outside.is_empty() {
            return Err(err(
                "git_dirty",
                format!("changes outside the scopes: {}", outside.join(", ")),
            ));
        }
        if changed.is_empty() {
            return Err(err("no_changes", "scopes contain no changes"));
        }
        let mut staged = BTreeMap::new();
        for entry in self
            .git(ix, &["ls-files", "-s", "-z"])?
            .split(|b| *b == 0)
            .filter(|e| !e.is_empty())
        {
            let entry = utf8(entry)?;
            let (meta, path) = entry.split_once('\t').unwrap_or((&entry, ""));
            if !(meta.starts_with("100644 ") || meta.starts_with("100755 "))
                || !meta.ends_with(" 0")
            {
                return Err(err(
                    "git_unsafe",
                    format!("gitlink, symlink or unmerged entry: {entry:?}"),
                ));
            }
            staged.insert(
                path.to_string(),
                meta.split(' ').nth(1).unwrap_or("").to_string(),
            );
        }
        // `add` applies attribute conversions (text/eol/ident/working-tree-encoding); the
        // candidate's own bytes must be exactly the staged blobs.
        let live: Vec<&str> = live.iter().map(String::as_str).collect();
        for (p, h) in live.iter().zip(raw_hashes(&self.root, &live)?) {
            if staged.get(*p) != Some(&h) {
                return Err(err(
                    "git_conversion",
                    format!(
                        "worktree bytes differ from the staged blob (attribute conversion?): {p}"
                    ),
                ));
            }
        }
        let tree = text(&self.git(ix, &["write-tree"])?);
        self.export(&tree, &export)?;
        Ok(Snapshot {
            ref_name: self.ref_name.clone(),
            head: self.head.clone(),
            tree,
            changed,
            export,
        })
    }

    /// `git archive` the tree into `dir`, then prove the result is byte-for-byte the tree
    /// (catches export-ignore, export-subst, eol/ident conversion, extra or missing files).
    fn export(&self, tree: &str, dir: &Path) -> Result<()> {
        fs::create_dir(dir).map_err(|e| err("scratch", format!("{}: {e}", dir.display())))?;
        let mut archive = git_command(&self.root, None, &["archive", "--format=tar", tree]);
        let mut a = archive
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| err("git", e.to_string()))?;
        let a_err = drain(a.stderr.take().expect("piped stderr"));
        let mut tar = command("tar")
            .args([
                "-x",
                "-f",
                "-",
                "--no-same-owner",
                "--no-same-permissions",
                "-C",
            ])
            .arg(dir)
            .stdin(Stdio::from(a.stdout.take().expect("piped stdout")))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| err("export", format!("spawn tar: {e}")))?;
        let t_err = drain(tar.stderr.take().expect("piped stderr"));
        let tar_status = tar.wait().map_err(|e| err("export", e.to_string()))?;
        let status = a.wait().map_err(|e| err("git", e.to_string()))?;
        let (a_err, t_err) = (
            a_err.join().unwrap_or_default(),
            t_err.join().unwrap_or_default(),
        );
        if !status.success() || !tar_status.success() {
            return Err(err(
                "export",
                format!("archive failed: {} {}", brief(&a_err), brief(&t_err)),
            ));
        }

        self.verify_export_tree(tree, dir)
    }

    fn verify_export_tree(&self, tree: &str, dir: &Path) -> Result<()> {
        let mut want = BTreeMap::new();
        for e in self
            .git(None, &["ls-tree", "-r", "-z", "--full-tree", tree])?
            .split(|b| *b == 0)
            .filter(|e| !e.is_empty())
        {
            let e = utf8(e)?;
            let (meta, path) = e
                .split_once('\t')
                .ok_or_else(|| err("git", "bad ls-tree output"))?;
            let f: Vec<&str> = meta.split(' ').collect();
            want.insert(path.to_string(), (f[0] == "100755", f[2].to_string()));
        }
        let mut have = BTreeMap::new();
        walk(dir, "", &mut have)?;
        let same = want.len() == have.len()
            && want
                .iter()
                .zip(&have)
                .all(|((a, (x, _)), (b, y))| a == b && x == y);
        if !same {
            return Err(err(
                "export_mismatch",
                "export files or modes differ from the tree",
            ));
        }
        if want.is_empty() {
            return Ok(());
        }
        let paths: Vec<&str> = want.keys().map(String::as_str).collect();
        if !raw_hashes(dir, &paths)?
            .iter()
            .eq(want.values().map(|(_, oid)| oid))
        {
            return Err(err(
                "export_mismatch",
                "export content differs from the tree",
            ));
        }
        Ok(())
    }

    /// Re-check a snapshot export after the trusted gate has run.
    pub fn verify_snapshot_export(&self, snap: &Snapshot) -> Result<()> {
        self.check_snap(snap)?;
        self.verify_export_tree(&snap.tree, &snap.export)
    }

    /// The snapshot is this repo's own (fields are public, so they can be forged).
    fn check_snap(&self, snap: &Snapshot) -> Result<()> {
        if snap.ref_name != self.ref_name
            || snap.head != self.head
            || !is_oid(&snap.head)
            || !is_oid(&snap.tree)
        {
            return Err(err("git", "snapshot does not belong to this repository"));
        }
        Ok(())
    }

    /// `commit` is a lowercase object id of a single-parent commit: `snap.tree` on `snap.head`.
    fn check_commit(&self, snap: &Snapshot, commit: &str) -> Result<()> {
        self.check_snap(snap)?;
        if !is_oid(commit) {
            return Err(err("git", "commit is not an object id"));
        }
        // Exactly "<commit> <head>": rejects tags, merges, roots and uppercase spellings.
        let line = text(&self.git(
            None,
            &[
                "rev-list",
                "--parents",
                "-n1",
                &format!("{commit}^{{commit}}"),
            ],
        )?);
        let tree = text(&self.git(
            None,
            &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
        )?);
        if line != format!("{commit} {}", snap.head) || tree != snap.tree {
            return Err(err("git", "commit is not snap.tree on snap.head"));
        }
        Ok(())
    }

    /// Create (but do not publish) a commit of `snap.tree` on `snap.head`.
    /// Call only after the gate for `snap.export` was verified externally.
    pub fn commit_tree(&self, snap: &Snapshot, message: &str) -> Result<String> {
        self.check_snap(snap)?;
        let out = ok(
            &self.root,
            None,
            &["commit-tree", &snap.tree, "-p", &snap.head, "-F", "-"],
            Some(message.as_bytes().to_vec()),
        )?;
        Ok(text(&out))
    }

    /// Compare-and-swap `snap.ref_name` from `snap.head` to `commit`, which must be a direct
    /// single-parent child of `snap.head` with tree `snap.tree`. Leaves the worktree and real
    /// index alone (see `sync_real_index`).
    pub fn advance_ref(&self, snap: &Snapshot, commit: &str) -> Result<()> {
        self.check_commit(snap, commit)?;
        self.verify_attached()?;
        self.git(
            None,
            &[
                "update-ref",
                "-m",
                "lmloop: advance",
                &snap.ref_name,
                commit,
                &snap.head,
            ],
        )?;
        Ok(())
    }

    /// After `advance_ref`, point the real index at the new commit; see `sync_real_index`.
    pub fn sync_index(&self, snap: &Snapshot, commit: &str) -> Result<()> {
        self.check_commit(snap, commit)?;
        sync_real_index(&self.root, &snap.ref_name, &snap.head, &snap.tree, commit)
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }
}

/// Does the index (`None` = the real one) equal `tree`? `diff-index --cached --quiet`: 0 same, 1 differs.
fn index_is(root: &Path, index: Option<&Path>, tree: &str) -> Result<bool> {
    let o = run(
        root,
        index,
        &["diff-index", "--cached", "--quiet", tree],
        None,
    )?;
    match o.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(err("git", format!("git diff-index: {}", brief(&o.stderr)))),
    }
}

/// Removes our private index directory, and our own `index.lock` while it is unpublished.
struct IndexCleanup {
    dir: PathBuf,
    lock: Option<PathBuf>,
}

impl Drop for IndexCleanup {
    fn drop(&mut self) {
        if let Some(l) = &self.lock {
            let _ = fs::remove_file(l);
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The one guarded real-index write, for the normal path and crash recovery alike. `commit` must
/// be `tree` on `head` with the branch `ref_name` attached and at `commit`, and the real index
/// must hold exactly `head`'s or `tree`'s tree, so staged work is never overwritten. Idempotent.
///
/// The new index is built by Git in a private directory under the per-worktree git dir (split
/// index and sparse index off, no hooks), then published under Git's own protocol: create
/// `index.lock` exclusively, re-validate everything while holding it, write and fsync the new
/// bytes into the lock, rename it onto `index`, fsync the directory. An existing `index.lock` is
/// refused (`index_locked`) and left alone; a stale one is never cleared. Dropped silently by the
/// rebuild: a split index (content is kept, as a plain index) and cache extensions (untracked
/// cache, resolve-undo). Skip-worktree/assume-unchanged entries are refused (`git_hidden`).
pub fn sync_real_index(
    root: &Path,
    ref_name: &str,
    head: &str,
    tree: &str,
    commit: &str,
) -> Result<()> {
    if ![head, tree, commit].into_iter().all(is_oid) {
        return Err(err("git", "index sync needs object ids"));
    }
    let git_dir = PathBuf::from(text(&ok(
        root,
        None,
        &["rev-parse", "--absolute-git-dir"],
        None,
    )?));
    if !git_dir.is_absolute() {
        return Err(err("git", "git dir is not absolute"));
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = git_dir.join(format!(
        "lmloop-index-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(io_err(&dir))?;
    let mut cleanup = IndexCleanup { dir, lock: None };
    let new = cleanup.dir.join("index");
    ok(
        root,
        Some(&new),
        &[
            "-c",
            "core.splitIndex=false",
            "-c",
            "index.sparse=false",
            "read-tree",
            commit,
        ],
        None,
    )?;
    if !index_is(root, Some(&new), tree)? {
        return Err(err("git", "prepared index does not hold the new tree"));
    }

    let (lock_path, index_path) = (git_dir.join("index.lock"), git_dir.join("index"));
    let mut lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                err(
                    "index_locked",
                    format!(
                        "{} exists: another git process is running or it is stale; left in place",
                        lock_path.display()
                    ),
                )
            } else {
                io_err(&lock_path)(e)
            }
        })?;
    cleanup.lock = Some(lock_path.clone());

    // Everything below is re-checked under the lock; nothing before it is trusted.
    let one = |args: &[&str]| ok(root, None, args, None).map(|o| text(&o));
    if one(&["symbolic-ref", "-q", "HEAD"]).ok().as_deref() != Some(ref_name)
        || one(&["rev-parse", "--verify", "-q", "HEAD^{commit}"])
            .ok()
            .as_deref()
            != Some(commit)
    {
        return Err(err("stale_head", "branch is not at the committed snapshot"));
    }
    // Exactly "<commit> <head>": rejects tags, merges, roots and uppercase spellings.
    let parents = one(&[
        "rev-list",
        "--parents",
        "-n1",
        &format!("{commit}^{{commit}}"),
    ])?;
    let its_tree = one(&["rev-parse", "--verify", &format!("{commit}^{{tree}}")])?;
    if parents != format!("{commit} {head}") || its_tree != tree {
        return Err(err("git", "commit is not the tree on the head"));
    }
    if index_is(root, None, tree)? {
        return Ok(());
    }
    if !index_is(root, None, head)? {
        return Err(err(
            "git_dirty",
            "real index holds neither the old nor the new tree; staged work left untouched",
        ));
    }
    if one(&["ls-files", "-v", "-z"])?
        .split('\0')
        .any(|e| !e.is_empty() && !e.starts_with("H "))
    {
        return Err(err(
            "git_hidden",
            "index has assume-unchanged or skip-worktree entries; left untouched",
        ));
    }

    let bytes = fs::read(&new).map_err(io_err(&new))?;
    if let Ok(m) = fs::metadata(&index_path) {
        lock.set_permissions(m.permissions())
            .map_err(io_err(&lock_path))?;
    }
    lock.write_all(&bytes)
        .and_then(|()| lock.sync_all())
        .map_err(io_err(&lock_path))?;
    fs::rename(&lock_path, &index_path).map_err(io_err(&index_path))?;
    cleanup.lock = None;
    File::open(&git_dir)
        .and_then(|d| d.sync_all())
        .map_err(io_err(&git_dir))
}
