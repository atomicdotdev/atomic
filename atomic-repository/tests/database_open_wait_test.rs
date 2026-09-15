use std::time::{Duration, Instant};

use atomic_repository::{Repository, RepositoryError};

#[test]
fn database_wait_is_bounded_and_does_not_retry_non_contention_errors() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let held = Repository::init(&root).unwrap();
    let start = Instant::now();
    let result = Repository::open_existing_wait(&root, Duration::from_millis(100));
    assert!(matches!(result, Err(RepositoryError::DatabaseBusy)));
    assert!(start.elapsed() >= Duration::from_millis(100));
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(held);
    std::fs::write(root.join(".atomic/pristine.redb"), b"corrupt database").unwrap();
    let start = Instant::now();
    let result = Repository::open_existing_wait(&root, Duration::from_secs(10));
    assert!(matches!(result, Err(RepositoryError::Database(_))));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn sandbox_database_wait_uses_the_canonical_database() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let sandbox = temp.path().join("sandbox");
    let held = Repository::init(&root).unwrap();
    held.provision_sandbox(&sandbox, "dev").unwrap();
    assert_eq!(
        Repository::canonical_dot_dir(&sandbox).unwrap(),
        held.dot_dir()
    );
    assert!(matches!(
        Repository::open_readonly_wait(&sandbox, Duration::ZERO),
        Err(RepositoryError::DatabaseBusy)
    ));
    let opener = std::thread::spawn(move || {
        Repository::open_readonly_wait(sandbox, Duration::from_secs(5)).unwrap()
    });
    std::thread::sleep(Duration::from_millis(100));
    drop(held);
    let opened = opener.join().unwrap();
    assert!(opened.is_sandbox());
    assert_eq!(opened.dot_dir(), root.join(".atomic"));
}
