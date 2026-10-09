use std::fs;
use std::path::Path;
use std::process::Command;

use crate::{ArchiveOptions, RecordOptions, StatusOptions, TrackingOptions};
use atomic_core::change::ChangeHeader;
use atomic_core::pristine::{MutTxnT, Pristine, TreeTxnT, ViewTxnT, WorkingCopyTxnT};
use atomic_core::{Hash, WorkingCopyId};
use tempfile::TempDir;

use super::*;

fn create_legacy_repository(root: &Path, identity: Option<&str>) {
    let dot_dir = root.join(DOT_DIR);
    fs::create_dir_all(dot_dir.join("changes")).unwrap();
    fs::create_dir_all(dot_dir.join(WORKSPACES_DIR)).unwrap();
    fs::write(dot_dir.join("config.toml"), "[view]\ndefault = \"dev\"\n").unwrap();
    fs::write(dot_dir.join("current_view"), "dev\n").unwrap();
    if let Some(identity) = identity {
        fs::write(dot_dir.join("working_copy_id"), identity).unwrap();
    }

    let pristine = Pristine::open(dot_dir.join("pristine.redb")).unwrap();
    let mut txn = pristine.write_txn().unwrap();
    txn.open_or_create_view("dev").unwrap();
    txn.commit().unwrap();
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&source_path, &destination_path);
        } else {
            fs::copy(source_path, destination_path).unwrap();
        }
    }
}

fn run_git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn init_writes_canonical_identity_record_and_reopens_stably() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let id = repo.require_working_copy_id().unwrap();

    let identity = fs::read_to_string(directory.path().join(".atomic/working_copy_id")).unwrap();
    assert_eq!(identity, format!("{id}\n"));
    assert_eq!(identity.trim().parse::<WorkingCopyId>().unwrap(), id);
    assert_eq!(repo.working_copy_id(), Some(id));
    // repo.root() is canonical (e.g. macOS /var -> /private/var).
    let canonical = std::fs::canonicalize(directory.path()).unwrap();
    assert_eq!(repo.working_copy_dot_dir(), canonical.join(".atomic"));
    repo.validate_working_copy(id).unwrap();

    let record = repo.working_copy_record(id).unwrap();
    let txn = repo.pristine().read_txn().unwrap();
    let view = txn.get_view("dev").unwrap().unwrap();
    assert_eq!(record.id, id);
    assert_eq!(record.desired_view, view.id);
    assert_eq!(record.desired_state, view.state);
    drop(txn);
    drop(repo);

    let reopened = Repository::open_existing(directory.path()).unwrap();
    assert_eq!(reopened.require_working_copy_id().unwrap(), id);
    assert_eq!(reopened.current_view(), "dev");
    reopened.validate_working_copy(id).unwrap();
}

#[test]
fn writable_open_migrates_missing_and_empty_legacy_identity() {
    for identity in [None, Some("\n")] {
        let directory = TempDir::new().unwrap();
        create_legacy_repository(directory.path(), identity);

        assert!(matches!(
            Repository::open_existing(directory.path()),
            Err(RepositoryError::WorkingCopyMigrationRequired { .. })
        ));
        assert!(matches!(
            Repository::open_readonly(directory.path()),
            Err(RepositoryError::WorkingCopyMigrationRequired { .. })
        ));

        let repo = Repository::open(directory.path()).unwrap();
        let id = repo.require_working_copy_id().unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join(".atomic/working_copy_id")).unwrap(),
            format!("{id}\n")
        );
        repo.validate_working_copy(id).unwrap();
        drop(repo);

        let reopened = Repository::open_existing(directory.path()).unwrap();
        assert_eq!(reopened.require_working_copy_id().unwrap(), id);
    }
}

#[test]
fn malformed_nonempty_identity_fails_closed() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    drop(repo);
    let identity_path = directory.path().join(".atomic/working_copy_id");
    fs::write(&identity_path, "definitely-not-a-ulid\n").unwrap();

    assert!(matches!(
        Repository::open(directory.path()),
        Err(RepositoryError::MalformedWorkingCopyIdentity { .. })
    ));
    assert!(matches!(
        Repository::open_readonly(directory.path()),
        Err(RepositoryError::MalformedWorkingCopyIdentity { .. })
    ));
    assert_eq!(
        fs::read_to_string(identity_path).unwrap(),
        "definitely-not-a-ulid\n"
    );
}

#[test]
fn persistent_record_ignores_and_repairs_current_view_tampering() {
    let directory = TempDir::new().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();
    let id = repo.require_working_copy_id().unwrap();
    repo.create_view_from("feature", "dev").unwrap();
    repo.set_current_view(repo.require_working_copy_id().unwrap(), "feature")
        .unwrap();

    let txn = repo.pristine().read_txn().unwrap();
    let feature = txn.get_view("feature").unwrap().unwrap();
    drop(txn);
    assert_eq!(
        repo.working_copy_record(id).unwrap().desired_view,
        feature.id
    );
    drop(repo);

    let current_view_path = directory.path().join(".atomic/current_view");
    fs::write(&current_view_path, "dev\n").unwrap();

    let existing = Repository::open_existing(directory.path()).unwrap();
    assert_eq!(existing.current_view(), "feature");
    assert_eq!(fs::read_to_string(&current_view_path).unwrap(), "dev\n");
    drop(existing);

    let reopened = Repository::open(directory.path()).unwrap();
    assert_eq!(reopened.current_view(), "feature");
    assert_eq!(fs::read_to_string(current_view_path).unwrap(), "feature\n");
    assert_eq!(
        reopened.working_copy_record(id).unwrap().desired_view,
        feature.id
    );
}

#[test]
fn copied_repository_rotates_identity_instead_of_aliasing_record() {
    let source_parent = TempDir::new().unwrap();
    let source = source_parent.path().join("source");
    fs::create_dir_all(&source).unwrap();
    let source_repo = Repository::init(&source).unwrap();
    let source_id = source_repo.require_working_copy_id().unwrap();
    drop(source_repo);

    let destination_parent = TempDir::new().unwrap();
    let destination = destination_parent.path().join("copy");
    copy_tree(&source, &destination);

    assert!(matches!(
        Repository::open_readonly(&destination),
        Err(RepositoryError::WorkingCopyMigrationRequired { .. })
    ));

    let copied_repo = Repository::open(&destination).unwrap();
    let copied_id = copied_repo.require_working_copy_id().unwrap();
    assert_ne!(copied_id, source_id);
    copied_repo.validate_working_copy(copied_id).unwrap();
    assert!(copied_repo.working_copy_record(source_id).is_ok());
    drop(copied_repo);

    let reopened = Repository::open_existing(&destination).unwrap();
    assert_eq!(reopened.require_working_copy_id().unwrap(), copied_id);
}

#[test]
fn linked_git_worktree_requires_writable_registration_and_gets_distinct_identity() {
    let directory = TempDir::new().unwrap();
    let primary = directory.path().join("primary");
    let linked = directory.path().join("linked");
    fs::create_dir_all(&primary).unwrap();

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

    let primary_repo = Repository::init(&primary).unwrap();
    let primary_id = primary_repo.require_working_copy_id().unwrap();
    drop(primary_repo);

    run_git(
        &primary,
        &[
            "worktree",
            "add",
            "-b",
            "linked-working-copy",
            linked.to_str().unwrap(),
        ],
    );
    assert!(linked.join(".git").is_file());
    assert!(!linked.join(".atomic").exists());

    assert!(matches!(
        Repository::open_readonly(&linked),
        Err(RepositoryError::WorkingCopyMigrationRequired { .. })
    ));
    assert!(!linked.join(".atomic").exists());

    let linked_repo = Repository::open(&linked).unwrap();
    let linked_id = linked_repo.require_working_copy_id().unwrap();
    assert_ne!(linked_id, primary_id);
    assert!(linked.join(".atomic/repository").is_file());
    // dot_dir is canonicalized by the repository (macOS tempdirs live
    // behind /var → /private/var); compare canonicalized on both sides.
    assert_eq!(
        std::fs::canonicalize(linked_repo.dot_dir()).unwrap(),
        std::fs::canonicalize(primary.join(".atomic")).unwrap()
    );
    assert_eq!(
        std::fs::canonicalize(linked_repo.working_copy_dot_dir()).unwrap(),
        std::fs::canonicalize(linked.join(".atomic")).unwrap()
    );
    linked_repo.validate_working_copy(linked_id).unwrap();

    let txn = linked_repo.pristine().read_txn().unwrap();
    let records = txn.list_working_copies().unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().any(|record| record.id == primary_id));
    assert!(records.iter().any(|record| record.id == linked_id));
}

#[test]
fn working_copy_boundary_rejects_an_id_from_another_repository_before_mutation() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let first_repo = Repository::init(first.path()).unwrap();
    let second_repo = Repository::init(second.path()).unwrap();
    let wrong_id = second_repo.require_working_copy_id().unwrap();
    fs::write(first.path().join("sentinel.txt"), b"unchanged\n").unwrap();

    assert!(matches!(
        first_repo.status(wrong_id, StatusOptions::default()),
        Err(RepositoryError::WorkingCopyIdentityMismatch { .. })
    ));
    assert!(matches!(
        first_repo.add(wrong_id, "sentinel.txt", TrackingOptions::default()),
        Err(RepositoryError::WorkingCopyIdentityMismatch { .. })
    ));
    assert!(!first_repo.is_tracked("sentinel.txt").unwrap());
    assert_eq!(
        fs::read(first.path().join("sentinel.txt")).unwrap(),
        b"unchanged\n"
    );

    let archive = first.path().join("archive");
    assert!(matches!(
        first_repo.archive(wrong_id, &archive, ArchiveOptions::directory()),
        Err(RepositoryError::WorkingCopyIdentityMismatch { .. })
    ));
    assert!(!archive.exists());

    assert!(matches!(
        first_repo.kg_enrich_files(wrong_id),
        Err(RepositoryError::WorkingCopyIdentityMismatch { .. })
    ));

    let sandbox = first.path().join("sandbox");
    assert!(matches!(
        first_repo.provision_sandbox(wrong_id, &sandbox, "dev"),
        Err(RepositoryError::WorkingCopyIdentityMismatch { .. })
    ));
    assert!(!sandbox.exists());
}

#[test]
fn ordinary_identity_survives_later_colocated_git_initialization() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let id = repo.require_working_copy_id().unwrap();
    drop(repo);

    run_git(directory.path(), &["init"]);

    let reopened = Repository::open_existing(directory.path()).unwrap();
    assert_eq!(reopened.require_working_copy_id().unwrap(), id);
    reopened.validate_working_copy(id).unwrap();
}

#[test]
fn full_materialization_updates_the_authoritative_record() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let id = repo.require_working_copy_id().unwrap();
    let before = repo.working_copy_record(id).unwrap();
    assert_eq!(before.materialized_state, None);

    repo.materialize(id).unwrap();

    let after = repo.working_copy_record(id).unwrap();
    assert_eq!(after.materialized_state, Some(after.desired_state));
    assert_eq!(after.materialized_manifest, None);
}

#[test]
fn linked_worktrees_keep_independent_desired_views_and_file_indexes() {
    let directory = TempDir::new().unwrap();
    let primary = directory.path().join("primary");
    let linked = directory.path().join("linked");
    fs::create_dir_all(&primary).unwrap();

    run_git(&primary, &["init"]);
    fs::write(primary.join("shared.txt"), b"base\n").unwrap();
    run_git(&primary, &["add", "shared.txt"]);
    run_git(
        &primary,
        &[
            "-c",
            "user.name=Atomic Test",
            "-c",
            "user.email=atomic@example.com",
            "commit",
            "-m",
            "initial",
        ],
    );

    let mut primary_repo = Repository::init(&primary).unwrap();
    let primary_id = primary_repo.require_working_copy_id().unwrap();
    primary_repo
        .add(primary_id, "shared.txt", TrackingOptions::default())
        .unwrap();
    primary_repo
        .record(
            primary_id,
            ChangeHeader::new("record shared file"),
            RecordOptions::new().with_all(true).apply_after_record(true),
        )
        .unwrap();
    primary_repo.create_view_from("feature", "dev").unwrap();
    drop(primary_repo);

    run_git(
        &primary,
        &[
            "worktree",
            "add",
            "-b",
            "linked-working-copy-independent",
            linked.to_str().unwrap(),
        ],
    );

    let linked_repo = Repository::open(&linked).unwrap();
    let linked_id = linked_repo.require_working_copy_id().unwrap();
    assert_ne!(linked_id, primary_id);
    fs::write(linked.join("shared.txt"), b"linked bytes\n").unwrap();
    linked_repo
        .update_file_index(
            linked_id,
            &[(
                "shared.txt".to_string(),
                10,
                20,
                13,
                Hash::of(b"linked bytes\n"),
            )],
        )
        .unwrap();
    assert_eq!(linked_repo.desired_view_name(linked_id).unwrap(), "dev");
    drop(linked_repo);

    let mut primary_repo = Repository::open(&primary).unwrap();
    let reopened_primary_id = primary_repo.require_working_copy_id().unwrap();
    assert_eq!(reopened_primary_id, primary_id);
    primary_repo
        .set_current_view(primary_id, "feature")
        .unwrap();
    primary_repo
        .update_file_index(
            primary_id,
            &[(
                "shared.txt".to_string(),
                30,
                40,
                14,
                Hash::of(b"primary bytes\n"),
            )],
        )
        .unwrap();

    let txn = primary_repo.pristine().read_txn().unwrap();
    let primary_index = txn
        .get_working_copy_file_index(primary_id, "shared.txt")
        .unwrap()
        .unwrap();
    let linked_index = txn
        .get_working_copy_file_index(linked_id, "shared.txt")
        .unwrap()
        .unwrap();
    assert_eq!(primary_index.3, Hash::of(b"primary bytes\n"));
    assert_eq!(linked_index.3, Hash::of(b"linked bytes\n"));
    drop(txn);

    assert_eq!(
        primary_repo.desired_view_name(primary_id).unwrap(),
        "feature"
    );
    assert_eq!(
        fs::read_to_string(primary.join(".atomic/current_view")).unwrap(),
        "feature\n"
    );
    assert_eq!(
        fs::read_to_string(linked.join(".atomic/current_view")).unwrap(),
        "dev\n"
    );
    drop(primary_repo);

    let linked_repo = Repository::open_existing(&linked).unwrap();
    assert_eq!(linked_repo.require_working_copy_id().unwrap(), linked_id);
    assert_eq!(linked_repo.desired_view_name(linked_id).unwrap(), "dev");
}
