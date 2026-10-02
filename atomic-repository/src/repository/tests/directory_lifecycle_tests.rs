use super::*;
use crate::record::RecordOptions;
use atomic_core::change::GraphOp;
use atomic_core::pristine::{directory_flags, CrdtTxnT, TreeTxnT};
use atomic_core::types::EdgeFlags;

fn record_all(repo: &Repository, message: &str) -> RecordOutcome {
    repo.record(
        repo.require_working_copy_id().unwrap(),
        ChangeHeader::new(message),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap()
}

fn directory_empty(repo: &Repository, path: &str) -> bool {
    let txn = repo.pristine.read_txn().unwrap();
    let inode = txn.get_inode(path).unwrap().unwrap();
    let flags = txn.get_directory_flags(inode).unwrap().unwrap();
    directory_flags::is_empty(flags)
}

#[test]
fn directory_delete_and_undelete_preserve_exact_claims_and_inode() {
    let (temp, repo) = create_temp_repo();
    let path = "empty-dir";
    let directory = temp.path().join(path);
    std::fs::create_dir(&directory).unwrap();
    repo.add_directory(path, TrackingOptions::default())
        .unwrap();
    let added = record_all(&repo, "add explicit directory");
    let original_inode = repo.get_file_inode(path).unwrap().unwrap();
    let original_position = {
        let txn = repo.pristine.read_txn().unwrap();
        txn.inode_position(original_inode).unwrap().unwrap()
    };
    assert!(matches!(
        added.change().hunks(),
        [GraphOp::DirAdd { path: added_path, .. }] if added_path == path
    ));

    std::fs::remove_dir(&directory).unwrap();
    let deleted = record_all(&repo, "delete explicit directory");
    let del = deleted
        .change()
        .hunks()
        .iter()
        .find_map(|op| match op {
            GraphOp::DirDel {
                del,
                path: deleted_path,
            } if deleted_path == path => Some(del),
            _ => None,
        })
        .expect("record must emit DirDel");
    assert_eq!(del.edges.len(), 2, "DirDel must delete both DirAdd claims");
    assert!(del.edges.iter().all(|edge| {
        edge.previous == EdgeFlags::FOLDER | EdgeFlags::BLOCK
            && edge.flag == EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED
    }));
    let inode_claim = del
        .edges
        .iter()
        .find(|edge| edge.to.start == original_position.pos && edge.to.end == original_position.pos)
        .expect("name -> inode claim must be deleted");
    assert_ne!(
        inode_claim.from.pos, original_position.pos,
        "name -> inode deletion must not become an inode self-loop"
    );

    std::fs::create_dir(&directory).unwrap();
    let restored = record_all(&repo, "restore explicit directory");
    let undel = restored
        .change()
        .hunks()
        .iter()
        .find_map(|op| match op {
            GraphOp::DirUndel {
                undel,
                path: restored_path,
            } if restored_path == path => Some(undel),
            _ => None,
        })
        .expect("record must emit DirUndel");
    assert_eq!(undel.edges.len(), 2);
    assert_eq!(repo.get_file_inode(path).unwrap(), Some(original_inode));
    assert!(directory_empty(&repo, path));
}

#[test]
fn file_undelete_preserves_inode_crdt_identity_and_changed_content() {
    let (temp, repo) = create_temp_repo();
    let path = "restored.txt";
    let file = temp.path().join(path);
    std::fs::write(&file, b"before\n").unwrap();
    repo.add(path, TrackingOptions::default()).unwrap();
    record_all(&repo, "add file");

    let original_inode = repo.get_file_inode(path).unwrap().unwrap();
    let original_trunk = {
        let txn = repo.pristine.read_txn().unwrap();
        txn.get_crdt_inode_trunk(original_inode.get())
            .unwrap()
            .expect("recorded file has CRDT trunk")
    };

    std::fs::remove_file(&file).unwrap();
    record_all(&repo, "delete file");
    std::fs::write(&file, b"after\n").unwrap();

    let status = repo.status(StatusOptions::default()).unwrap();
    let entry = status
        .entries()
        .iter()
        .find(|entry| entry.path() == std::path::Path::new(path))
        .expect("reappearing deleted file must be recordable");
    assert_eq!(entry.status(), FileStatus::Added);
    assert_eq!(entry.inode(), Some(original_inode));

    let restored = record_all(&repo, "undelete and edit file");
    assert!(restored.change().hunks().iter().any(
        |op| matches!(op, GraphOp::FileUndel { path: restored_path, .. } if restored_path == path)
    ));
    assert!(!restored.change().hunks().iter().any(
        |op| matches!(op, GraphOp::FileAdd { path: restored_path, .. } if restored_path == path)
    ));
    assert_eq!(repo.get_file_inode(path).unwrap(), Some(original_inode));
    assert_eq!(
        repo.get_file_content(path).unwrap(),
        Some(b"after\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_via_crdt(path).unwrap(),
        Some(b"after\n".to_vec())
    );
    let txn = repo.pristine.read_txn().unwrap();
    assert_eq!(
        txn.get_crdt_inode_trunk(original_inode.get()).unwrap(),
        Some(original_trunk)
    );
}

#[test]
fn explicit_empty_directory_materializes_full_sequential_parallel_and_prefix() {
    let (temp, repo) = create_temp_repo();
    let path = "kept-empty";
    let directory = temp.path().join(path);
    std::fs::create_dir(&directory).unwrap();
    repo.add_directory(path, TrackingOptions::default())
        .unwrap();
    record_all(&repo, "add kept empty directory");

    std::fs::remove_dir(&directory).unwrap();
    repo.materialize().unwrap();
    assert!(directory.is_dir(), "parallel full materialization");

    std::fs::remove_dir(&directory).unwrap();
    repo.materialize_parallel(None).unwrap();
    assert!(directory.is_dir(), "explicit parallel materialization");

    std::fs::remove_dir(&directory).unwrap();
    repo.materialize_sequential().unwrap();
    assert!(directory.is_dir(), "sequential full materialization");

    std::fs::remove_dir(&directory).unwrap();
    repo.materialize_prefix(path).unwrap();
    assert!(directory.is_dir(), "prefix materialization");
}

#[test]
fn directory_occupancy_survives_reopen_and_sibling_views() {
    let (temp, mut repo) = create_temp_repo();
    let directory = temp.path().join("box");
    std::fs::create_dir(&directory).unwrap();
    repo.add_directory("box", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "add empty box");
    assert!(directory_empty(&repo, "box"));

    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    std::fs::write(directory.join("item.txt"), b"item\n").unwrap();
    repo.add("box/item.txt", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "add first child");
    assert!(!directory_empty(&repo, "box"));

    repo.switch_view("dev").unwrap();
    assert!(directory_empty(&repo, "box"));
    repo.switch_view("feature").unwrap();
    assert!(!directory_empty(&repo, "box"));

    std::fs::remove_file(directory.join("item.txt")).unwrap();
    record_all(&repo, "remove last child");
    assert!(directory_empty(&repo, "box"));

    drop(repo);
    let mut reopened = Repository::open(temp.path()).unwrap();
    assert!(directory_empty(&reopened, "box"));
    let working_copy = reopened.require_working_copy_id().unwrap();
    reopened.switch_view(working_copy, "dev").unwrap();
    assert!(directory_empty(&reopened, "box"));
}

#[test]
fn materialize_never_recursively_removes_untracked_directory_children() {
    let (temp, repo) = create_temp_repo();
    let path = "safe-dir";
    let directory = temp.path().join(path);
    std::fs::create_dir(&directory).unwrap();
    repo.add_directory(path, TrackingOptions::default())
        .unwrap();
    record_all(&repo, "add safe directory");
    std::fs::remove_dir(&directory).unwrap();
    record_all(&repo, "delete safe directory");

    std::fs::create_dir(&directory).unwrap();
    let untracked = directory.join("untracked.txt");
    std::fs::write(&untracked, b"keep me\n").unwrap();
    let error = repo.materialize().unwrap_err();
    assert!(error.to_string().contains("never removed recursively"));
    assert_eq!(std::fs::read(&untracked).unwrap(), b"keep me\n");
}
