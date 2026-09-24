use std::process::Command;

use atomic_core::change::ChangeHeader;
use atomic_repository::changestore::ChangeStore;
use atomic_repository::{RecordOptions, Repository, TrackingOptions};

#[test]
fn doctor_check_is_read_only_and_native_repair_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    std::fs::write(temp.path().join("f.txt"), b"content\n").unwrap();
    repo.add(working_copy, "f.txt", TrackingOptions::default())
        .unwrap();
    repo.record(
        working_copy,
        ChangeHeader::new("base"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    drop(repo);

    let pristine_path = temp.path().join(".atomic/pristine.redb");
    let pristine_len = std::fs::metadata(&pristine_path).unwrap().len();
    let before = Repository::open_readonly_for_native_repair(temp.path())
        .unwrap()
        .verify_native_derived_indexes()
        .unwrap();
    let worktree_before = std::fs::read(temp.path().join("f.txt")).unwrap();

    let check = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .current_dir(temp.path())
        .args(["--no-color", "doctor", "check"])
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "doctor check failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(
        String::from_utf8_lossy(&check.stdout).contains("Native derived indexes are consistent")
    );
    let after = Repository::open_readonly_for_native_repair(temp.path())
        .unwrap()
        .verify_native_derived_indexes()
        .unwrap();
    assert_eq!(after.expected_rows, before.expected_rows);
    assert_eq!(after.actual_rows, before.actual_rows);
    assert_eq!(after.problems, before.problems);
    assert_eq!(
        std::fs::metadata(&pristine_path).unwrap().len(),
        pristine_len
    );
    assert_eq!(
        std::fs::read(temp.path().join("f.txt")).unwrap(),
        worktree_before
    );

    let repair = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .current_dir(temp.path())
        .args(["--no-color", "doctor", "repair-native-indexes"])
        .output()
        .unwrap();
    assert!(
        repair.status.success(),
        "doctor repair failed: {}",
        String::from_utf8_lossy(&repair.stderr)
    );
    assert!(String::from_utf8_lossy(&repair.stdout).contains("already consistent"));
    assert_eq!(
        std::fs::read(temp.path().join("f.txt")).unwrap(),
        worktree_before
    );
}

#[test]
fn doctor_check_does_not_recreate_a_missing_change_store() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    std::fs::write(temp.path().join("f.txt"), b"content\n").unwrap();
    repo.add(working_copy, "f.txt", TrackingOptions::default())
        .unwrap();
    repo.record(
        working_copy,
        ChangeHeader::new("base"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    drop(repo);

    let changes = temp.path().join(".atomic/changes");
    let preserved = temp.path().join(".atomic/changes-preserved");
    std::fs::rename(&changes, &preserved).unwrap();
    let pristine = temp.path().join(".atomic/pristine.redb");
    let pristine_before = std::fs::read(&pristine).unwrap();

    for args in [
        ["--no-color", "doctor", "check"],
        ["--no-color", "doctor", "repair-native-indexes"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
            .current_dir(temp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(rendered.to_ascii_lowercase().contains("unrepairable"));
        assert!(!changes.exists(), "native doctor recreated .atomic/changes");
        assert!(preserved.is_dir());
        assert_eq!(
            std::fs::read(&pristine).unwrap().len(),
            pristine_before.len(),
            "doctor wrote to pristine.redb (length changed); byte-for-byte equality is
             intentionally not required because redb 4.2 rewrites its
             graceful-shutdown marker on every clean close"
        );
    }
}

#[test]
fn doctor_rejects_corrupt_current_view_without_repairing_dev() {
    let temp = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(temp.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    std::fs::write(temp.path().join("base.txt"), b"base\n").unwrap();
    repo.add(working_copy, "base.txt", TrackingOptions::default())
        .unwrap();
    repo.record(
        working_copy,
        ChangeHeader::new("base"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view(working_copy, "feature").unwrap();
    std::fs::write(temp.path().join("feature.txt"), b"feature\n").unwrap();
    repo.add(working_copy, "feature.txt", TrackingOptions::default())
        .unwrap();
    repo.record(
        working_copy,
        ChangeHeader::new("feature"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    drop(repo);

    let current_view = temp.path().join(".atomic/current_view");
    let invalid = [0xff, 0xfe, 0xfd];
    std::fs::write(&current_view, invalid).unwrap();
    let pristine = temp.path().join(".atomic/pristine.redb");
    let pristine_before = std::fs::read(&pristine).unwrap();
    let feature_before = std::fs::read(temp.path().join("feature.txt")).unwrap();

    for args in [
        ["--no-color", "doctor", "check"],
        ["--no-color", "doctor", "repair-native-indexes"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
            .current_dir(temp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(rendered.to_ascii_lowercase().contains("unrepairable"));
        assert_eq!(
            std::fs::read(&pristine).unwrap().len(),
            pristine_before.len(),
            "doctor wrote to pristine.redb (length changed); byte-for-byte equality is
             intentionally not required because redb 4.2 rewrites its
             graceful-shutdown marker on every clean close"
        );
        assert_eq!(std::fs::read(&current_view).unwrap(), invalid);
        assert_eq!(
            std::fs::read(temp.path().join("feature.txt")).unwrap(),
            feature_before
        );
    }
}

#[test]
fn doctor_reports_missing_change_authority_as_unrepairable_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    std::fs::write(temp.path().join("f.txt"), b"content\n").unwrap();
    repo.add(working_copy, "f.txt", TrackingOptions::default())
        .unwrap();
    let outcome = repo
        .record(
            working_copy,
            ChangeHeader::new("base"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    let store = ChangeStore::from_root(temp.path(), 1).unwrap();
    let change_path = store.change_path(outcome.hash());
    drop(store);
    drop(repo);

    let preserved_change = change_path.with_extension("change-missing");
    std::fs::rename(&change_path, &preserved_change).unwrap();
    let pristine_path = temp.path().join(".atomic/pristine.redb");
    let pristine_before = std::fs::read(&pristine_path).unwrap();
    let worktree_before = std::fs::read(temp.path().join("f.txt")).unwrap();

    for args in [
        ["--no-color", "doctor", "check"],
        ["--no-color", "doctor", "repair-native-indexes"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
            .current_dir(temp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            rendered.to_ascii_lowercase().contains("unrepairable"),
            "output did not explain unrepairable authority: {rendered}"
        );
        assert_eq!(
            std::fs::read(&pristine_path).unwrap().len(),
            pristine_before.len(),
            "doctor wrote to pristine.redb (length changed); byte-for-byte equality is
             intentionally not required because redb 4.2 rewrites its
             graceful-shutdown marker on every clean close"
        );
        assert_eq!(
            std::fs::read(temp.path().join("f.txt")).unwrap(),
            worktree_before
        );
        assert!(!change_path.exists());
        assert!(preserved_change.is_file());
    }
}
