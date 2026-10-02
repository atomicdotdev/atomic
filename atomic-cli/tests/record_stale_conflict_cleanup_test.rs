//! End-to-end: explicit `record --allow-conflict-markers <paths>` clears only
//! verified-stale conflict metadata on an *unanchored* colocated workspace,
//! without importing Git or fabricating a baseline.
//!
//! The fixture is a disposable Git+Atomic colocated repository that opted in
//! to the bridge (`[git.bridge] enabled = true`) and whose bridge checkpoint
//! has been removed, so the fresh CLI classifies it as `MissingCheckpoint` and
//! the ordinary content-record boundary refuses before the record body runs.
//! (Without the opt-in a colocated repository is native and never refuses for
//! a missing anchor — see `git_bridge_opt_in_test.rs`.) The narrow
//! metadata-only route must still:
//!
//! 1. clear a persisted `Order` row whose canonical render no longer conflicts
//!    and whose bytes equal the working tree, through the exact scoped CLI flags;
//! 2. refuse (and clear nothing) when any named path has a content delta, is
//!    untracked, or the scope is mixed;
//! 3. refuse while Git owns an operation, an index lock, or a conflicted index;
//! 4. leave the three approved source files, Git objects/index/refs, view
//!    memberships, and the absent checkpoint untouched;
//! 5. write nothing on `--dry-run`.

#![cfg(not(windows))]
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

/// Legitimate marker-shaped content: the fixture carries literal marker lines
/// that must never be treated as an unresolved merge. Assembled at runtime so
/// this source file itself stays free of marker lines and recordable without
/// `--allow-conflict-markers`.
fn marker_fixture() -> String {
    let gt = ">".repeat(7);
    let eq = "=".repeat(7);
    let lt = "<".repeat(7);
    format!("Conflict marker example:\n{gt} 1\nleft side\n{eq} 1\nright side\n{lt} 1\n")
}

struct Fixture {
    repository: TempDir,
    home: TempDir,
}

impl Fixture {
    fn empty() -> Self {
        Self {
            repository: TempDir::new().expect("repository tempdir"),
            home: TempDir::new().expect("home tempdir"),
        }
    }

    fn root(&self) -> &Path {
        self.repository.path()
    }

    /// A clean-Git colocated bridge workspace imported into Atomic with its
    /// bridge checkpoint deliberately removed (opted in, unanchored).
    fn unanchored_colocated() -> Self {
        let fixture = Self::empty();
        fixture.git_ok(&["init", "-q", "-b", "main"]);
        fixture.git_ok(&["config", "user.name", "Record Cleanup Tests"]);
        fixture.git_ok(&["config", "user.email", "cleanup@example.com"]);
        fixture.git_ok(&["config", "core.autocrlf", "false"]);
        fixture.git_ok(&["config", "commit.gpgsign", "false"]);
        fs::write(fixture.root().join("fixture.md"), marker_fixture()).unwrap();
        fs::write(fixture.root().join("other.txt"), "other\n").unwrap();
        fixture.git_ok(&["add", "fixture.md", "other.txt"]);
        fixture.git_ok(&["commit", "--no-gpg-sign", "-q", "-m", "base"]);
        fixture.atomic_ok(&["git", "import", "--no-vault"]);

        let bridge = fixture.root().join(".atomic/bridge");
        if bridge.exists() {
            fs::remove_dir_all(&bridge).expect("remove bridge checkpoint");
        }
        fixture.record_bridge_opt_in();
        assert!(
            !fixture
                .root()
                .join(".atomic/bridge/workspace.json")
                .exists(),
            "fixture must be unanchored (MissingCheckpoint)"
        );
        fixture
    }

    /// Record `[git.bridge] enabled = true` directly (what `atomic git bridge
    /// enable` records) without installing hook dispatchers, so the Git
    /// operations these tests drive stay hook-free.
    fn record_bridge_opt_in(&self) {
        let config = self.root().join(".atomic/config.toml");
        let mut text = fs::read_to_string(&config).expect("read repository config");
        assert!(
            !text.contains("[git.bridge]"),
            "fixture expects no bridge table yet"
        );
        text.push_str("\n[git.bridge]\nenabled = true\n");
        fs::write(&config, text).expect("record bridge opt-in");
    }

    fn atomic(&self, args: &[&str]) -> Output {
        let mut command = Command::new(ATOMIC_BIN);
        command
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("TERM", "dumb")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.output().expect("wait for atomic")
    }

    fn atomic_ok(&self, args: &[&str]) -> Output {
        let output = self.atomic(args);
        assert_success(&output, &format!("atomic {}", args.join(" ")));
        output
    }

    fn git(&self, args: &[&str]) -> Output {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Record Cleanup Tests")
            .env("GIT_AUTHOR_EMAIL", "cleanup@example.com")
            .env("GIT_COMMITTER_NAME", "Record Cleanup Tests")
            .env("GIT_COMMITTER_EMAIL", "cleanup@example.com")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.output().expect("run git")
    }

    fn git_ok(&self, args: &[&str]) -> Output {
        let output = self.git(args);
        assert_success(&output, &format!("git {}", args.join(" ")));
        output
    }
}

fn assert_success(output: &Output, what: &str) {
    if !output.status.success() {
        panic!(
            "{what} failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Inject one persisted `Order` conflict row directly into the repository's
/// view for `path`. Test-only: production never writes this row.
fn inject_order_conflict(root: &Path, path: &str, line: u32) {
    use atomic_core::pristine::{MutTxnT, StoredConflict, StoredConflictKind, TreeTxnT, ViewTxnT};

    let view_name = desired_view(root);
    let pristine = atomic_core::pristine::Pristine::open(root.join(".atomic/pristine.redb"))
        .expect("open pristine");
    let (view_id, inode) = {
        let txn = pristine.read_txn().expect("read txn");
        let view = txn
            .get_view(&view_name)
            .expect("get view")
            .expect("view exists");
        let inode = txn
            .get_inode(path)
            .expect("get inode")
            .unwrap_or_else(|| panic!("path '{path}' must be tracked"));
        (view.id, inode)
    };
    let mut txn = pristine.write_txn().expect("write txn");
    txn.put_conflicts(
        view_id,
        inode.get(),
        &[StoredConflict {
            kind: StoredConflictKind::Order,
            path: path.to_string(),
            line: Some(line),
            sides: Vec::new(),
        }],
    )
    .expect("put conflicts");
    txn.commit().expect("commit");
}

fn persisted_conflict_count(root: &Path, path: &str) -> usize {
    use atomic_core::pristine::{TreeTxnT, ViewTxnT};

    let view_name = desired_view(root);
    let pristine = atomic_core::pristine::Pristine::open(root.join(".atomic/pristine.redb"))
        .expect("open pristine");
    let txn = pristine.read_txn().expect("read txn");
    let view = txn
        .get_view(&view_name)
        .expect("get view")
        .expect("view exists");
    let inode = txn
        .get_inode(path)
        .expect("get inode")
        .expect("tracked inode");
    txn.get_conflicts(view.id, inode.get())
        .expect("get conflicts")
        .len()
}

fn desired_view(root: &Path) -> String {
    let repo = atomic_repository::Repository::open_readonly(root).expect("open repo");
    let working_copy = repo.require_working_copy_id().expect("working copy id");
    repo.desired_view_name(working_copy)
        .expect("desired view name")
}

fn digest(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_default()
}

#[test]
fn scoped_flag_clears_stale_metadata_on_unanchored_workspace() {
    let fixture = Fixture::unanchored_colocated();
    inject_order_conflict(fixture.root(), "fixture.md", 1);
    assert_eq!(persisted_conflict_count(fixture.root(), "fixture.md"), 1);

    let fixture_bytes = digest(&fixture.root().join("fixture.md"));
    let head_before = digest(&fixture.root().join(".git/HEAD"));
    let index_before = digest(&fixture.root().join(".git/index"));

    let output = fixture.atomic_ok(&[
        "record",
        "--allow-conflict-markers",
        "-m",
        "clear stale conflict metadata",
        "fixture.md",
    ]);
    let text = combined(&output);
    assert!(
        text.contains("Cleared stale conflict metadata"),
        "cleanup must be reported: {text}"
    );
    assert!(text.contains("fixture.md"), "report names the path: {text}");
    assert!(
        text.contains("operation:"),
        "report names the journaled operation: {text}"
    );

    assert_eq!(
        persisted_conflict_count(fixture.root(), "fixture.md"),
        0,
        "the stale row is cleared"
    );
    assert_eq!(
        digest(&fixture.root().join("fixture.md")),
        fixture_bytes,
        "the working-tree bytes are untouched"
    );
    assert_eq!(
        digest(&fixture.root().join(".git/HEAD")),
        head_before,
        "Git HEAD is untouched"
    );
    assert_eq!(
        digest(&fixture.root().join(".git/index")),
        index_before,
        "the Git index is untouched"
    );
    assert!(
        !fixture
            .root()
            .join(".atomic/bridge/workspace.json")
            .exists(),
        "no checkpoint is fabricated"
    );
}

#[test]
fn content_record_still_refuses_missing_checkpoint() {
    let fixture = Fixture::unanchored_colocated();
    // A genuine content delta: the narrow metadata-only route must not apply.
    fs::write(fixture.root().join("other.txt"), "changed content\n").unwrap();

    let output = fixture.atomic(&[
        "record",
        "--allow-conflict-markers",
        "-m",
        "should be refused",
        "other.txt",
    ]);
    assert!(
        !output.status.success(),
        "a content record must still be refused on an unanchored workspace"
    );
    assert!(
        combined(&output).contains("MissingCheckpoint"),
        "the ordinary guarded refusal is preserved: {}",
        combined(&output)
    );
    assert_eq!(
        fs::read(fixture.root().join("other.txt")).unwrap(),
        b"changed content\n",
        "the refusal writes nothing"
    );
}

#[test]
fn mixed_scope_clears_nothing() {
    let fixture = Fixture::unanchored_colocated();
    inject_order_conflict(fixture.root(), "fixture.md", 1);
    // A pending content delta alongside the stale metadata.
    fs::write(fixture.root().join("other.txt"), "changed content\n").unwrap();

    let output = fixture.atomic(&[
        "record",
        "--allow-conflict-markers",
        "-m",
        "mixed scope",
        "fixture.md",
        "other.txt",
    ]);
    assert!(
        !output.status.success(),
        "a mixed scope is not metadata-only"
    );
    assert_eq!(
        persisted_conflict_count(fixture.root(), "fixture.md"),
        1,
        "the stale row is untouched when the scope is mixed"
    );
}

#[test]
fn untracked_named_path_clears_nothing() {
    let fixture = Fixture::unanchored_colocated();
    inject_order_conflict(fixture.root(), "fixture.md", 1);
    fs::write(fixture.root().join("untracked.txt"), "new file\n").unwrap();

    let output = fixture.atomic(&[
        "record",
        "--allow-conflict-markers",
        "-m",
        "untracked scope",
        "fixture.md",
        "untracked.txt",
    ]);
    assert!(
        !output.status.success(),
        "an untracked path is not metadata-only"
    );
    assert_eq!(
        persisted_conflict_count(fixture.root(), "fixture.md"),
        1,
        "the stale row is untouched when an untracked path is named"
    );
}

#[test]
fn git_index_lock_refuses_and_clears_nothing() {
    let fixture = Fixture::unanchored_colocated();
    inject_order_conflict(fixture.root(), "fixture.md", 1);

    let lock = fixture.root().join(".git/index.lock");
    fs::write(&lock, b"").unwrap();
    let output = fixture.atomic(&[
        "record",
        "--allow-conflict-markers",
        "-m",
        "locked index",
        "fixture.md",
    ]);
    let _ = fs::remove_file(&lock);

    assert!(!output.status.success(), "a Git index lock must refuse");
    assert_eq!(
        persisted_conflict_count(fixture.root(), "fixture.md"),
        1,
        "the stale row is untouched while Git holds the index lock"
    );
}

#[test]
fn git_active_operation_refuses_and_clears_nothing() {
    let fixture = Fixture::unanchored_colocated();
    inject_order_conflict(fixture.root(), "fixture.md", 1);

    // A Git-owned sequence marker: MERGE_HEAD is enough to classify an active
    // operation, and must refuse before the cleanup.
    let merge_head = fixture.root().join(".git/MERGE_HEAD");
    fs::write(&merge_head, b"0123456789abcdef0123456789abcdef01234567\n").unwrap();
    let output = fixture.atomic(&[
        "record",
        "--allow-conflict-markers",
        "-m",
        "active merge",
        "fixture.md",
    ]);
    let _ = fs::remove_file(&merge_head);

    assert!(
        !output.status.success(),
        "an active Git operation must refuse"
    );
    assert_eq!(
        persisted_conflict_count(fixture.root(), "fixture.md"),
        1,
        "the stale row is untouched during an active Git operation"
    );
}

#[test]
fn dry_run_writes_nothing() {
    let fixture = Fixture::unanchored_colocated();
    inject_order_conflict(fixture.root(), "fixture.md", 1);
    let fixture_bytes = digest(&fixture.root().join("fixture.md"));
    let head_before = digest(&fixture.root().join(".git/HEAD"));

    let _ = fixture.atomic(&[
        "record",
        "--allow-conflict-markers",
        "--dry-run",
        "fixture.md",
    ]);

    assert_eq!(
        persisted_conflict_count(fixture.root(), "fixture.md"),
        1,
        "a dry run clears nothing"
    );
    assert_eq!(
        digest(&fixture.root().join("fixture.md")),
        fixture_bytes,
        "a dry run writes no working-tree bytes"
    );
    assert_eq!(
        digest(&fixture.root().join(".git/HEAD")),
        head_before,
        "a dry run leaves Git untouched"
    );
    assert!(
        !fixture
            .root()
            .join(".atomic/bridge/workspace.json")
            .exists(),
        "a dry run writes no checkpoint"
    );
}
