//! CB-0A end-to-end checks for forensic, strictly read-only status.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use git2::{Oid, Repository as GitRepository, Signature};
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .output()
        .expect("run atomic")
}

fn init_atomic(root: &Path, home: &Path) {
    let output = atomic(root, home, &["init", "--view", "main"]);
    assert!(
        output.status.success(),
        "atomic init failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_git_with_commit(root: &Path) -> Oid {
    let repository = GitRepository::init(root).expect("init Git");
    repository
        .set_head("refs/heads/main")
        .expect("set Git main");
    fs::write(root.join("tracked.txt"), b"tracked\n").expect("write tracked file");
    let mut index = repository.index().expect("Git index");
    index
        .add_path(Path::new("tracked.txt"))
        .expect("stage tracked file");
    index.write().expect("write Git index");
    let tree_oid = index.write_tree().expect("write Git tree");
    let tree = repository.find_tree(tree_oid).expect("find Git tree");
    let signature = Signature::now("Atomic Test", "atomic@example.com").expect("signature");
    repository
        .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
        .expect("initial Git commit")
}

#[derive(Debug, Eq, PartialEq)]
enum SnapshotValue {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
    Other,
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, SnapshotValue> {
    fn collect(root: &Path, current: &Path, values: &mut BTreeMap<PathBuf, SnapshotValue>) {
        let mut entries = fs::read_dir(current)
            .expect("read snapshot directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("read snapshot entries");
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("relative path")
                .to_path_buf();
            let file_type = entry.file_type().expect("file type");
            if file_type.is_symlink() {
                values.insert(
                    relative,
                    SnapshotValue::Symlink(fs::read_link(&path).expect("read symlink")),
                );
            } else if file_type.is_dir() {
                values.insert(relative, SnapshotValue::Directory);
                collect(root, &path, values);
            } else if file_type.is_file() {
                values.insert(
                    relative,
                    SnapshotValue::File(fs::read(&path).expect("read file")),
                );
            } else {
                values.insert(relative, SnapshotValue::Other);
            }
        }
    }

    let mut values = BTreeMap::new();
    collect(root, root, &mut values);
    values
}

#[test]
fn no_reconcile_reports_forensics_without_mutating_git_or_atomic() {
    let repository = TempDir::new().expect("repository tempdir");
    let home = TempDir::new().expect("home tempdir");
    init_atomic(repository.path(), home.path());
    let head = init_git_with_commit(repository.path());
    let git_before = snapshot_tree(&repository.path().join(".git"));
    let atomic_before = snapshot_tree(&repository.path().join(".atomic"));

    let output = atomic(
        repository.path(),
        home.path(),
        &["status", "--no-reconcile"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "forensic status failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("Forensic status (read-only; reconciliation disabled)"));
    assert!(stdout.contains("HEAD: attached"));
    assert!(stdout.contains(&format!("OID: {head}")));
    assert!(stdout.contains("canonical index digest (not a Git tree OID)"));
    assert!(stdout.contains("exact index tree OID:"));
    assert!(stdout.contains("index tree availability: ComputedReadOnly"));
    assert!(stdout.contains("Checkpoint classification: Unanchored"));
    assert!(
        stdout.contains("Ordinary Atomic file status, reconciliation, and mutation were not run.")
    );
    assert_eq!(git_before, snapshot_tree(&repository.path().join(".git")));
    assert_eq!(
        atomic_before,
        snapshot_tree(&repository.path().join(".atomic"))
    );
}

#[test]
fn no_reconcile_reports_explicit_no_git_without_mutating_atomic() {
    let repository = TempDir::new().expect("repository tempdir");
    let home = TempDir::new().expect("home tempdir");
    init_atomic(repository.path(), home.path());
    let atomic_before = snapshot_tree(&repository.path().join(".atomic"));

    let output = atomic(
        repository.path(),
        home.path(),
        &["status", "--no-reconcile"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "forensic no-Git status failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("Git: not present"));
    assert!(stdout.contains("Checkpoint classification: Unanchored"));
    assert!(stdout.contains("typed reason: NoGit"));
    assert_eq!(
        atomic_before,
        snapshot_tree(&repository.path().join(".atomic"))
    );
}
