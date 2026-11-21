//! Real Git, real scratch repositories: the snapshot helper never commits
//! unless explicitly told to, never touches the real index, and refuses
//! anything it cannot prove is exactly the tree it hashed.

use lmloop::git::Repo;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let base = std::env::var_os("LMLOOP_TEST_TMP")
            .map_or_else(|| PathBuf::from("/tmp/opencode"), PathBuf::from);
        let dir = base.join(format!("lmloop-git-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        fs::create_dir_all(dir.join("scratch")).unwrap();
        fs::set_permissions(dir.join("scratch"), PermissionsExt::from_mode(0o700)).unwrap();
        let s = Self(dir);
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["config", "user.name", "t"]);
        s.git(&["config", "user.email", "t@example.com"]);
        s.write("src/a.txt", "one\n");
        s.write("other/b.txt", "two\n");
        s.git(&["add", "-A"]);
        s.git(&["commit", "-q", "-m", "base"]);
        s
    }
    fn repo(&self) -> PathBuf {
        self.0.join("repo")
    }
    fn scratch(&self) -> PathBuf {
        self.0.join("scratch")
    }
    fn write(&self, rel: &str, body: &str) {
        let p = self.repo().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    fn git(&self, args: &[&str]) -> String {
        git_in(&self.repo(), args)
    }
    fn open(&self) -> Repo {
        Repo::open(&self.repo()).unwrap()
    }
    fn index(&self) -> Vec<u8> {
        fs::read(self.repo().join(".git/index")).unwrap()
    }
    fn commits(&self) -> String {
        self.git(&["rev-list", "--all", "--count"])
    }
    /// Nothing observable changed: no commit, same ref, same real index.
    fn assert_untouched(&self, head: &str, index: &[u8]) {
        assert_eq!(self.git(&["rev-parse", "HEAD"]), head);
        assert_eq!(self.commits(), "1");
        assert_eq!(self.index(), index);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn kind<T>(r: lmloop::state::Result<T>) -> &'static str {
    match r {
        Ok(_) => panic!("expected an error"),
        Err(e) => e.kind,
    }
}

#[test]
fn captures_exact_tree_and_clean_export_then_commits_only_when_told() {
    let s = Scratch::new("happy");
    let head = s.git(&["rev-parse", "HEAD"]);
    let repo = s.open();
    s.write("src/a.txt", "changed\n");
    s.write("src/new.sh", "#!/bin/sh\n");
    fs::set_permissions(
        s.repo().join("src/new.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let snap = repo.capture(&["src"], &s.scratch()).unwrap();

    assert_eq!(snap.changed, ["src/a.txt", "src/new.sh"]);
    assert_eq!(
        fs::read_to_string(snap.export.join("src/a.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(
        fs::read_to_string(snap.export.join("other/b.txt")).unwrap(),
        "two\n"
    );
    // The exact tree an independent `git add` + `write-tree` would produce.
    s.git(&["add", "-A"]);
    assert_eq!(snap.tree, s.git(&["write-tree"]));
    // Capture itself created no commit.
    assert_eq!(s.commits(), "1");
    assert_eq!(snap.head, head);

    // `add -A` above staged things; restore the baseline index for the CAS half.
    s.git(&["read-tree", "HEAD"]);
    let commit = repo.commit_tree(&snap, "lmloop: test\n").unwrap();
    assert_eq!(
        s.git(&["rev-parse", "HEAD"]),
        head,
        "commit-tree must not move the ref"
    );
    repo.advance_ref(&snap, &commit).unwrap();
    assert_eq!(s.git(&["rev-parse", "HEAD"]), commit);
    assert_eq!(s.git(&["rev-parse", "HEAD^{tree}"]), snap.tree);
    assert_eq!(s.git(&["rev-parse", "HEAD^"]), head);
    assert_eq!(s.git(&["log", "-1", "--format=%s"]), "lmloop: test");
}

#[test]
fn rejects_dirty_baseline_detached_head_and_subdirectory() {
    let s = Scratch::new("baseline");
    let sub = s.repo().join("src");
    assert_eq!(kind(Repo::open(&sub)), "git_not_toplevel");

    s.write("untracked.txt", "x");
    assert_eq!(kind(Repo::open(&s.repo())), "git_dirty");
    fs::remove_file(s.repo().join("untracked.txt")).unwrap();
    s.write("src/a.txt", "dirty\n");
    assert_eq!(kind(Repo::open(&s.repo())), "git_dirty");
    s.git(&["checkout", "-q", "--", "src/a.txt"]);

    s.git(&["update-index", "--assume-unchanged", "src/a.txt"]);
    assert_eq!(kind(Repo::open(&s.repo())), "git_hidden");
    s.git(&["update-index", "--no-assume-unchanged", "src/a.txt"]);

    s.git(&["checkout", "-q", "--detach"]);
    assert_eq!(kind(Repo::open(&s.repo())), "git_detached");
}

#[test]
fn failures_leave_no_commit_and_real_index_unchanged() {
    let s = Scratch::new("failures");
    let head = s.git(&["rev-parse", "HEAD"]);
    let index = s.index();
    let repo = s.open();

    // Nothing changed.
    assert_eq!(kind(repo.capture(&["src"], &s.scratch())), "no_changes");
    // Change outside the allowed scope.
    s.write("other/b.txt", "edited\n");
    s.write("src/a.txt", "edited\n");
    assert_eq!(kind(repo.capture(&["src"], &s.scratch())), "git_dirty");
    // Untracked file outside the scope.
    s.write("other/b.txt", "two\n");
    s.write("stray.txt", "x");
    assert_eq!(kind(repo.capture(&["src"], &s.scratch())), "git_dirty");
    fs::remove_file(s.repo().join("stray.txt")).unwrap();
    // Scratch must exist and sit outside the worktree.
    assert_eq!(
        kind(repo.capture(&["src"], &s.repo().join("src"))),
        "scratch"
    );
    assert_eq!(
        kind(repo.capture(&["src"], &s.0.join("missing"))),
        "scratch"
    );
    s.write("src/a.txt", "one\n");
    s.assert_untouched(&head, &index);
}

#[test]
fn scopes_are_literal_relative_paths() {
    let s = Scratch::new("scopes");
    let repo = s.open();
    s.write("src/a.txt", "edited\n");
    for bad in [
        "",
        "/src",
        "../src",
        "src/../src",
        "./src",
        "src/",
        "src//a.txt",
        ".git",
        "src/.GIT/x",
    ] {
        assert_eq!(kind(repo.capture(&[bad], &s.scratch())), "scope", "{bad:?}");
    }
    assert_eq!(kind(repo.capture(&[], &s.scratch())), "scope");
    // A glob is a literal name matching nothing; git refuses it, nothing is captured.
    assert_eq!(kind(repo.capture(&["s*"], &s.scratch())), "git");
    // A literal file scope works.
    let snap = repo.capture(&["src/a.txt"], &s.scratch()).unwrap();
    assert_eq!(snap.changed, ["src/a.txt"]);
}

#[test]
fn rejects_symlinks_and_gitlinks() {
    let s = Scratch::new("links");
    let head = s.git(&["rev-parse", "HEAD"]);
    let index = s.index();
    let repo = s.open();

    symlink("a.txt", s.repo().join("src/link")).unwrap();
    assert_eq!(kind(repo.capture(&["src"], &s.scratch())), "git_unsafe");
    fs::remove_file(s.repo().join("src/link")).unwrap();

    let nested = s.repo().join("src/nested");
    fs::create_dir_all(&nested).unwrap();
    git_in(&nested, &["init", "-q", "-b", "main"]);
    git_in(
        &nested,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@e",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "n",
        ],
    );
    assert_eq!(kind(repo.capture(&["src"], &s.scratch())), "git_unsafe");
    fs::remove_dir_all(nested).unwrap();
    s.assert_untouched(&head, &index);
}

#[test]
fn export_guards_attributes_filters_and_hidden_conversions() {
    // export-ignore would silently drop the file from the gate's view.
    let s = Scratch::new("ignore");
    s.write(".gitattributes", "src/skip.txt export-ignore\n");
    s.write("src/skip.txt", "v1\n");
    s.git(&["add", "-A"]);
    s.git(&["commit", "-q", "-m", "attrs"]);
    let repo = s.open();
    s.write("src/skip.txt", "v2\n");
    let (head, index) = (s.git(&["rev-parse", "HEAD"]), s.index());
    assert_eq!(
        kind(repo.capture(&["src"], &s.scratch())),
        "export_mismatch"
    );
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(s.index(), index);

    // export-subst never expands for a bare tree, so the literal bytes are what the gate sees.
    let s = Scratch::new("subst");
    s.write(".gitattributes", "src/a.txt export-subst\n");
    s.write("src/a.txt", "$Format:%H$\n");
    s.git(&["add", "-A"]);
    s.git(&["commit", "-q", "-m", "attrs"]);
    let repo = s.open();
    s.write("src/a.txt", "$Format:%h$ edited\n");
    let snap = repo.capture(&["src"], &s.scratch()).unwrap();
    assert_eq!(
        fs::read_to_string(snap.export.join("src/a.txt")).unwrap(),
        "$Format:%h$ edited\n"
    );

    // A clean/smudge filter driver would run commands during add/archive.
    let s = Scratch::new("filter");
    s.git(&[
        "config",
        "filter.x.clean",
        "touch /tmp/opencode/lmloop-filter-ran",
    ]);
    assert_eq!(kind(Repo::open(&s.repo())), "git_filters");
    // Added after open: capture refuses too, before any add/archive could run it.
    let s2 = Scratch::new("filter2");
    let repo = s2.open();
    s2.git(&[
        "config",
        "filter.x.clean",
        "touch /tmp/opencode/lmloop-filter-ran",
    ]);
    s2.write(".gitattributes", "*.txt filter=x\n");
    s2.write("src/a.txt", "edited\n");
    assert_eq!(kind(repo.capture(&["src"], &s2.scratch())), "git_filters");
    assert!(!Path::new("/tmp/opencode/lmloop-filter-ran").exists());
}

#[test]
fn attribute_conversions_are_refused_not_gated() {
    // Without a configured filter, `* text eol=lf` makes `add` rewrite CRLF -> LF.
    let s = Scratch::new("attr-eol");
    s.write(".gitattributes", "*.txt text eol=lf\n");
    s.git(&["add", "-A"]);
    s.git(&["commit", "-q", "-m", "attrs"]);
    let repo = s.open();
    s.write("src/a.txt", "line1\r\nline2\r\n");
    assert_eq!(kind(repo.capture(&["src"], &s.scratch())), "git_conversion");
    // `ident` expands $Id$ on add as well.
    let s = Scratch::new("attr-ident");
    s.write(".gitattributes", "*.txt ident\n");
    s.git(&["add", "-A"]);
    s.git(&["commit", "-q", "-m", "attrs"]);
    let repo = s.open();
    s.write("src/a.txt", "$Id: junk$\n");
    assert_eq!(kind(repo.capture(&["src"], &s.scratch())), "git_conversion");
}

#[test]
fn scratch_must_be_private_and_work_dir_is_0700() {
    let s = Scratch::new("private");
    let repo = s.open();
    s.write("src/a.txt", "edited\n");
    fs::set_permissions(s.scratch(), PermissionsExt::from_mode(0o755)).unwrap();
    assert_eq!(kind(repo.capture(&["src"], &s.scratch())), "scratch");
    fs::set_permissions(s.scratch(), PermissionsExt::from_mode(0o700)).unwrap();
    let snap = repo.capture(&["src"], &s.scratch()).unwrap();
    let work = snap.export.parent().unwrap();
    assert_eq!(fs::metadata(work).unwrap().permissions().mode() & 0o077, 0);
}

#[test]
fn forged_snapshots_and_merge_commits_are_refused_and_index_sync_is_guarded() {
    let s = Scratch::new("forged");
    let repo = s.open();
    s.write("src/a.txt", "edited\n");
    let snap = repo.capture(&["src"], &s.scratch()).unwrap();
    let commit = repo.commit_tree(&snap, "m\n").unwrap();

    let mut forged = snap.clone();
    forged.tree = "not-a-tree".into();
    assert_eq!(kind(repo.commit_tree(&forged, "m\n")), "git");
    let mut forged = snap.clone();
    forged.ref_name = "refs/heads/other".into();
    assert_eq!(kind(repo.advance_ref(&forged, &commit)), "git");
    assert_eq!(kind(repo.advance_ref(&snap, &commit.to_uppercase())), "git");

    // A merge commit with the right tree whose first parent is head is not a plain child.
    let side = s.git(&[
        "commit-tree",
        "-p",
        &snap.head,
        "-m",
        "side",
        &format!("{}^{{tree}}", snap.head),
    ]);
    let merge = s.git(&[
        "commit-tree",
        &snap.tree,
        "-p",
        &snap.head,
        "-p",
        &side,
        "-m",
        "merge",
    ]);
    assert_eq!(kind(repo.advance_ref(&snap, &merge)), "git");
    assert_eq!(s.git(&["rev-parse", "HEAD"]), snap.head);

    // sync_index refuses before the ref moved.
    assert_eq!(kind(repo.sync_index(&snap, &commit)), "stale_head");
    repo.advance_ref(&snap, &commit).unwrap();
    // Staged unrelated work in the real index is never overwritten.
    s.write("other/b.txt", "staged\n");
    s.git(&["add", "other/b.txt"]);
    assert_eq!(kind(repo.sync_index(&snap, &commit)), "git_dirty");
    s.git(&["reset", "-q", "--", "other/b.txt"]);
    s.write("other/b.txt", "two\n");
    // Index still at the old tree: sync moves it; a second call is a no-op.
    repo.sync_index(&snap, &commit).unwrap();
    repo.sync_index(&snap, &commit).unwrap();
    assert_eq!(s.git(&["status", "--porcelain"]), "");
}

/// A committed-and-advanced snapshot whose real index still holds the old tree.
fn advanced(s: &Scratch, repo: &Repo) -> (lmloop::git::Snapshot, String) {
    s.write("src/a.txt", "edited\n");
    let snap = repo.capture(&["src"], &s.scratch()).unwrap();
    let commit = repo.commit_tree(&snap, "m\n").unwrap();
    repo.advance_ref(&snap, &commit).unwrap();
    (snap, commit)
}

#[test]
fn sync_index_refuses_a_stale_lock_leaves_it_and_succeeds_on_retry() {
    let s = Scratch::new("lock");
    let repo = s.open();
    let (snap, commit) = advanced(&s, &repo);
    let lock = s.repo().join(".git/index.lock");
    fs::write(&lock, "someone else\n").unwrap();
    let before = s.index();

    assert_eq!(kind(repo.sync_index(&snap, &commit)), "index_locked");
    // Never cleared, never written through; the real index is untouched.
    assert_eq!(fs::read(&lock).unwrap(), b"someone else\n");
    assert_eq!(s.index(), before);
    // The private index directory was removed.
    let leftovers = |p: &str| {
        fs::read_dir(s.repo().join(p))
            .unwrap()
            .filter(|e| {
                let n = e.as_ref().unwrap().file_name();
                n.to_string_lossy().starts_with("lmloop-index-")
            })
            .count()
    };
    assert_eq!(leftovers(".git"), 0);

    // The operator clears the stale lock; the retry publishes and leaves no lock behind.
    fs::remove_file(&lock).unwrap();
    repo.sync_index(&snap, &commit).unwrap();
    assert!(!lock.exists());
    assert_eq!(leftovers(".git"), 0);
    assert_eq!(s.git(&["status", "--porcelain"]), "");
    assert_eq!(s.git(&["write-tree"]), snap.tree);
}

#[test]
fn sync_index_failures_remove_only_their_own_lock() {
    let s = Scratch::new("ownlock");
    let repo = s.open();
    let (snap, commit) = advanced(&s, &repo);
    // Staged unrelated work: refused after the lock was taken, so our lock must be gone again.
    s.write("other/b.txt", "staged\n");
    s.git(&["add", "other/b.txt"]);
    let before = s.index();
    assert_eq!(kind(repo.sync_index(&snap, &commit)), "git_dirty");
    assert!(!s.repo().join(".git/index.lock").exists());
    assert_eq!(s.index(), before, "staged index preserved byte for byte");

    // Skip-worktree entries on the old tree are refused rather than silently dropped.
    s.git(&["reset", "-q", "--", "other/b.txt"]);
    s.write("other/b.txt", "two\n");
    s.git(&["update-index", "--skip-worktree", "other/b.txt"]);
    let before = s.index();
    assert_eq!(kind(repo.sync_index(&snap, &commit)), "git_hidden");
    assert!(!s.repo().join(".git/index.lock").exists());
    assert_eq!(s.index(), before);
}

#[test]
fn sync_index_uses_the_linked_worktrees_own_index_and_lock() {
    let s = Scratch::new("linked");
    let linked = s.0.join("linked");
    s.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "side",
        linked.to_str().unwrap(),
    ]);
    let main_index = s.index();
    let repo = Repo::open(&linked).unwrap();
    assert!(repo.git_dir().ends_with(".git/worktrees/linked"));

    // A lock in the common dir does not belong to this worktree's index.
    let common_lock = s.repo().join(".git/index.lock");
    fs::write(&common_lock, "main worktree busy\n").unwrap();
    fs::write(linked.join("src/a.txt"), "edited\n").unwrap();
    let snap = repo.capture(&["src"], &s.scratch()).unwrap();
    let commit = repo.commit_tree(&snap, "m\n").unwrap();
    repo.advance_ref(&snap, &commit).unwrap();

    // Its own lock does.
    let own_lock = repo.git_dir().join("index.lock");
    fs::write(&own_lock, "x").unwrap();
    assert_eq!(kind(repo.sync_index(&snap, &commit)), "index_locked");
    fs::remove_file(&own_lock).unwrap();

    repo.sync_index(&snap, &commit).unwrap();
    assert_eq!(git_in(&linked, &["status", "--porcelain"]), "");
    assert_eq!(git_in(&linked, &["write-tree"]), snap.tree);
    assert_eq!(fs::read(&common_lock).unwrap(), b"main worktree busy\n");
    fs::remove_file(&common_lock).unwrap();
    assert_eq!(
        s.index(),
        main_index,
        "the main worktree's index is untouched"
    );
    assert_eq!(s.git(&["rev-parse", "HEAD"]), snap.head);
}

#[test]
fn eol_conversion_config_cannot_change_the_tree() {
    let s = Scratch::new("eol");
    s.git(&["config", "core.autocrlf", "true"]);
    let repo = s.open();
    s.write("src/a.txt", "line1\r\nline2\r\n");
    let snap = repo.capture(&["src"], &s.scratch()).unwrap();
    // Raw bytes were hashed: CRLF preserved, export identical.
    assert_eq!(
        fs::read(snap.export.join("src/a.txt")).unwrap(),
        b"line1\r\nline2\r\n"
    );
}

#[test]
fn hooks_never_run_and_ref_cas_refuses_a_moved_branch() {
    let s = Scratch::new("cas");
    let marker = s.0.join("hook-ran");
    let hooks = s.0.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    for name in [
        "pre-commit",
        "commit-msg",
        "post-commit",
        "reference-transaction",
        "pre-auto-gc",
        "post-index-change",
    ] {
        let p = hooks.join(name);
        fs::write(&p, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    s.git(&["config", "core.hooksPath", hooks.to_str().unwrap()]);
    fs::copy(
        hooks.join("pre-commit"),
        s.repo().join(".git/hooks/reference-transaction"),
    )
    .unwrap();
    let head = s.git(&["rev-parse", "HEAD"]);

    let repo = s.open();
    s.write("src/a.txt", "edited\n");
    let snap = repo.capture(&["src"], &s.scratch()).unwrap();
    let commit = repo.commit_tree(&snap, "m\n").unwrap();

    // Someone else advances the branch before we publish: CAS must refuse.
    s.git(&[
        "-c",
        "core.hooksPath=/dev/null",
        "checkout",
        "-q",
        "--",
        "src/a.txt",
    ]);
    s.git(&[
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@e",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "other",
    ]);
    let moved = s.git(&["rev-parse", "HEAD"]);
    assert_eq!(kind(repo.advance_ref(&snap, &commit)), "stale_head");
    assert_eq!(s.git(&["rev-parse", "HEAD"]), moved);
    assert_ne!(moved, head);

    // A commit that is not snap.tree on snap.head is refused even on a fresh branch.
    let fresh = s.git(&["rev-parse", "HEAD"]);
    assert_eq!(kind(repo.advance_ref(&snap, &fresh)), "git");
    assert_eq!(kind(repo.advance_ref(&snap, "not-an-oid")), "git");
    assert!(!marker.exists(), "a hook ran");
}

#[test]
fn successful_publish_runs_no_hooks() {
    let s = Scratch::new("nohook");
    let marker = s.0.join("hook-ran");
    let hooks = s.0.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let p = hooks.join("reference-transaction");
    fs::write(&p, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.git(&["config", "core.hooksPath", hooks.to_str().unwrap()]);
    let repo = s.open();
    s.write("src/a.txt", "edited\n");
    let snap = repo.capture(&["src"], &s.scratch()).unwrap();
    let commit = repo.commit_tree(&snap, "m\n").unwrap();
    repo.advance_ref(&snap, &commit).unwrap();
    assert_eq!(s.git(&["rev-parse", "HEAD"]), commit);
    assert!(!marker.exists(), "a hook ran");
}
