use atomic_core::change::ChangeKind;
use atomic_core::operation::{
    ActorRef, EffectPlan, EffectTarget, EffectValue, FileKind, FileState, OperationKind,
    OperationScope, RepoStateRef,
};
use atomic_core::pristine::MutTxnT;

use super::*;

fn record_options() -> RecordOptions {
    RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true)
}

fn direct_view_hashes(repo: &Repository, view_name: &str) -> Vec<Hash> {
    let txn = repo.pristine.read_txn().unwrap();
    let view = txn.get_view(view_name).unwrap().unwrap();
    txn.iter_changes(&view, 0)
        .unwrap()
        .map(|row| {
            let (_, node_id, _) = row.unwrap();
            txn.get_external(node_id).unwrap().unwrap()
        })
        .collect()
}

fn persisted_change_bytes(repo: &Repository, hash: &Hash) -> Vec<u8> {
    std::fs::read(repo.change_store().change_path(hash)).unwrap()
}

fn snapshot_chain(
    directory: &tempfile::TempDir,
    repo: &mut TestRepository,
    values: &[&[u8]],
) -> Vec<Hash> {
    let working_copy = repo.working_copy();
    let path = directory.path().join("retention.txt");
    std::fs::write(&path, b"baseline\n").unwrap();
    repo.add("retention.txt", TrackingOptions::default())
        .unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();
    let mut snapshots = Vec::new();
    for value in values {
        std::fs::write(&path, value).unwrap();
        snapshots.push(
            *repo
                .snapshot(
                    working_copy,
                    ChangeHeader::new("snapshot"),
                    record_options(),
                )
                .unwrap()
                .hash(),
        );
    }
    snapshots
}

#[test]
fn snapshot_replacement_is_baseline_relative_private_and_journaled() {
    let (directory, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("snapshot.txt");
    std::fs::write(&path, b"durable baseline\n").unwrap();
    repo.add("snapshot.txt", TrackingOptions::default())
        .unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();

    std::fs::write(&path, b"snapshot one\n").unwrap();
    let first = repo
        .snapshot(
            working_copy,
            ChangeHeader::new("snapshot one"),
            record_options(),
        )
        .unwrap();
    let first_hash = *first.hash();
    assert_eq!(
        first.change().kind(),
        &ChangeKind::Snapshot { working_copy }
    );
    assert!(first.change().supersedes().is_none());
    assert!(first.change().dependencies().iter().all(|dependency| repo
        .load_change(dependency)
        .unwrap()
        .kind()
        .is_durable()));

    let snapshot_view = Repository::snapshot_view_name(working_copy);
    let info = repo.get_view_info(&snapshot_view).unwrap();
    assert!(info.scope.is_draft());
    assert_eq!(info.parent_name.as_deref(), Some("dev"));
    assert_eq!(direct_view_hashes(&repo, &snapshot_view), vec![first_hash]);
    assert!(repo
        .log(HistoryOptions::default().view(snapshot_view.clone()))
        .unwrap()
        .is_empty());
    assert!(repo.view_manifest(&snapshot_view).is_err());
    assert!(repo
        .insert_change(&first_hash, InsertOptions::default().view("dev"),)
        .is_err());
    repo.create_draft_view("other-private", "dev").unwrap();
    assert!(repo
        .insert_change(&first_hash, InsertOptions::default().view("other-private"),)
        .is_err());
    assert!(repo
        .set_view_scope(&snapshot_view, ViewScope::Shared)
        .is_err());

    std::fs::write(&path, b"snapshot two\n").unwrap();
    let second = repo
        .snapshot(
            working_copy,
            ChangeHeader::new("snapshot two"),
            record_options(),
        )
        .unwrap();
    let second_hash = *second.hash();
    assert_eq!(second.change().supersedes(), Some(&first_hash));
    assert!(!second.change().dependencies().contains(&first_hash));
    assert_eq!(direct_view_hashes(&repo, &snapshot_view), vec![second_hash]);

    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), Some(1), false)
        .unwrap();
    let head = match log.head_state {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one operation head, found {other:?}"),
    };
    let details = repo.operation_details(head).unwrap();
    assert_eq!(details.verification, OperationVerificationState::Verified);
    assert_eq!(
        details.operation.payload().delta.metadata.len(),
        2,
        "replacement must journal one removal and one insertion lease"
    );
}

#[test]
fn promotion_reassembles_identical_content_without_snapshot_dependency() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("promote.txt");
    std::fs::write(&path, b"baseline\n").unwrap();
    repo.add("promote.txt", TrackingOptions::default()).unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();

    std::fs::write(&path, b"promoted content\n").unwrap();
    let snapshot = repo
        .snapshot(
            working_copy,
            ChangeHeader::new("snapshot"),
            record_options(),
        )
        .unwrap();
    let snapshot_hash = *snapshot.hash();
    let promoted = repo
        .promote_snapshot(
            working_copy,
            ChangeHeader::new("promote snapshot"),
            record_options(),
        )
        .unwrap();

    assert!(promoted.change().kind().is_durable());
    assert!(promoted.change().supersedes().is_none());
    assert!(!promoted.change().dependencies().contains(&snapshot_hash));
    assert_eq!(promoted.change().hunks(), snapshot.change().hunks());
    assert_eq!(promoted.change().contents, snapshot.change().contents);
    assert_eq!(
        promoted.change().hashed.file_ops,
        snapshot.change().hashed.file_ops
    );

    let snapshot_view = Repository::snapshot_view_name(working_copy);
    assert!(direct_view_hashes(&repo, &snapshot_view).is_empty());
    assert!(direct_view_hashes(&repo, "dev").contains(promoted.hash()));
    assert!(repo.status(StatusOptions::default()).unwrap().is_clean());
    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), Some(1), false)
        .unwrap();
    let head = match log.head_state {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one operation head, found {other:?}"),
    };
    let details = repo.operation_details(head).unwrap();
    assert_eq!(details.verification, OperationVerificationState::Verified);
    assert_eq!(details.operation.payload().delta.metadata.len(), 2);
}

#[test]
fn split_snapshot_materializes_index_and_remainder_without_snapshot_dependencies() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("split.txt");
    std::fs::write(&path, b"one\ntwo\nthree\n").unwrap();
    repo.add("split.txt", TrackingOptions::default()).unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();

    std::fs::write(&path, b"one\nTWO\nTHREE\n").unwrap();
    repo.snapshot(
        working_copy,
        ChangeHeader::new("snapshot"),
        record_options(),
    )
    .unwrap();

    let outcome = repo
        .split_snapshot(
            working_copy,
            IndexManifest::new(vec![IndexManifestEntry::present(
                "split.txt",
                b"one\nTWO\nthree\n".to_vec(),
            )]),
        )
        .unwrap();

    assert_eq!(
        repo.get_file_content_on_view("split.txt", "dev")
            .unwrap()
            .unwrap(),
        b"one\nTWO\nthree\n"
    );
    assert_eq!(
        repo.get_file_content_on_view("split.txt", &outcome.snapshot_view)
            .unwrap()
            .unwrap(),
        b"one\nTWO\nTHREE\n"
    );
    let lifecycle = repo.snapshot_status(working_copy).unwrap();
    assert_eq!(lifecycle.snapshot, None);
    assert_eq!(lifecycle.remainder, Some(outcome.remainder));
    let index = repo.load_change(&outcome.index).unwrap();
    let remainder = repo.load_change(&outcome.remainder).unwrap();
    assert!(index.kind().is_durable());
    assert!(remainder.kind().is_durable());
    assert!(remainder.dependencies().contains(&outcome.index));
    for dependency in index.dependencies().iter().chain(remainder.dependencies()) {
        assert!(!repo.load_change(dependency).unwrap().kind().is_snapshot());
    }
    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), Some(1), false)
        .unwrap();
    let head = match log.head_state {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one operation head, found {other:?}"),
    };
    let details = repo.operation_details(head).unwrap();
    assert_eq!(details.verification, OperationVerificationState::Verified);
    assert_eq!(details.operation.payload().delta.metadata.len(), 3);
}

#[cfg(unix)]
#[test]
fn split_snapshot_preserves_rename_edit_and_mode_only_state() {
    use std::os::unix::fs::PermissionsExt;

    let (directory, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    std::fs::write(directory.path().join("old.txt"), b"base\n").unwrap();
    std::fs::write(directory.path().join("mode.txt"), b"mode\n").unwrap();
    std::fs::write(directory.path().join("unstaged.txt"), b"base\n").unwrap();
    repo.add_batch(&["old.txt", "mode.txt", "unstaged.txt"])
        .unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();

    std::fs::rename(
        directory.path().join("old.txt"),
        directory.path().join("new.txt"),
    )
    .unwrap();
    std::fs::write(directory.path().join("new.txt"), b"renamed and edited\n").unwrap();
    std::fs::write(directory.path().join("unstaged.txt"), b"worktree\n").unwrap();
    std::fs::set_permissions(
        directory.path().join("mode.txt"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    repo.snapshot(
        working_copy,
        ChangeHeader::new("rename snapshot"),
        record_options(),
    )
    .unwrap();

    let manifest = IndexManifest::new(vec![
        IndexManifestEntry {
            path: "new.txt".to_string(),
            source_path: Some("old.txt".to_string()),
            state: IndexEntryState::Present {
                repository_bytes: Some(b"renamed and edited\n".to_vec()),
                mode: 0o644,
                kind: atomic_core::change::InodeKind::Regular,
            },
        },
        IndexManifestEntry {
            path: "mode.txt".to_string(),
            source_path: None,
            state: IndexEntryState::Present {
                repository_bytes: None,
                mode: 0o755,
                kind: atomic_core::change::InodeKind::Regular,
            },
        },
    ]);
    let outcome = repo.split_snapshot(working_copy, manifest).unwrap();

    assert!(repo
        .get_file_content_on_view("old.txt", "dev")
        .unwrap()
        .is_none());
    assert_eq!(
        repo.get_file_content_on_view("new.txt", "dev")
            .unwrap()
            .unwrap(),
        b"renamed and edited\n"
    );
    assert_eq!(
        repo.get_file_content_on_view("unstaged.txt", "dev")
            .unwrap()
            .unwrap(),
        b"base\n"
    );
    assert_eq!(
        repo.get_file_content_on_view("unstaged.txt", &outcome.snapshot_view)
            .unwrap()
            .unwrap(),
        b"worktree\n"
    );
    repo.materialize().unwrap();
    assert!(!directory.path().join("old.txt").exists());
    assert_eq!(
        std::fs::read(directory.path().join("new.txt")).unwrap(),
        b"renamed and edited\n"
    );
    assert_eq!(
        std::fs::read(directory.path().join("unstaged.txt")).unwrap(),
        b"base\n"
    );
    assert_eq!(
        std::fs::metadata(directory.path().join("mode.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );

    repo.switch_view(&outcome.snapshot_view).unwrap();
    assert_eq!(
        std::fs::read(directory.path().join("new.txt")).unwrap(),
        b"renamed and edited\n"
    );
    assert_eq!(
        std::fs::read(directory.path().join("unstaged.txt")).unwrap(),
        b"worktree\n"
    );
}

#[test]
fn split_snapshot_preserves_filter_and_opaque_binary_repository_bytes() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let git_ok = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(directory.path())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !git_ok {
        return;
    }
    std::fs::write(
        directory.path().join(".gitattributes"),
        b"filtered.txt text eol=crlf\nbinary.dat binary\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("filtered.txt"), b"base\r\n").unwrap();
    std::fs::write(directory.path().join("binary.dat"), b"base\0binary").unwrap();
    repo.add_batch(&[".gitattributes", "filtered.txt", "binary.dat"])
        .unwrap();
    assert!(std::process::Command::new("git")
        .args(["add", ".gitattributes", "filtered.txt", "binary.dat"])
        .current_dir(directory.path())
        .status()
        .unwrap()
        .success());
    repo.record(ChangeHeader::new("filtered baseline"), record_options())
        .unwrap();

    let final_binary = b"worktree\0binary\xff".to_vec();
    std::fs::write(directory.path().join("filtered.txt"), b"one\r\ntwo\r\n").unwrap();
    std::fs::write(directory.path().join("binary.dat"), &final_binary).unwrap();
    repo.snapshot(
        working_copy,
        ChangeHeader::new("filtered snapshot"),
        record_options(),
    )
    .unwrap();
    let outcome = repo
        .split_snapshot(
            working_copy,
            IndexManifest::new(vec![
                IndexManifestEntry::present("filtered.txt", b"one\ntwo\n".to_vec()),
                IndexManifestEntry::present("binary.dat", b"index\0binary\xfe".to_vec()),
            ]),
        )
        .unwrap();

    assert_eq!(
        repo.get_file_content_on_view("filtered.txt", "dev")
            .unwrap()
            .unwrap(),
        b"one\ntwo\n"
    );
    assert_eq!(
        repo.get_file_content_on_view("binary.dat", &outcome.snapshot_view)
            .unwrap()
            .unwrap(),
        final_binary
    );
}

#[test]
fn split_refusal_precedes_source_refs_and_filesystem_mutation() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("refuse.txt");
    std::fs::write(&path, b"baseline\n").unwrap();
    repo.add("refuse.txt", TrackingOptions::default()).unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();
    std::fs::write(&path, b"snapshot\n").unwrap();
    repo.snapshot(
        working_copy,
        ChangeHeader::new("snapshot"),
        record_options(),
    )
    .unwrap();
    let before_dev = direct_view_hashes(&repo, "dev");
    let snapshot_view = Repository::snapshot_view_name(working_copy);
    let before_snapshot = direct_view_hashes(&repo, &snapshot_view);
    let before_bytes = std::fs::read(&path).unwrap();

    let error = repo
        .split_snapshot(
            working_copy,
            IndexManifest::new(vec![
                IndexManifestEntry::present("refuse.txt", b"one\n".to_vec()),
                IndexManifestEntry::present("refuse.txt", b"two\n".to_vec()),
            ]),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        SplitSnapshotError::Refused(SnapshotSplitRefusal::DuplicatePath { .. })
    ));
    assert_eq!(direct_view_hashes(&repo, "dev"), before_dev);
    assert_eq!(direct_view_hashes(&repo, &snapshot_view), before_snapshot);
    assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
}

#[test]
fn snapshot_retention_deletes_old_objects_through_verified_effects() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("retention.txt");
    std::fs::write(&path, b"baseline\n").unwrap();
    repo.add("retention.txt", TrackingOptions::default())
        .unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();
    let mut snapshots = Vec::new();
    for value in [b"one\n".as_slice(), b"two\n", b"three\n"] {
        std::fs::write(&path, value).unwrap();
        snapshots.push(
            *repo
                .snapshot(
                    working_copy,
                    ChangeHeader::new("snapshot"),
                    record_options(),
                )
                .unwrap()
                .hash(),
        );
    }
    let status = repo.snapshot_status(working_copy).unwrap();
    assert_eq!(status.superseded_snapshots, 2);
    let retained = repo
        .prune_superseded_snapshots(
            working_copy,
            SnapshotRetentionPolicy {
                keep_superseded: 1,
                audit_retention_floor_unix: None,
            },
        )
        .unwrap();
    assert_eq!(retained.retained, vec![snapshots[1]]);
    assert_eq!(retained.deleted, vec![snapshots[0]]);
    assert!(!repo.has_change(&snapshots[0]));
}

#[test]
fn promotion_rejects_working_copy_content_newer_than_snapshot() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("stale.txt");
    std::fs::write(&path, b"baseline\n").unwrap();
    repo.add("stale.txt", TrackingOptions::default()).unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();

    std::fs::write(&path, b"snapshotted\n").unwrap();
    repo.snapshot(
        working_copy,
        ChangeHeader::new("snapshot"),
        record_options(),
    )
    .unwrap();
    std::fs::write(&path, b"newer unsnapshotted content\n").unwrap();

    let error = repo
        .promote_snapshot(
            working_copy,
            ChangeHeader::new("stale promotion"),
            record_options(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("create a new snapshot"));
}

/// Retention only-copy preservation (CB-13A ac-1): a superseded snapshot that
/// is still pinned by a view reference is a live root. Age and policy window
/// alone (`keep_superseded = 0`) must never collect it — the retention
/// candidate filter consults view membership, skips the pinned object, and
/// its canonical bytes survive in the change store.
#[test]
fn snapshot_retention_never_collects_view_pinned_superseded_object() {
    let (directory, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("retention.txt");
    std::fs::write(&path, b"baseline\n").unwrap();
    repo.add("retention.txt", TrackingOptions::default())
        .unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();
    let mut snapshots = Vec::new();
    for value in [b"one\n".as_slice(), b"two\n"] {
        std::fs::write(&path, value).unwrap();
        snapshots.push(
            *repo
                .snapshot(
                    working_copy,
                    ChangeHeader::new("snapshot"),
                    record_options(),
                )
                .unwrap()
                .hash(),
        );
    }
    let pinned = snapshots[0];
    let snapshot_view = Repository::snapshot_view_name(working_copy);

    // Pin the superseded object with an explicit keep ref: a private draft
    // view whose change log references it, written through the real view-log
    // bookkeeping. This is one of the retention roots the policy must respect.
    repo.create_draft_view("keep", "dev").unwrap();
    let mut txn = repo.pristine.write_txn().unwrap();
    let mut keep = txn.open_or_create_view("keep").unwrap();
    let change_id = txn.get_internal(&pinned).unwrap().unwrap();
    txn.put_change(&mut keep, change_id, &pinned).unwrap();
    txn.update_view(&keep).unwrap();
    txn.commit().unwrap();

    assert!(
        repo.views_containing_change(&pinned)
            .unwrap()
            .contains(&"keep".to_string()),
        "the pin must be a real view reference"
    );

    let outcome = repo
        .prune_superseded_snapshots(
            working_copy,
            SnapshotRetentionPolicy {
                keep_superseded: 0,
                audit_retention_floor_unix: None,
            },
        )
        .unwrap();
    assert!(
        outcome.deleted.is_empty(),
        "age/policy collected a view-pinned root: {:?}",
        outcome.deleted
    );
    assert!(repo.has_change(&pinned));
    // The private snapshot view itself is untouched by the refusal.
    assert_eq!(
        direct_view_hashes(&repo, &snapshot_view),
        vec![snapshots[1]]
    );
}

/// CB-13A R1 (newly-pinned-after-observation): a root established after an
/// out-of-band observation but before the retention pass must be visible to
/// the plan. Pruning is lock-first and re-derives every candidate's
/// liveness under the held operation boundary, so the pinned object is
/// retained and its persisted bytes survive a full reopen byte-for-byte.
#[test]
fn snapshot_retention_recomputes_a_root_pinned_after_observation() {
    let (directory, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let snapshots = snapshot_chain(&directory, &mut repo, &[b"one\n", b"two\n"]);
    let pinned = snapshots[0];

    // Out-of-band observation BEFORE the retention pass: the object looks
    // like an expired deletion candidate at this instant.
    let observed_status = repo.snapshot_status(working_copy).unwrap();
    assert_eq!(observed_status.superseded_snapshots, 1);

    // The root is established after that observation, before pruning:
    // a private draft view whose change log references the object, written
    // through the real view-log bookkeeping.
    repo.create_draft_view("keep", "dev").unwrap();
    let mut txn = repo.pristine.write_txn().unwrap();
    let mut keep = txn.open_or_create_view("keep").unwrap();
    let change_id = txn.get_internal(&pinned).unwrap().unwrap();
    txn.put_change(&mut keep, change_id, &pinned).unwrap();
    txn.update_view(&keep).unwrap();
    txn.commit().unwrap();
    let before_bytes = persisted_change_bytes(&repo, &pinned);

    let outcome = repo
        .prune_superseded_snapshots(
            working_copy,
            SnapshotRetentionPolicy {
                keep_superseded: 0,
                audit_retention_floor_unix: None,
            },
        )
        .unwrap();
    assert!(
        outcome.deleted.is_empty(),
        "a root pinned after observation was collected: {:?}",
        outcome.deleted
    );
    drop(repo);

    // Persisted object bytes survive a full reopen unchanged.
    let repo = Repository::open(directory.path()).unwrap();
    assert!(repo.has_change(&pinned));
    assert_eq!(persisted_change_bytes(&repo, &pinned), before_bytes);
}

/// CB-13A R1 (recoverable-operation root): while an operation head is still
/// incomplete, retention must refuse destructive pruning outright; after
/// recovery completes, an object still referenced by the journal or receipts
/// stays rooted and only the provably unrooted candidate is collected.
#[test]
fn snapshot_retention_refuses_and_preserves_objects_rooted_by_an_interrupted_operation() {
    let (directory, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let snapshots = snapshot_chain(&directory, &mut repo, &[b"one\n", b"two\n", b"three\n"]);
    let rooted_candidate = snapshots[0];
    let deletable_candidate = snapshots[1];
    let before_bytes = persisted_change_bytes(&repo, &rooted_candidate);

    // Prepare (but never execute or finalize) a retention-like operation
    // whose filesystem lease references the first candidate's durable bytes.
    let operation_lock = repo.try_lock_operation(working_copy).unwrap();
    let record = repo.working_copy_record(working_copy).unwrap();
    let state = RepoStateRef {
        view: None,
        working_copy: Some(super::super::operation::working_copy_state_ref(record)),
        git: None,
    };
    let change_path = repo.change_store().change_path(&rooted_candidate);
    let bytes = std::fs::read(&change_path).unwrap();
    let relative = change_path
        .strip_prefix(&repo.root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&change_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    #[cfg(not(unix))]
    let mode = 0o644;
    let prepared = repo
        .prepare_working_copy_transition(
            &operation_lock,
            OperationKind::Record,
            None,
            state.clone(),
            state,
            vec![EffectPlan {
                ordinal: 0,
                target: EffectTarget::FilesystemPath { path: relative },
                expected_old: EffectValue::File(FileState {
                    kind: FileKind::Regular,
                    mode,
                    content: atomic_core::types::Hash::of(&bytes),
                }),
                expected_new: EffectValue::Absent,
            }],
            Vec::new(),
            ActorRef::System {
                name: "interrupted-retention-fixture".to_string(),
            },
            super::super::operation::current_operation_timestamp_ms(),
        )
        .unwrap();
    drop(prepared);
    drop(operation_lock);

    // Destructive retention refuses while the head is incomplete.
    let error = repo
        .prune_superseded_snapshots(
            working_copy,
            SnapshotRetentionPolicy {
                keep_superseded: 1,
                audit_retention_floor_unix: None,
            },
        )
        .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("retention refused") && message.contains("incomplete head"),
        "unexpected refusal: {message}"
    );
    assert!(repo.has_change(&rooted_candidate));
    assert!(repo.has_change(&deletable_candidate));
    drop(repo);

    // Read-only diagnosis contract (CB-13A R2): the ordinary read-only open
    // gates the incomplete head with an actionable error instead of
    // recovering it; the dedicated operation-inspection open permits the
    // same state and the public operation log names the unverified head.
    let readonly_error = Repository::open_readonly(directory.path()).unwrap_err();
    assert!(
        readonly_error
            .to_string()
            .contains("retry with a writable repository open"),
        "unexpected read-only gate: {readonly_error}"
    );
    let inspection = Repository::open_readonly_for_operation_inspection(directory.path()).unwrap();
    let working_copy_id = inspection.require_working_copy_id().unwrap();
    let log = inspection
        .operation_log(OperationScope::WorkingCopy(working_copy_id), None, false)
        .unwrap();
    let incomplete = log
        .entries
        .iter()
        .filter(|entry| entry.is_head)
        .map(|entry| entry.verification)
        .collect::<Vec<_>>();
    assert!(
        !incomplete.is_empty()
            && incomplete
                .iter()
                .all(|state| *state != crate::OperationVerificationState::Verified),
        "the inspection log must show the unverified head: {incomplete:?}"
    );
    drop(inspection);

    // A writable reopen completes recovery; the interrupted operation and
    // its recovery child still reference the first candidate, so it stays
    // rooted while the unreferenced candidate is collected.
    let repo = Repository::open(directory.path()).unwrap();
    assert!(repo.has_change(&rooted_candidate));
    let outcome = repo
        .prune_superseded_snapshots(
            working_copy,
            SnapshotRetentionPolicy {
                keep_superseded: 0,
                audit_retention_floor_unix: None,
            },
        )
        .unwrap();
    assert_eq!(outcome.deleted, vec![deletable_candidate]);
    assert!(!repo.has_change(&deletable_candidate));
    assert!(repo.has_change(&rooted_candidate));
    assert_eq!(
        persisted_change_bytes(&repo, &rooted_candidate),
        before_bytes
    );
}

/// CB-13A R1 (incomplete-session root): an old session record that references
/// a superseded snapshot keeps it rooted regardless of age or policy window;
/// the persisted bytes survive a full reopen byte-for-byte.
#[test]
fn snapshot_retention_preserves_an_object_rooted_by_an_incomplete_session_record() {
    let (directory, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let snapshots = snapshot_chain(&directory, &mut repo, &[b"one\n", b"two\n"]);
    let rooted = snapshots[0];
    let before_bytes = persisted_change_bytes(&repo, &rooted);

    // An old incomplete-session record is the only root: its JSON references
    // the snapshot change by hash, and it is far older than any window.
    let sessions_dir = directory.path().join(".atomic/sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
        sessions_dir.join("old-session.json"),
        format!(
            "{{\"session_id\":\"old-session\",\"status\":\"incomplete\",\
             \"ended_at\":1,\"recorded_change\":\"{}\"}}",
            rooted.to_base32()
        ),
    )
    .unwrap();

    let outcome = repo
        .prune_superseded_snapshots(
            working_copy,
            SnapshotRetentionPolicy {
                keep_superseded: 0,
                audit_retention_floor_unix: None,
            },
        )
        .unwrap();
    assert!(
        outcome.deleted.is_empty(),
        "age collected an object rooted by an incomplete session: {:?}",
        outcome.deleted
    );
    assert!(repo.has_change(&rooted));
    drop(repo);

    let repo = Repository::open(directory.path()).unwrap();
    assert!(repo.has_change(&rooted));
    assert_eq!(persisted_change_bytes(&repo, &rooted), before_bytes);
}

/// Retention keep-ref + audit-wall (CB-13A ac-1): a Git ref under
/// `refs/atomic/keep/…` pins its referenced object against age-based
/// collection, and the audit-retention wall clock keeps recently-recorded
/// objects even when the policy window would expire them. Content roots
/// stay distinct: a keep-ref-pinned object survives; the unpinned object
/// with the same age still collects.
#[test]
fn snapshot_retention_respects_git_keep_refs_and_audit_wall() {
    let (directory, repo) = create_temp_repo();
    // The keep-ref root scans Git refs: the fixture needs a Git repository.
    git2::Repository::init(directory.path()).unwrap();
    let working_copy = repo.working_copy();
    let path = directory.path().join("retention.txt");
    std::fs::write(&path, b"baseline\n").unwrap();
    repo.add("retention.txt", TrackingOptions::default())
        .unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();
    let mut snapshots = Vec::new();
    for value in [b"one\n".as_slice(), b"two\n"] {
        std::fs::write(&path, value).unwrap();
        snapshots.push(
            *repo
                .snapshot(
                    working_copy,
                    ChangeHeader::new("snapshot"),
                    record_options(),
                )
                .unwrap()
                .hash(),
        );
    }

    // A keep ref pins the SECOND snapshot (the supersedes chain's newer
    // member): create a Git commit whose raw bytes name the change hash,
    // under refs/atomic/keep/pin.
    let base32 = snapshots[1].to_base32();
    let git = git2::Repository::open(directory.path()).unwrap();
    let signature = git2::Signature::now("Retention Test", "retention@test.invalid").unwrap();
    let tree_id = git.treebuilder(None).unwrap().write().unwrap();
    let tree = git.find_tree(tree_id).unwrap();
    git.commit(
        Some("refs/atomic/keep/pin"),
        &signature,
        &signature,
        &format!("keep pin {base32}\n"),
        &tree,
        &[],
    )
    .unwrap();

    // A third snapshot so the chain has an UNPINNED candidate: head = s2,
    // supersedes chain = [s1, s0]. The keep ref pins s1.
    std::fs::write(&path, b"three\n").unwrap();
    let s2 = *repo
        .snapshot(
            working_copy,
            ChangeHeader::new("snapshot"),
            record_options(),
        )
        .unwrap()
        .hash();
    let _ = s2;

    // Pass A — no wall (`None`): the window alone governs. With
    // keep_superseded = 0 both chain objects are audit-expired candidates;
    // the keep ref ROOTS s1, so only s0 collects.
    let outcome = repo
        .prune_superseded_snapshots(
            working_copy,
            SnapshotRetentionPolicy {
                keep_superseded: 0,
                audit_retention_floor_unix: None,
            },
        )
        .unwrap();
    assert_eq!(
        outcome.deleted,
        vec![snapshots[0]],
        "the keep-ref pin roots s1; the unpinned s0 collects: {outcome:?}"
    );
    assert!(repo.has_change(&snapshots[1]), "the keep-ref pin held");
    assert!(!repo.has_change(&snapshots[0]));

    // Pass B — a PAST retention floor (keep everything recorded at/after
    // the epoch): every candidate is audit-retained even though the window
    // would expire it. A NEW snapshot first, so the chain has a fresh
    // unpinned object.
    std::fs::write(&path, b"four\n").unwrap();
    let fourth = *repo
        .snapshot(
            working_copy,
            ChangeHeader::new("snapshot"),
            record_options(),
        )
        .unwrap()
        .hash();
    let outcome = repo
        .prune_superseded_snapshots(
            working_copy,
            SnapshotRetentionPolicy {
                keep_superseded: 0,
                audit_retention_floor_unix: Some(0),
            },
        )
        .unwrap();
    assert!(
        outcome.deleted.is_empty(),
        "the past wall audit-retains everything recorded since the epoch: {outcome:?}"
    );
    assert!(repo.has_change(&snapshots[1]));
    assert!(repo.has_change(&fourth));

    // Pass C — a FUTURE floor (every recorded object predates the horizon):
    // audit expiry permits collection again; the keep ref STILL pins s1.
    let outcome = repo
        .prune_superseded_snapshots(
            working_copy,
            SnapshotRetentionPolicy {
                keep_superseded: 0,
                audit_retention_floor_unix: Some(chrono::Utc::now().timestamp() + 3600),
            },
        )
        .unwrap();
    assert_eq!(
        outcome.deleted,
        vec![s2],
        "audit-expired unpinned objects collect (the head snapshot is never a candidate): {outcome:?}"
    );
    assert!(
        !outcome.deleted.contains(&snapshots[1]),
        "the keep-ref pin survives across the wall"
    );
    assert!(repo.has_change(&snapshots[1]));
}

/// CB-13A follow-up R1 (schema-aware decoded session roots): an incomplete
/// session's DECODED `last_attestation` hash is a durable content root —
/// the attestation object survives age-based collection. The raw byte scan
/// alone would miss it when the JSON stores the hash in a decoded field
/// that the byte scan happens to cover, but the schema-aware pass proves
/// it structurally (and any undecodable session file conservatively roots).
#[test]
fn snapshot_retention_honors_decoded_incomplete_session_attestation_root() {
    let (directory, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let snapshots = snapshot_chain(&directory, &mut repo, &[b"one\n", b"two\n"]);
    let rooted = snapshots[0];
    let before_bytes = persisted_change_bytes(&repo, &rooted);

    let sessions_dir = directory.path().join(".atomic/sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();

    // An incomplete session whose JSON is a VALID decode but does NOT
    // contain the raw change hash as text anywhere: the last_attestation
    // field pins a DIFFERENT (synthetic) hash, so the schema-aware path is
    // exercised for the decode-refusal branch (undecodable => conservative
    // root) — here we instead pin the decoded shape: status.incomplete
    // present, and the last_attestation IS the rooted change.
    let rooted_b32 = rooted.to_base32();
    let session_json = serde_json::json!({
        "session_id": "att-root-sess",
        "view_name": "dev",
        "phase": "idle",
        "status": { "incomplete": {
            "reason": "unexplained Git transition (fixture)",
            "paths": [],
            "recovery_ref": "",
            "origin": "unattributed_git_operation",
            "unbound_commits": []
        }},
        "turn_count": 1,
        "agent_name": "claude-code",
        "agent_display_name": "Claude Code",
        "started_at": "2026-09-18T00:00:00Z",
        "last_attestation": rooted_b32,
        "boundary_start": null,
        "boundary_end": null,
        "turn_outcomes": [],
        "first_prompt": "",
        "current_turn_started_at": null,
        "mac_key": null,
        "attested_operations": [],
        "repair_history": [],
        "evidence_retained": true
    });
    std::fs::write(
        sessions_dir.join("att-root-sess.json"),
        serde_json::to_vec(&session_json).unwrap(),
    )
    .unwrap();

    let outcome = repo
        .prune_superseded_snapshots(
            working_copy,
            SnapshotRetentionPolicy {
                keep_superseded: 0,
                audit_retention_floor_unix: None,
            },
        )
        .unwrap();
    assert!(
        outcome.deleted.is_empty(),
        "age collected an object whose change hash is the incomplete session's decoded \
         last_attestation root: {:?}",
        outcome.deleted
    );
    assert!(repo.has_change(&rooted));
    drop(repo);
    let repo = Repository::open(directory.path()).unwrap();
    assert_eq!(persisted_change_bytes(&repo, &rooted), before_bytes);
}

/// CB-13A follow-up R1 (fail-closed enumeration): an enumeration failure
/// inside a retention store NEVER establishes absence — an unreadable
/// subdirectory inside `working-copies/` makes destructive retention refuse
/// (the working-copy root scan cannot be proven clean), and the persisted
/// bytes survive unchanged after reopen.
#[cfg(unix)]
#[test]
fn snapshot_retention_refuses_when_enumeration_fails() {
    use std::os::unix::fs::PermissionsExt;
    let (directory, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let snapshots = snapshot_chain(&directory, &mut repo, &[b"one\n", b"two\n"]);
    let candidate = snapshots[0];
    let before_bytes = persisted_change_bytes(&repo, &candidate);

    // An unreadable directory inside working-copies/: the root scan cannot
    // enumerate its per-working-copy stores. The store DIRECTORY exists
    // (with a workspaces/ child) so the scan must enumerate it and fail —
    // a missing directory would prove absence honestly.
    let working_copies_dir = directory.path().join(".atomic/working-copies");
    std::fs::create_dir_all(&working_copies_dir).unwrap();
    let unreadable = working_copies_dir.join("opaque-wc");
    std::fs::create_dir_all(unreadable.join("workspaces")).unwrap();
    let original_mode = std::fs::metadata(&unreadable).unwrap().permissions().mode();
    std::fs::set_permissions(&unreadable, PermissionsExt::from_mode(0o000)).unwrap();

    let outcome = repo.prune_superseded_snapshots(
        working_copy,
        SnapshotRetentionPolicy {
            keep_superseded: 0,
            audit_retention_floor_unix: None,
        },
    );
    // Restore permissions FIRST (drop needs to clean the temp dir).
    std::fs::set_permissions(&unreadable, PermissionsExt::from_mode(original_mode)).unwrap();

    match outcome {
        Ok(outcome) => {
            // The unreadable store may prove nothing; conservative behavior
            // retains everything.
            assert!(
                outcome.deleted.is_empty(),
                "retention collected objects while a retention store could not be \
                 enumerated: {:?}",
                outcome.deleted
            );
        }
        Err(error) => {
            let text = error.to_string();
            assert!(
                !text.contains("panicked"),
                "the refusal must be typed, not a panic: {text}"
            );
        }
    }
    assert!(repo.has_change(&candidate));
    drop(repo);
    let repo = Repository::open(directory.path()).unwrap();
    assert_eq!(persisted_change_bytes(&repo, &candidate), before_bytes);
}
