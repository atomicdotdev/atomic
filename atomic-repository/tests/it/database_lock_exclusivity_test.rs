//! Repository and provenance handles must agree on database exclusion, both
//! within one process and across processes. All probes use disposable repos.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use atomic_repository::redb_change_store::{RedbChangeStore, RedbStoreError};
use atomic_repository::{Repository, RepositoryError};

const CHILD_ROOT: &str = "ATOMIC_DATABASE_LOCK_ROOT";
const CHILD_REPORT: &str = "ATOMIC_DATABASE_LOCK_REPORT";
const CHILD_TEST: &str = "database_handles_exclude_incompatible_opens_across_processes";

/// libtest's name for the child test: its module path without the crate name.
fn child_test_name() -> String {
    match module_path!().split_once("::") {
        Some((_, module)) => format!("{module}::{CHILD_TEST}"),
        None => CHILD_TEST.to_string(),
    }
}

fn probe(root: &Path) -> String {
    let readonly = match Repository::open_readonly_wait(root, Duration::ZERO) {
        Ok(_) => "readonly-ok".to_string(),
        Err(RepositoryError::DatabaseBusy) => "readonly-busy".to_string(),
        Err(e) => format!("readonly-other: {e}"),
    };
    let readwrite = match Repository::open_existing_wait(root, Duration::ZERO) {
        Ok(_) => "readwrite-ok".to_string(),
        Err(RepositoryError::DatabaseBusy) => "readwrite-busy".to_string(),
        Err(e) => format!("readwrite-other: {e}"),
    };
    // The journal opens native redb against the same database the repository
    // opens through SnapshotBackend. Neither may bypass the other's lock.
    let journal = match RedbChangeStore::open_existing(root.join(".atomic/atomic.redb")) {
        Ok(_) => "journal-ok".to_string(),
        Err(RedbStoreError::Database(e))
            if matches!(*e, redb::DatabaseError::DatabaseAlreadyOpen) =>
        {
            "journal-busy".to_string()
        }
        Err(e) => format!("journal-other: {e}"),
    };
    format!("{readonly} {readwrite} {journal}")
}

fn child_probe(root: &Path, report_path: &Path) -> String {
    let mut child = Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", &child_test_name(), "--test-threads=1"])
        .env(CHILD_ROOT, root)
        .env(CHILD_REPORT, report_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the child probe");
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().expect("poll child").is_none() {
        if Instant::now() >= deadline {
            child.kill().expect("stop hung probe");
            child.wait().expect("reap hung probe");
            panic!("database open blocked instead of returning a contention error");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("collect child output");
    assert!(
        output.status.success(),
        "child failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    std::fs::read_to_string(report_path).expect("child wrote a report")
}

fn check_exclusion(cross_process: bool) {
    for holder in ["repository", "journal", "reader"] {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("repo");
        drop(Repository::init(&root).expect("init"));
        let path = root.join(".atomic/atomic.redb");
        // Keep the chosen handle alive throughout every refused-open probe.
        let held: Box<dyn std::any::Any> = match holder {
            "repository" => Box::new(Repository::open_existing(&root).unwrap()),
            "journal" => Box::new(RedbChangeStore::open_existing(&path).unwrap()),
            "reader" => Box::new(redb::ReadOnlyDatabase::open(&path).unwrap()),
            _ => unreachable!(),
        };
        let expected = if holder == "reader" {
            "readonly-ok readwrite-busy journal-busy"
        } else {
            "readonly-busy readwrite-busy journal-busy"
        };
        // A failed open must not release the live holder's lock. Probe again
        // after all the first probe's failed handles have been cleaned up.
        for attempt in 0..2 {
            let report = if cross_process {
                child_probe(&root, &temp.path().join(format!("report-{attempt}")))
            } else {
                probe(&root)
            };
            assert_eq!(report, expected, "holder={holder}, attempt={attempt}");
        }
        drop(held);
        let report = if cross_process {
            child_probe(&root, &temp.path().join("released"))
        } else {
            probe(&root)
        };
        assert_eq!(
            report, "readonly-ok readwrite-ok journal-ok",
            "dropping {holder} must release exclusion"
        );
    }
}

#[test]
fn database_handles_exclude_incompatible_opens_in_one_process() {
    check_exclusion(false);
}

#[test]
fn database_handles_exclude_incompatible_opens_across_processes() {
    if let Ok(root) = std::env::var(CHILD_ROOT) {
        let report = probe(Path::new(&root));
        std::fs::write(std::env::var(CHILD_REPORT).expect("CHILD_REPORT"), report)
            .expect("write child report");
        return;
    }
    check_exclusion(true);
}
