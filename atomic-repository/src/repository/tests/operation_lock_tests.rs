use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use atomic_core::pristine::OperationTxnT;
use fs2::FileExt;
use tempfile::TempDir;

use super::*;
use crate::RepositoryLockKind;

const CHILD_LOCK_PATH: &str = "ATOMIC_TEST_OPERATION_LOCK_PATH";
const CHILD_READY_PATH: &str = "ATOMIC_TEST_OPERATION_LOCK_READY";
const CHILD_RELEASE_PATH: &str = "ATOMIC_TEST_OPERATION_LOCK_RELEASE";

struct ChildLockHolder {
    child: Child,
    release_path: std::path::PathBuf,
    _signals: TempDir,
}

impl ChildLockHolder {
    fn spawn(lock_path: &Path) -> Self {
        let signals = TempDir::new().expect("create lock-test signals");
        let ready_path = signals.path().join("ready");
        let release_path = signals.path().join("release");
        let child = Command::new(std::env::current_exe().expect("resolve test executable"))
            .arg("operation_lock_child_holds_file")
            .arg("--nocapture")
            .env(CHILD_LOCK_PATH, lock_path)
            .env(CHILD_READY_PATH, &ready_path)
            .env(CHILD_RELEASE_PATH, &release_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cross-process lock holder");
        let mut holder = Self {
            child,
            release_path,
            _signals: signals,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if ready_path.is_file() {
                return holder;
            }
            if let Some(status) = holder.child.try_wait().expect("poll lock holder") {
                panic!("lock-holder child exited before ready: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for lock-holder child"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ChildLockHolder {
    fn drop(&mut self) {
        let _ = fs::write(&self.release_path, b"release\n");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    assert!(status.success(), "lock-holder child failed: {status}");
                    return;
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("timed out waiting for lock-holder child to exit");
                }
                Err(error) => panic!("failed waiting for lock-holder child: {error}"),
            }
        }
    }
}

#[test]
fn operation_lock_child_holds_file() {
    let Some(lock_path) = std::env::var_os(CHILD_LOCK_PATH) else {
        return;
    };
    let ready_path = std::env::var_os(CHILD_READY_PATH).expect("child ready path");
    let release_path = std::env::var_os(CHILD_RELEASE_PATH).expect("child release path");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .expect("open child lock file");
    file.try_lock_exclusive().expect("acquire child lock");
    fs::write(ready_path, b"ready\n").expect("publish child ready marker");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !Path::new(&release_path).is_file() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for parent release marker"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn common_operation_lock_is_cross_process_and_independent_same_thread_exclusive() {
    let (_temp_dir, repo) = create_temp_repo();
    let common_path = repo.common_operation_lock_path();
    assert_eq!(common_path, repo.dot_dir().join("bridge.lock"));

    let holder = ChildLockHolder::spawn(&common_path);
    let error = match repo.try_lock_common_operation() {
        Ok(_) => panic!("common lock unexpectedly acquired while child holds it"),
        Err(error) => error,
    };
    assert!(error.is_lock_contended());
    assert!(matches!(
        error,
        RepositoryError::LockContended {
            lock: RepositoryLockKind::Common,
            path,
        } if path == common_path
    ));
    drop(holder);

    let first = repo
        .try_lock_common_operation()
        .expect("common lock should be available after child exits");
    assert!(matches!(
        repo.try_lock_common_operation(),
        Err(RepositoryError::LockContended { .. })
    ));
    drop(first);
    assert!(repo.try_lock_common_operation().is_ok());
}

#[test]
fn working_copy_operation_lock_is_cross_process_and_independent_same_thread_exclusive() {
    let (_temp_dir, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = repo.working_copy_operation_lock_path(working_copy);
    assert_eq!(
        path,
        repo.dot_dir()
            .join("working-copies")
            .join(working_copy.to_string())
            .join("operation.lock")
    );
    fs::create_dir_all(path.parent().expect("working-copy lock parent"))
        .expect("create working-copy lock parent");

    let holder = ChildLockHolder::spawn(&path);
    let common = repo
        .try_lock_common_operation()
        .expect("common lock should be available");
    let error = match common.try_lock_working_copy(working_copy) {
        Ok(_) => panic!("working-copy lock unexpectedly acquired while child holds it"),
        Err(error) => error,
    };
    assert!(error.is_lock_contended());
    assert!(matches!(
        error,
        RepositoryError::LockContended {
            lock: RepositoryLockKind::WorkingCopy { id },
            path: contended_path,
        } if id == working_copy && contended_path == path
    ));
    drop(holder);

    let first = repo
        .try_lock_operation(working_copy)
        .expect("ordered operation locks should be available");
    assert_eq!(first.working_copy(), working_copy);
    assert!(matches!(
        repo.try_lock_operation(working_copy),
        Err(RepositoryError::LockContended { .. })
    ));
    drop(first);
    assert!(repo.try_lock_operation(working_copy).is_ok());
}

#[test]
fn final_resource_locks_are_acquired_from_the_ordered_write_stage() {
    let (_temp_dir, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let operation = repo
        .try_lock_operation(working_copy)
        .expect("acquire ordered operation locks");

    let shelf = operation
        .begin_write()
        .expect("begin ordered pristine write")
        .try_lock_shelf()
        .expect("acquire shelf lock after pristine write");
    let shelf_path = repo.working_copy_shelf_lock_path(working_copy);
    assert_eq!(
        shelf_path,
        repo.dot_dir()
            .join("working-copies")
            .join(working_copy.to_string())
            .join("shelf.lock")
    );
    assert!(shelf_path.is_file());
    drop(shelf);

    let deferred = operation
        .begin_write()
        .expect("begin second ordered pristine write")
        .try_lock_deferred_tree()
        .expect("acquire deferred-tree lock after pristine write");
    let deferred_path = repo.deferred_tree_operation_lock_path();
    assert_eq!(
        deferred_path,
        repo.dot_dir().join("deferred-tree-alignment.lock")
    );
    assert!(deferred_path.is_file());
    drop(deferred);
}

#[test]
fn final_resource_contention_is_typed_and_reacquirable() {
    let (_temp_dir, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let operation = repo
        .try_lock_operation(working_copy)
        .expect("acquire ordered operation locks");

    let shelf_path = repo.working_copy_shelf_lock_path(working_copy);
    let shelf_holder = ChildLockHolder::spawn(&shelf_path);
    let error = match operation
        .begin_write()
        .expect("begin ordered pristine write")
        .try_lock_shelf()
    {
        Ok(_) => panic!("shelf lock unexpectedly acquired while child holds it"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        RepositoryError::LockContended {
            lock: RepositoryLockKind::Shelf { id },
            path,
        } if id == working_copy && path == shelf_path
    ));
    drop(shelf_holder);
    drop(
        operation
            .begin_write()
            .expect("begin ordered pristine write after shelf contention")
            .try_lock_shelf()
            .expect("reacquire shelf lock"),
    );

    let deferred_path = repo.deferred_tree_operation_lock_path();
    let deferred_holder = ChildLockHolder::spawn(&deferred_path);
    let error = match operation
        .begin_write()
        .expect("begin ordered pristine write")
        .try_lock_deferred_tree()
    {
        Ok(_) => panic!("deferred-tree lock unexpectedly acquired while child holds it"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        RepositoryError::LockContended {
            lock: RepositoryLockKind::DeferredTree,
            path,
        } if path == deferred_path
    ));
    drop(deferred_holder);
    drop(
        operation
            .begin_write()
            .expect("begin ordered pristine write after deferred contention")
            .try_lock_deferred_tree()
            .expect("reacquire deferred-tree lock"),
    );
}

#[test]
fn same_view_switch_is_a_noop_without_a_cyclic_operation() {
    let (_temp, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let current = repo.current_view().to_string();
    repo.switch_view(&current).unwrap();
    let first_head = {
        let txn = repo.pristine().read_txn().unwrap();
        txn.get_operation_heads(atomic_core::operation::OperationScope::WorkingCopy(
            working_copy,
        ))
        .unwrap()
        .as_slice()[0]
    };
    repo.switch_view(&current).unwrap();
    let txn = repo.pristine().read_txn().unwrap();
    assert_eq!(
        txn.get_operation_heads(atomic_core::operation::OperationScope::WorkingCopy(
            working_copy,
        ))
        .unwrap()
        .as_slice(),
        &[first_head]
    );
}

#[test]
fn switch_contention_returns_before_record_or_filesystem_mutation() {
    let (temp, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let current_view = repo.current_view().to_string();
    let record_before = repo.working_copy_record(working_copy).unwrap();
    let pointer_before = fs::read(temp.path().join(".atomic/current_view")).unwrap();
    let holder = ChildLockHolder::spawn(&repo.common_operation_lock_path());

    let error = repo.switch_view(&current_view).unwrap_err();
    assert!(matches!(
        error,
        RepositoryError::LockContended {
            lock: RepositoryLockKind::Common,
            ..
        }
    ));
    assert_eq!(
        repo.working_copy_record(working_copy).unwrap(),
        record_before
    );
    assert_eq!(
        fs::read(temp.path().join(".atomic/current_view")).unwrap(),
        pointer_before
    );
    let txn = repo.pristine().read_txn().unwrap();
    assert!(txn
        .get_operation_heads(atomic_core::operation::OperationScope::WorkingCopy(
            working_copy
        ))
        .unwrap()
        .is_empty());
    drop(txn);
    drop(holder);
}

#[test]
fn linked_worktrees_share_common_lock_path_and_use_distinct_working_copy_locks() {
    let directory = TempDir::new().expect("create linked-worktree test root");
    let primary = directory.path().join("primary");
    let linked = directory.path().join("linked");
    fs::create_dir_all(&primary).expect("create primary worktree");

    run_git(&primary, &["init"]);
    run_git(
        &primary,
        &[
            "-c",
            "user.name=Atomic Test",
            "-c",
            "user.email=atomic@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );

    let primary_repo = Repository::init(&primary).expect("initialize Atomic repository");
    let primary_id = primary_repo
        .require_working_copy_id()
        .expect("primary working-copy ID");
    let primary_common_path = primary_repo.common_operation_lock_path();
    let primary_working_copy_path = primary_repo.working_copy_operation_lock_path(primary_id);
    let primary_guard = primary_repo
        .try_lock_operation(primary_id)
        .expect("create primary operation lock files");
    drop(primary_guard);
    drop(primary_repo);

    run_git(
        &primary,
        &[
            "worktree",
            "add",
            "-b",
            "linked-operation-lock-test",
            linked.to_str().expect("linked path is UTF-8"),
        ],
    );

    let linked_repo = Repository::open(&linked).expect("register linked Atomic working copy");
    let linked_id = linked_repo
        .require_working_copy_id()
        .expect("linked working-copy ID");
    assert_ne!(linked_id, primary_id);
    assert_eq!(
        linked_repo.common_operation_lock_path(),
        primary_common_path
    );
    assert_ne!(
        linked_repo.working_copy_operation_lock_path(linked_id),
        primary_working_copy_path
    );
    // repo.root() is canonical (e.g. macOS /var -> /private/var), so the
    // expected prefix must be canonicalized too.
    let primary_canonical = std::fs::canonicalize(&primary).expect("canonicalize primary worktree");
    assert!(primary_common_path.starts_with(primary_canonical.join(".atomic")));
    assert!(!linked.join(".atomic/bridge.lock").exists());

    let linked_guard = linked_repo
        .try_lock_operation(linked_id)
        .expect("linked worktree uses common operation lock");
    drop(linked_guard);
    assert!(primary_common_path.is_file());
}

fn run_git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}
