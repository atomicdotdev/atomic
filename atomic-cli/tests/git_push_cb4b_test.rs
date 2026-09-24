#![cfg(feature = "remote")]
//! CB-4B publication gates fail before any local or remote mutation.

use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use git2::{Repository as GitRepository, Signature};
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

fn init_fixture(root: &Path, home: &Path) {
    let git = GitRepository::init(root).expect("init Git");
    git.set_head("refs/heads/main").expect("set main HEAD");
    fs::write(root.join("tracked.txt"), b"git-only\n").expect("write Git file");
    let mut index = git.index().expect("open index");
    index
        .add_path(Path::new("tracked.txt"))
        .expect("stage Git file");
    index.write().expect("write index");
    let tree_oid = index.write_tree().expect("write tree");
    let tree = git.find_tree(tree_oid).expect("find tree");
    let signature = Signature::now("Atomic Test", "atomic@example.com").expect("signature");
    git.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
        .expect("commit Git file");
    drop(tree);
    drop(git);

    let output = atomic(root, home, &["init", "--view", "main"]);
    assert!(
        output.status.success(),
        "atomic init failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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

fn assert_cb4b_refusal(output: &Output) {
    assert!(
        !output.status.success(),
        "mismatched publication unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("CB-4B publication equivalence failed"),
        "unexpected stderr: {stderr}"
    );
    assert!(stderr.contains("No publication mutation was attempted"));
}

#[test]
fn git_push_mismatch_preserves_refs_index_odb_worktree_and_operation_heads() {
    let repository = TempDir::new().expect("repository tempdir");
    let home = TempDir::new().expect("home tempdir");
    init_fixture(repository.path(), home.path());
    let before = snapshot_tree(repository.path());

    let output = atomic(
        repository.path(),
        home.path(),
        &["git", "push", "--no-push"],
    );

    assert_cb4b_refusal(&output);
    assert_eq!(before, snapshot_tree(repository.path()));
}

#[test]
fn native_push_bridge_mismatch_preserves_all_state_and_sends_no_request() {
    let repository = TempDir::new().expect("repository tempdir");
    let home = TempDir::new().expect("home tempdir");
    init_fixture(repository.path(), home.path());

    let exclude = repository.path().join(".git/info/exclude");
    fs::create_dir_all(exclude.parent().expect("exclude parent")).expect("create info");
    fs::write(&exclude, b"/.atomic/\n/.vault/\n").expect("activate shadow marker");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let url = format!(
        "http://{}",
        listener.local_addr().expect("listener address")
    );
    let before = snapshot_tree(repository.path());

    let output = atomic(repository.path(), home.path(), &["push", &url]);

    assert_cb4b_refusal(&output);
    assert_eq!(before, snapshot_tree(repository.path()));
    assert!(
        listener.accept().is_err(),
        "publication gate allowed a remote connection (and therefore could have POSTed)"
    );
}
