//! Is the pristine's file lock exclusive across processes, or only within one?
//!
//! The database owner re-opens the repository on every request and retries while
//! it is busy, which works either way. Whether it may instead hold one handle for
//! its lifetime depends on the answer: a cross-process lock would stop local
//! `atomic status` / `record` from opening the repository at all while an owner
//! is running.
//!
//! One test binary, two roles. The parent holds a `Repository` open and re-runs
//! itself as the child; the child tries to open the same repository and reports
//! what happened.

use std::process::Command;

use atomic_repository::{Repository, RepositoryError};

const CHILD_ROOT: &str = "ATOMIC_DATABASE_LOCK_ROOT";
const CHILD_REPORT: &str = "ATOMIC_DATABASE_LOCK_REPORT";
const CHILD_TEST: &str = "a_second_process_can_open_a_repository_another_one_holds";

/// What the child managed to do, one line, on stdout.
fn probe(root: &std::path::Path) -> String {
    // Read-only first: the gentlest thing a local command does.
    let readonly = match Repository::open_readonly(root) {
        Ok(_) => "readonly-ok".to_string(),
        Err(RepositoryError::DatabaseBusy) => "readonly-busy".to_string(),
        Err(e) => format!("readonly-other: {e}"),
    };
    // Then read-write, which is what `record` needs.
    let readwrite = match Repository::open_existing(root) {
        Ok(_) => "readwrite-ok".to_string(),
        Err(RepositoryError::DatabaseBusy) => "readwrite-busy".to_string(),
        Err(e) => format!("readwrite-other: {e}"),
    };
    format!("{readonly} {readwrite}")
}

#[test]
fn a_second_process_can_open_a_repository_another_one_holds() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("repo");

    if let Ok(child_root) = std::env::var(CHILD_ROOT) {
        // The child: report on the repository the parent named and get out. The
        // report goes to a file, not stdout — libtest's capture decides whether
        // a child's `println!` reaches its parent, and this must not depend on
        // whether the parent was run with `--nocapture`.
        let report = probe(std::path::Path::new(&child_root));
        let path = std::env::var(CHILD_REPORT).expect("CHILD_REPORT");
        std::fs::write(path, report).expect("write the child report");
        return;
    }

    // Held for the whole child run: this is the thing being tested.
    let held = Repository::init(&root).expect("init");
    assert_eq!(held.current_view(), "dev", "the parent's handle is open");

    let report_path = temp.path().join("child-report.txt");
    let output = Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", CHILD_TEST, "--test-threads=1"])
        .env(CHILD_ROOT, &root)
        .env(CHILD_REPORT, &report_path)
        .output()
        .expect("run the child probe");

    let report = std::fs::read_to_string(&report_path).unwrap_or_else(|e| {
        panic!(
            "the child reported nothing ({e})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });

    println!("while one process holds the repository, a second sees: {report}");

    // The answer, asserted. Exclusive across processes, and for a read-only
    // open as much as a read-write one — so the database owner cannot hold the
    // pristine for its lifetime without locking local `atomic status` and
    // `record` out of the repository. That is why it opens per request and
    // retries, and why an idle release is required of any handle it does keep.
    assert_eq!(
        report, "readonly-busy readwrite-busy",
        "the pristine's lock is no longer exclusive across processes; the owner \
         may be able to hold it open, and its open-per-request retry is \
         redundant"
    );

    // And the corollary, which is the one that bites: dropping the parent's
    // handle must let the very next open succeed, or an idle release would
    // never actually free the file.
    drop(held);
    match Repository::open_readonly(&root) {
        Ok(_) => {}
        Err(e) => panic!("releasing the handle did not free the file: {e}"),
    }
}
