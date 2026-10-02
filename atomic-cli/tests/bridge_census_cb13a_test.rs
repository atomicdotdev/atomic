//! CB-13A R2: the deterministic diagnostic census. `git bridge verify`
//! enumerates the durable classes a recovery would need to face — missing
//! changes, invalid bindings, stale/broken working-copy records, orphaned
//! WIP refs, divergent ref mappings — READ-ONLY: injected corruption
//! produces named per-class results and byte-for-byte identical `.atomic`
//! AND `.git` state before/after.

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
        .env("GIT_AUTHOR_NAME", "CB-13A Tests")
        .env("GIT_AUTHOR_EMAIL", "cb13a@example.com")
        .env("GIT_COMMITTER_NAME", "CB-13A Tests")
        .env("GIT_COMMITTER_EMAIL", "cb13a@example.com")
        .output()
        .expect("run atomic")
}

fn atomic_ok(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for base in [root.join(".atomic"), root.join(".git")] {
        if let Ok(stack) = walkdir(&base) {
            out.extend(stack);
        }
    }
    out.sort();
    out
}

fn walkdir(dir: &Path) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    if dir.is_file() {
        let relative = dir.to_string_lossy().into_owned();
        out.push((relative, fs::read(dir).unwrap_or_default()));
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            out.extend(walkdir(&path)?);
        } else {
            let relative = path.to_string_lossy().into_owned();
            out.push((relative, fs::read(&path).unwrap_or_default()));
        }
    }
    Ok(out)
}

fn bytes_differ(before: &[(String, Vec<u8>)], after: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut differences = Vec::new();
    for (path, bytes) in before {
        match after.iter().find(|(other, _)| other == path) {
            None => differences.push(format!("{path} removed")),
            Some((_, other)) if other != bytes => differences.push(format!("{path} changed")),
            Some(_) => {}
        }
    }
    for (path, _) in after {
        if !before.iter().any(|(other, _)| other == path) {
            differences.push(format!("{path} added"));
        }
    }
    differences
}

#[test]
fn census_reports_classes_read_only_and_clean_state_passes() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    let git = || Command::new("git");
    git()
        .args(["init", "-q", "-b", "main"])
        .current_dir(&root)
        .output()
        .unwrap();
    fs::write(root.join("seed.txt"), b"seed\n").unwrap();
    let commit = || {
        let out = git()
            .args(["add", "-A"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success());
        let out = git()
            .args(["commit", "-q", "-m", "base"])
            .current_dir(&root)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success());
    };
    commit();
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // A clean repository's census is all-green and named.
    let verify = atomic_ok(&root, &home, &["git", "bridge", "verify"]);
    for class in [
        "census/changes",
        "census/bindings",
        "census/working-copies",
        "census/wip",
        "census/refs",
    ] {
        assert!(
            verify.contains(&format!("layer {class}: ")),
            "{class} in: {verify}"
        );
        let line = verify
            .lines()
            .find(|line| line.contains(&format!("layer {class}: ")))
            .expect("class line");
        assert!(
            line.starts_with("✓"),
            "clean state must pass the {class} census: {line}"
        );
    }

    // READ-ONLY under corruption: corrupt one change file (flip a byte in
    // the change store copy); the census NAMES the failing class while the
    // byte-for-byte snapshot proves no repair ran.
    let changes_dir = root.join(".atomic").join("changes");
    // Find one change file and flip a byte inside it.
    let mut change_file: Option<std::path::PathBuf> = None;
    for entry in walkdir(&changes_dir).unwrap() {
        let (path, bytes) = entry;
        if path.ends_with(".change") && !bytes.is_empty() {
            let path = std::path::PathBuf::from(path);
            let mut bytes = bytes;
            let last = bytes.len() - 1;
            bytes[last] ^= 0xff;
            fs::write(&path, &bytes).unwrap();
            change_file = Some(path);
            break;
        }
    }
    assert!(change_file.is_some(), "a change file exists to corrupt");
    drop(change_file);

    // Snapshot AFTER the corruption edit: any verify-side write is a
    // difference; the corruption edit itself is not.
    let before = snapshot(&root);
    let corrupted = atomic(&root, &home, &["git", "bridge", "verify"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&corrupted.stdout),
        String::from_utf8_lossy(&corrupted.stderr)
    );
    assert!(
        !corrupted.status.success(),
        "a corrupt change must fail the census: {text}"
    );
    assert!(
        text.contains("census/changes") && text.contains("FAIL to load"),
        "the census names the change-coverage failure: {text}"
    );
    // The remaining census classes still RAN and reported (no generic
    // dirty-state short circuit).
    assert!(
        text.contains("census/bindings"),
        "the binding census still ran alongside the failure: {text}"
    );

    // Byte-for-byte: the read-only diagnostic mutated NOTHING.
    let after = snapshot(&root);
    let differences = bytes_differ(&before, &after);
    // The verify command itself must not rewrite anything; the corruption
    // edit is the only difference (the corrupt file we wrote).
    assert!(
        differences.is_empty(),
        "the census must be read-only; changed: {differences:?}"
    );
}

/// CB-13A follow-up R2: an incomplete operation head in ANOTHER working
/// copy's scope is enumerated by the census — the inspection path can no
/// longer pass a head the writable path would gate on.
#[test]
fn census_reports_incomplete_heads_in_other_working_copy_scopes() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    let git = || Command::new("git");
    git()
        .args(["init", "-q", "-b", "main"])
        .current_dir(&root)
        .output()
        .unwrap();
    fs::write(root.join("seed.txt"), b"seed\n").unwrap();
    let commit = || {
        git()
            .args(["add", "-A"])
            .current_dir(&root)
            .output()
            .unwrap();
        let out = git()
            .args(["commit", "-q", "-m", "base"])
            .current_dir(&root)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success());
    };
    commit();
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // Forge a SECOND working-copy record whose scope holds an incomplete
    // (unverified) operation head: the census must enumerate it even though
    // the inspecting working copy (the repository scope and our own scope)
    // is clean — the inspection path can no longer pass a head the writable
    // path would gate on.
    {
        use atomic_core::pristine::{MutTxnT, OperationMutTxnT, ViewTxnT, WorkingCopyRecord};
        use atomic_core::types::WorkingCopyId;
        let repo = atomic_repository::Repository::open(&root).expect("writable open");
        let mut txn = repo.pristine().write_txn().expect("write txn");
        // The second working copy desires the same view 'main'.
        let main_view = txn.get_view("main").unwrap().expect("main exists");
        let second = WorkingCopyId::from_bytes([0xa7; 16]);
        let record = WorkingCopyRecord {
            id: second,
            location_fingerprint: atomic_core::types::Hash::of(b"second-wc"),
            desired_view: main_view.id,
            desired_state: main_view.state,
            materialized_state: None,
            materialized_manifest: None,
        };
        atomic_core::pristine::WorkingCopyMutTxnT::put_working_copy(&mut txn, &record).unwrap();

        // An incomplete operation head in the second scope: an ANCHOR
        // operation (anchors may root a chain with no parents) followed by
        // an unverified child whose head has no verified receipt — the
        // interrupted-remediation shape the writable path would gate on.
        use atomic_core::operation::{
            ActorRef, Operation, OperationKind, OperationPayload, OperationScope, RepoStateDelta,
            RepoStateRef,
        };
        let anchor = Operation::new(OperationPayload {
            parents: Vec::new(),
            kind: OperationKind::Anchor,
            relation: None,
            working_copy: Some(second),
            before: RepoStateRef::EMPTY,
            delta: RepoStateDelta {
                after: RepoStateRef::EMPTY,
                metadata: Vec::new(),
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence: Vec::new(),
            actor: ActorRef::System {
                name: "census-fixture-second-scope".to_string(),
            },
            timestamp_ms: 0,
            lossy: Vec::new(),
        })
        .expect("anchor builds");
        let unverified = Operation::new(OperationPayload {
            parents: vec![anchor.id()],
            kind: OperationKind::Repair,
            relation: None,
            working_copy: Some(second),
            before: RepoStateRef::EMPTY,
            delta: RepoStateDelta {
                after: RepoStateRef::EMPTY,
                metadata: Vec::new(),
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence: Vec::new(),
            actor: ActorRef::System {
                name: "census-fixture-second-scope".to_string(),
            },
            timestamp_ms: 1000,
            lossy: Vec::new(),
        })
        .expect("unverified op builds");
        txn.put_operation(&anchor).unwrap();
        txn.put_operation(&unverified).unwrap();
        txn.compare_and_set_operation_heads(
            OperationScope::WorkingCopy(second),
            &[],
            &[unverified.id()],
        )
        .unwrap();
        txn.commit().unwrap();
    }
    let before = snapshot(&root);
    let verify = atomic(&root, &home, &["git", "bridge", "verify"]);
    let after = snapshot(&root);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(
        text.contains("census/working-copies"),
        "the working-copy census class ran: {text}"
    );
    // The OTHER scope's incomplete head must be reported by the census
    // operations layer — the inspection path cannot pass it silently.
    let ops_line = text
        .lines()
        .find(|line| line.contains("layer operations: "))
        .expect("operations layer line");
    assert!(
        !ops_line.starts_with('✓'),
        "the second scope's incomplete head must surface in the operations verdict: {ops_line}"
    );
    assert!(
        bytes_differ(&before, &after).is_empty(),
        "the census must remain read-only across all scopes"
    );
    // Clean single-scope state: the census classes pass.
    for class in [
        "census/changes",
        "census/bindings",
        "census/wip",
        "census/refs",
    ] {
        let line = text
            .lines()
            .find(|line| line.contains(&format!("layer {class}: ")))
            .expect("class line");
        assert!(line.starts_with("✓"), "{class} must pass: {line}");
    }
}
