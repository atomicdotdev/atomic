// Temporary Unix-only Git bridge support to unblock Windows native CI.
// Revisit after Windows support is defined; see the RFC temporary platform policy.
#![cfg(unix)]

//! Bridge reconcile adoption of a Git branch while an unrelated Atomic view
//! holds the working copy (RFC §7.3 preserved import / git-import-design.md
//! "incremental import ... preserves the active view and working copy").
//!
//! The preserved import records TREE updates in the deferred lifecycle
//! journal instead of the ambient tables, so every foreign identity
//! resolution must go through the target view's graph closure — never the
//! ambient TREE/INODES indexes, which either do not bind the path at all or
//! bind it to another view's inode. This suite pins that contract for the
//! delta kinds a Git branch can carry:
//!
//! - deletion of a path a *previous commit of the same import* added (the
//!   deferred journal holds the anchor while TREE never saw it),
//! - deletion of a path the unrelated active view binds at the same name
//!   (TREE points at the foreign inode; the target closure decides),
//! - pure rename and rename+edit (the move source identity),
//! - the ambiguous-rename delete+add fallback (the old path's FileDel),
//! - a merge resolution that deletes a file alive in both parents.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Bridge Delete Tests")
        .env("GIT_AUTHOR_EMAIL", "bridge-delete@example.com")
        .env("GIT_COMMITTER_NAME", "Bridge Delete Tests")
        .env("GIT_COMMITTER_EMAIL", "bridge-delete@example.com")
        .output()
        .expect("run atomic")
}

fn atomic_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn atomic_ok(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\n{}",
        atomic_text(&output)
    );
    atomic_text(&output)
}

fn git(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Bridge Delete Tests")
        .env("GIT_AUTHOR_EMAIL", "bridge-delete@example.com")
        .env("GIT_COMMITTER_NAME", "Bridge Delete Tests")
        .env("GIT_COMMITTER_EMAIL", "bridge-delete@example.com")
        .output()
        .expect("run git")
}

fn git_ok(root: &Path, args: &[&str]) -> String {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git_commit(root: &Path, message: &str) -> String {
    git_ok(root, &["add", "."]);
    git_ok(root, &["commit", "-q", "-m", message]);
    git_ok(root, &["rev-parse", "HEAD"])
}

/// One colocated Git+Atomic fixture whose active Atomic view (`aged-rain`)
/// is unrelated to the checked-out Git branch `main`, with no verified
/// bridge checkpoint: `atomic git shim reconcile` must import `main`
/// through the preserved (deferred-TREE) path.
///
/// Atomic is initialized *after* the Git history exists, mirroring a real
/// colocated repository (and keeping `.atomic/` out of the Git trees, whose
/// later mutations would otherwise dirty the guard's tracked-path check).
struct Preserved {
    root: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Preserved {
    fn new(name: &str) -> Self {
        let root = tempfile::tempdir().expect("repo tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        assert!(git(root.path(), &["init", "-q", "-b", "main"])
            .status
            .success());
        let _ = name;
        Self { root, home }
    }

    /// The unrelated active Atomic view. `--no-vault` keeps the fixture
    /// minimal; the view stays empty (0 changes) and never verifies a
    /// bridge checkpoint, so reconcile takes the preserved import path.
    /// The generated `.atomicignore` is removed (the reconcile path does not
    /// record it; leaving it would surface as untracked noise — same pattern
    /// as the CB-10A fixture).
    fn init_atomic(&self) {
        atomic_ok(
            self.root(),
            self.home(),
            &["init", "--no-vault", "--view", "aged-rain"],
        );
        let _ = fs::remove_file(self.root().join(".atomicignore"));
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        let target = self.root().join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(target, bytes).expect("write file");
    }

    fn atomic(&self, args: &[&str]) -> String {
        atomic_ok(self.root(), self.home(), args)
    }

    fn atomic_output(&self, args: &[&str]) -> Output {
        atomic(self.root(), self.home(), args)
    }

    fn reconcile(&self) -> String {
        self.atomic(&["git", "shim", "reconcile"])
    }

    fn git(&self, args: &[&str]) -> String {
        git_ok(self.root(), args)
    }

    fn git_commit(&self, message: &str) -> String {
        git_commit(self.root(), message)
    }

    fn assert_git_clean(&self) {
        let status = git_ok(self.root(), &["status", "--porcelain"]);
        assert!(status.is_empty(), "git status not clean: {status}");
    }

    fn assert_atomic_clean(&self) {
        let status = self.atomic(&["status", "--short"]);
        assert!(
            status.trim().is_empty(),
            "atomic status not clean: {status}"
        );
    }

    /// Atomic cleanliness tolerating paths the preserved foreign view owns:
    /// the fresh import records `.atomicignore` into the *foreign* view, so
    /// while `main` is adopted the file sits on disk untracked by `main`.
    /// That is the preserved working-copy contract, not drift.
    fn assert_atomic_clean_allow_untracked(&self, allowed: &[&str]) {
        let status = self.atomic(&["status", "--short"]);
        let unexpected: Vec<String> = status
            .lines()
            .filter(|line| {
                let path = line.get(3..).unwrap_or_default().trim();
                !allowed.contains(&path)
            })
            .map(str::to_owned)
            .collect();
        assert!(
            unexpected.is_empty(),
            "atomic status has unexpected entries (allowed untracked: {allowed:?}): {status}"
        );
    }

    fn assert_bridge_verified(&self) {
        let output = self.atomic_output(&["git", "shim", "verify"]);
        assert!(
            output.status.success(),
            "atomic git shim verify failed:\n{}",
            atomic_text(&output)
        );
    }

    fn view_change_count(&self, view: &str) -> usize {
        let listing = self.atomic(&["view", "list"]);
        let line = listing
            .lines()
            .find(|line| line.split_whitespace().any(|token| token == view))
            .unwrap_or_else(|| panic!("view '{view}' missing from:\n{listing}"));
        let counted = line
            .split('(')
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|count| count.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("cannot parse change count from:\n{listing}"));
        counted
    }
}

/// (device, inode, mtime_nanos) of a path — adoption must never rewrite the
/// Git-rendered working copy (RFC Learning M11/M12).
#[cfg(unix)]
fn file_identity(root: &Path, path: &str) -> (u64, u64, i128) {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(root.join(path)).expect("stat adopted file");
    (
        metadata.dev(),
        metadata.ino(),
        i128::from(metadata.mtime()) * 1_000_000_000_000 + i128::from(metadata.mtime_nsec()),
    )
}

fn assert_file_bytes(root: &Path, path: &str, expected: &[u8]) {
    let actual = fs::read(root.join(path)).unwrap_or_else(|error| {
        panic!(
            "read '{path}' after reconcile: {error} (expected {:?})",
            expected
        )
    });
    assert_eq!(actual, expected, "'{path}' bytes after reconcile");
}

fn assert_path_absent(root: &Path, path: &str) {
    assert!(
        !root.join(path).exists(),
        "'{path}' must not exist after the imported deletion"
    );
}

// ── deletion of a path a previous import commit added ─────────────────────

#[test]
fn reconcile_imports_deletion_of_prior_import_commit_under_preservation() {
    let fixture = Preserved::new("delete-prior-add");
    fixture.write("README.md", b"readme\n");
    fixture.write("skills/codebase-context/SKILL.md", b"skill body\n");
    fixture.write("skills/other/KEEP.md", b"keep me\n");
    fixture.git_commit("add skills including nested codebase-context");

    fixture.git(&["rm", "-q", "skills/codebase-context/SKILL.md"]);
    fixture.git_commit("delete the nested skill, keep the other");
    fixture.init_atomic();

    // Working-copy preservation: adoption is metadata-only, so the
    // Git-rendered surviving files must not be rewritten.
    let readme_before = file_identity(fixture.root(), "README.md");
    let keep_before = file_identity(fixture.root(), "skills/other/KEEP.md");

    let reconcile = fixture.reconcile();
    assert!(
        !reconcile.contains("no inode binding"),
        "deletion resolved through the ambient TREE instead of the target \
         closure:\n{reconcile}"
    );

    assert_eq!(fixture.view_change_count("main"), 2);
    assert_path_absent(fixture.root(), "skills/codebase-context/SKILL.md");
    assert_file_bytes(fixture.root(), "README.md", b"readme\n");
    assert_file_bytes(fixture.root(), "skills/other/KEEP.md", b"keep me\n");
    fixture.assert_git_clean();
    fixture.assert_atomic_clean();
    fixture.assert_bridge_verified();

    #[cfg(unix)]
    {
        assert_eq!(
            file_identity(fixture.root(), "README.md"),
            readme_before,
            "reconcile rewrote an adopted Git-rendered file"
        );
        assert_eq!(
            file_identity(fixture.root(), "skills/other/KEEP.md"),
            keep_before,
            "reconcile rewrote an adopted Git-rendered file"
        );
    }

    // Idempotent second reconcile: no new changes, no Git movement.
    let head = fixture.git(&["rev-parse", "HEAD"]);
    fixture.reconcile();
    assert_eq!(fixture.view_change_count("main"), 2);
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
}

// ── deletion of a path the unrelated active view binds at the same name ───

#[test]
fn reconcile_delete_binds_target_closure_not_ambient_tree_binding() {
    let fixture = Preserved::new("same-name-foreign-view");
    // main: add then delete the nested path; foreign: holds a different file
    // at the same name. The ambient TREE therefore binds the path to the
    // foreign view's inode while the target closure must decide the identity.
    fixture.write("README.md", b"readme\n");
    fixture.git_commit("base");
    let base = fixture.git(&["rev-parse", "HEAD"]);

    fixture.git(&["switch", "-q", "-c", "foreign"]);
    fixture.write("skills/nested/SKILL.md", b"foreign body\n");
    fixture.git_commit("foreign owns the same path");
    let foreign_tip = fixture.git(&["rev-parse", "HEAD"]);

    fixture.git(&["switch", "-q", "main"]);
    fixture.write("skills/nested/SKILL.md", b"git body\n");
    fixture.git_commit("git adds the same path");
    fixture.git(&["rm", "-q", "skills/nested/SKILL.md"]);
    fixture.git_commit("git deletes the same path");

    // First import the checked-out foreign branch so the unrelated active
    // view owns the working copy and the ambient TREE binding. The fresh
    // import also records the generated `.atomicignore` as its own change.
    fixture.git(&["switch", "-q", "foreign"]);
    fixture.atomic(&["git", "import", "--no-vault"]);
    assert_eq!(fixture.view_change_count("foreign"), 3);

    // Raw Git switch to main: no verified checkpoint for this state, the
    // active view stays foreign, and reconcile must import main under
    // preservation.
    fixture.git(&["switch", "-q", "main"]);
    let reconcile = fixture.reconcile();
    assert!(
        !reconcile.contains("no stable inode binding"),
        "deletion resolved through the ambient TREE instead of the target \
         closure:\n{reconcile}"
    );

    assert_eq!(fixture.view_change_count("main"), 3);
    assert_path_absent(fixture.root(), "skills/nested/SKILL.md");
    fixture.assert_git_clean();
    // `.atomicignore` was recorded by the foreign view's fresh import and
    // is preserved on disk while `main` owns the working copy.
    fixture.assert_atomic_clean_allow_untracked(&[".atomicignore"]);
    fixture.assert_bridge_verified();
    // The unrelated view's own history is untouched.
    assert_eq!(fixture.view_change_count("foreign"), 3);

    // Switching back to the foreign branch re-adopts it and proves the
    // foreign file survived main's import and deletion.
    fixture.git(&["switch", "-q", "foreign"]);
    fixture.reconcile();
    assert_file_bytes(fixture.root(), "skills/nested/SKILL.md", b"foreign body\n");
    fixture.assert_git_clean();
    fixture.assert_atomic_clean();
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), foreign_tip);
    let _ = base;
}

// ── pure rename and rename+edit under preservation ─────────────────────────

#[test]
fn reconcile_imports_pure_rename_under_preservation() {
    let fixture = Preserved::new("pure-rename");
    fixture.write("README.md", b"readme\n");
    fixture.write("a.txt", b"move me\n");
    fixture.git_commit("add a");

    fixture.git(&["mv", "a.txt", "b.txt"]);
    fixture.git_commit("rename a to b");
    fixture.init_atomic();

    let reconcile = fixture.reconcile();
    assert!(
        !reconcile.contains("no inode binding"),
        "move source resolved through the ambient TREE instead of the target \
         closure:\n{reconcile}"
    );

    assert_eq!(fixture.view_change_count("main"), 2);
    assert_path_absent(fixture.root(), "a.txt");
    assert_file_bytes(fixture.root(), "b.txt", b"move me\n");
    fixture.assert_git_clean();
    fixture.assert_atomic_clean();
    fixture.assert_bridge_verified();
}

#[test]
fn reconcile_imports_rename_with_edit_under_preservation() {
    let fixture = Preserved::new("rename-edit");
    fixture.write("README.md", b"readme\n");
    fixture.write("a.txt", b"line1\nline2\n");
    fixture.git_commit("add a");

    fixture.git(&["mv", "a.txt", "b.txt"]);
    fixture.write("b.txt", b"line1\nline2\nline3\n");
    fixture.git_commit("rename a to b and extend");
    fixture.init_atomic();

    let reconcile = fixture.reconcile();
    assert!(
        !reconcile.contains("no inode binding"),
        "move source resolved through the ambient TREE instead of the target \
         closure:\n{reconcile}"
    );

    assert_eq!(fixture.view_change_count("main"), 2);
    assert_path_absent(fixture.root(), "a.txt");
    assert_file_bytes(fixture.root(), "b.txt", b"line1\nline2\nline3\n");
    fixture.assert_git_clean();
    fixture.assert_atomic_clean();
    fixture.assert_bridge_verified();
}

// ── ambiguous rename fallback (delete old + add new) under preservation ────

#[test]
fn reconcile_imports_ambiguous_rename_fallback_under_preservation() {
    let fixture = Preserved::new("ambiguous-rename");
    fixture.write("README.md", b"readme\n");
    fixture.write("x.txt", b"same bytes\n");
    fixture.write("y.txt", b"same bytes\n");
    fixture.git_commit("add identical x and y");

    // Two byte-identical deletions and one byte-identical addition: the
    // greedy Git pairing carries no causal information, so the import
    // downgrades the rename to canonical delete+add (RenameUnresolved).
    fixture.git(&["rm", "-q", "x.txt", "y.txt"]);
    fixture.write("z.txt", b"same bytes\n");
    fixture.git_commit("ambiguous swap");
    fixture.init_atomic();

    let reconcile = fixture.reconcile();
    assert!(
        !reconcile.contains("no inode binding"),
        "fallback deletion resolved through the ambient TREE instead of the \
         target closure:\n{reconcile}"
    );

    assert_eq!(fixture.view_change_count("main"), 2);
    assert_path_absent(fixture.root(), "x.txt");
    assert_path_absent(fixture.root(), "y.txt");
    assert_file_bytes(fixture.root(), "z.txt", b"same bytes\n");
    fixture.assert_git_clean();
    fixture.assert_atomic_clean();
    fixture.assert_bridge_verified();
}

// ── merge resolution deleting a file alive in both parents ─────────────────

#[test]
fn reconcile_imports_merge_resolution_deletion_under_preservation() {
    let fixture = Preserved::new("merge-delete");
    fixture.write("README.md", b"readme\n");
    fixture.write("dir_a/file.txt", b"base\n");
    fixture.git_commit("base with shared file");
    let base = fixture.git(&["rev-parse", "HEAD"]);

    fixture.git(&["switch", "-q", "-c", "side"]);
    fixture.write("dir_a/file.txt", b"side edit\n");
    fixture.git_commit("side edits the shared file");
    let side_tip = fixture.git(&["rev-parse", "HEAD"]);

    fixture.git(&["switch", "-q", "main"]);
    fixture.write("README.md", b"readme edited on main\n");
    fixture.git_commit("main edits readme");

    // Merge the side branch and resolve the shared file as deleted: both
    // parents hold the file, the merge tree must not. `git rm -f` because
    // the in-progress merge already staged the side's content.
    let merge = git(fixture.root(), &["merge", "--no-ff", "--no-commit", "side"]);
    assert!(
        merge.status.success(),
        "fixture merge failed: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    fixture.git(&["rm", "-q", "-f", "dir_a/file.txt"]);
    fixture.git_commit("merge side and delete the shared file");
    fixture.init_atomic();

    let reconcile = fixture.reconcile();
    assert!(
        !reconcile.contains("no stable inode binding"),
        "merge deletion resolved through the ambient TREE instead of the \
         union closure:\n{reconcile}"
    );

    // base + main edit + merge resolution = 3 view members at this revision:
    // the second-parent leg is folded through the resolution rather than a
    // direct view member (pre-existing merge-import behavior, unchanged by
    // this fix; the CB-9B parent-closure suite tracks it separately).
    assert_eq!(fixture.view_change_count("main"), 3);
    assert_path_absent(fixture.root(), "dir_a/file.txt");
    assert_file_bytes(fixture.root(), "README.md", b"readme edited on main\n");
    fixture.assert_git_clean();
    fixture.assert_atomic_clean();
    fixture.assert_bridge_verified();
    let _ = (base, side_tip);
}
