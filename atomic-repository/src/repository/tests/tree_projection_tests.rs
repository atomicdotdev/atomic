use super::*;
use crate::record::RecordOptions;
use crate::tracking::{
    TrackingOptions, TreeProjectionKind, TreeProjectionOperation, TreeProjectionPlan,
};
use crate::unrecord::UnrecordOptions;
use atomic_core::pristine::{directory_flags, TreeTxnT};

fn verified_projection(repo: &Repository) -> VerifiedProspectiveEquivalence {
    let policy = ConversionPolicy::new(atomic_core::operation::GitHashAlgorithm::Sha1);
    let project = repo.project_tree(repo.current_view(), &policy).unwrap();
    verify_prospective_equivalence(&project, &project.git.root).unwrap()
}

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

fn projection_shape(repo: &Repository) -> Vec<(String, bool, Option<bool>)> {
    let txn = repo.pristine.read_txn().unwrap();
    let mut shape = Vec::new();
    for entry in txn.iter_tree().unwrap() {
        let (path, inode) = entry.unwrap();
        assert_eq!(txn.get_path(inode).unwrap().as_deref(), Some(path.as_str()));
        let position = txn
            .inode_position(inode)
            .unwrap()
            .unwrap_or_else(|| panic!("recorded path '{path}' has no inode position"));
        assert_eq!(txn.position_inode(position).unwrap(), Some(inode));
        let flags = txn.get_directory_flags(inode).unwrap();
        shape.push((path, flags.is_some(), flags.map(directory_flags::is_empty)));
    }
    shape.sort();
    shape
}

fn assert_directory_empty(repo: &Repository, path: &str, expected: bool) {
    let txn = repo.pristine.read_txn().unwrap();
    let inode = txn.get_inode(path).unwrap().unwrap();
    let flags = txn.get_directory_flags(inode).unwrap().unwrap();
    assert_eq!(
        directory_flags::is_empty(flags),
        expected,
        "{path}: {:?}",
        projection_shape(repo)
    );
}

#[test]
fn projection_tracks_first_last_child_and_cross_directory_move() {
    let (temp, repo) = create_temp_repo();
    std::fs::create_dir_all(temp.path().join("src/left")).unwrap();
    std::fs::create_dir_all(temp.path().join("src/right")).unwrap();
    std::fs::create_dir_all(temp.path().join("src/rightish")).unwrap();
    std::fs::write(temp.path().join("src/left/item.txt"), b"item\n").unwrap();
    std::fs::write(temp.path().join("src/right/seed.txt"), b"seed\n").unwrap();
    std::fs::write(temp.path().join("src/rightish/noise.txt"), b"noise\n").unwrap();
    repo.add("src/left/item.txt", TrackingOptions::default())
        .unwrap();
    repo.add("src/right/seed.txt", TrackingOptions::default())
        .unwrap();
    repo.add("src/rightish/noise.txt", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "add first children");

    assert_directory_empty(&repo, "src/left", false);
    assert_directory_empty(&repo, "src/right", false);
    std::fs::remove_file(temp.path().join("src/right/seed.txt")).unwrap();
    record_all(&repo, "empty target directory");
    assert_directory_empty(&repo, "src/right", true);

    std::fs::rename(
        temp.path().join("src/left/item.txt"),
        temp.path().join("src/right/item.txt"),
    )
    .unwrap();
    record_all(&repo, "move only child across directories");
    assert_directory_empty(&repo, "src/left", true);
    assert_directory_empty(&repo, "src/right", false);

    let before_reopen = projection_shape(&repo);
    drop(repo);
    let reopened = Repository::open(temp.path()).unwrap();
    assert_eq!(projection_shape(&reopened), before_reopen);
}

#[test]
fn sibling_replay_insert_unrecord_and_reinsert_share_projection() {
    let (temp, mut repo) = create_temp_repo();
    std::fs::create_dir_all(temp.path().join("box")).unwrap();
    std::fs::write(temp.path().join("box/seed.txt"), b"seed\n").unwrap();
    repo.add("box/seed.txt", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "add box");
    std::fs::remove_file(temp.path().join("box/seed.txt")).unwrap();
    record_all(&repo, "empty box");
    assert_directory_empty(&repo, "box", true);
    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();

    std::fs::write(temp.path().join("box/item.txt"), b"feature\n").unwrap();
    repo.add("box/item.txt", TrackingOptions::default())
        .unwrap();
    let added = record_all(&repo, "add feature child");
    let hash = *added.hash();
    assert_directory_empty(&repo, "box", false);

    repo.switch_view("dev").unwrap();
    assert_directory_empty(&repo, "box", true);
    repo.insert_change(&hash, InsertOptions::default()).unwrap();
    assert_directory_empty(&repo, "box", false);
    let inserted = projection_shape(&repo);

    repo.unrecord(&hash, UnrecordOptions::default()).unwrap();
    repo.reinsert_change(&hash, None).unwrap();
    assert_directory_empty(&repo, "box", false);
    assert_eq!(projection_shape(&repo), inserted);

    drop(repo);
    let reopened = Repository::open(temp.path()).unwrap();
    assert_eq!(projection_shape(&reopened), inserted);
}

#[test]
fn graph_first_direct_import_matches_native_record_projection() {
    let (source_temp, source) = create_temp_repo();
    let path = source_temp.path().join("src/domain/model.rs");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"pub struct Model;\n").unwrap();
    source
        .add("src/domain/model.rs", TrackingOptions::default())
        .unwrap();
    let recorded = record_all(&source, "native nested add");
    let native_shape = projection_shape(&source);

    let target_temp = TempDir::new().unwrap();
    let target = Repository::init(target_temp.path()).unwrap();
    let verified = verified_projection(&source);

    target
        .write_import_graph_change(
            recorded.change().clone(),
            &[],
            false,
            &verified,
            InsertOptions::default(),
        )
        .unwrap();
    assert_eq!(projection_shape(&target), native_shape);

    drop(target);
    let reopened = Repository::open(target_temp.path()).unwrap();
    assert_eq!(projection_shape(&reopened), native_shape);
}

#[test]
fn duplicate_path_claim_is_rejected_without_reverse_only_compatibility() {
    use atomic_core::pristine::PathClaimTxnT;

    let (_temp, repo) = create_temp_repo();
    let mut txn = repo.pristine.write_txn().unwrap();
    let primary = txn.alloc_inode().unwrap();
    let competing = txn.alloc_inode().unwrap();
    TreeProjectionPlan::plan(
        &txn,
        [TreeProjectionOperation::Add {
            inode: primary,
            path: Some("same.txt".to_string()),
            position: None,
            kind: TreeProjectionKind::File,
        }],
    )
    .unwrap()
    .apply(&mut txn)
    .unwrap();

    let error = TreeProjectionPlan::plan(
        &txn,
        [TreeProjectionOperation::Add {
            inode: competing,
            path: Some("same.txt".to_string()),
            position: None,
            kind: TreeProjectionKind::File,
        }],
    )
    .unwrap_err();
    assert!(error.to_string().contains("already owned"));
    assert_eq!(txn.get_inode("same.txt").unwrap(), Some(primary));
    assert_eq!(txn.get_path(primary).unwrap().as_deref(), Some("same.txt"));
    assert_eq!(txn.get_path(competing).unwrap(), None);
    txn.validate_tree_bijection().unwrap();
    txn.abort().unwrap();
}

#[test]
fn simultaneous_duplicate_projection_fails_before_mutation() {
    let (_temp, repo) = create_temp_repo();
    let mut txn = repo.pristine.write_txn().unwrap();
    let first = txn.alloc_inode().unwrap();
    let second = txn.alloc_inode().unwrap();
    let error = TreeProjectionPlan::plan(
        &txn,
        [
            TreeProjectionOperation::Undelete {
                inode: first,
                path: "same.txt".to_string(),
                kind: TreeProjectionKind::File,
            },
            TreeProjectionOperation::Undelete {
                inode: second,
                path: "same.txt".to_string(),
                kind: TreeProjectionKind::File,
            },
        ],
    )
    .unwrap_err();
    assert!(error.to_string().contains("projected owners"));
    assert_eq!(txn.get_inode("same.txt").unwrap(), None);
    assert_eq!(txn.get_path(first).unwrap(), None);
    assert_eq!(txn.get_path(second).unwrap(), None);
    txn.abort().unwrap();
}

#[test]
fn projection_conflict_fails_before_mutating_tree_pairs() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("a.txt"), b"a\n").unwrap();
    std::fs::write(temp.path().join("b.txt"), b"b\n").unwrap();
    repo.add_batch(&["a.txt", "b.txt"]).unwrap();
    let txn = repo.pristine.write_txn().unwrap();
    let a = txn.get_inode("a.txt").unwrap().unwrap();
    let b = txn.get_inode("b.txt").unwrap().unwrap();

    let error = TreeProjectionPlan::plan(
        &txn,
        [TreeProjectionOperation::Move {
            inode: a,
            path: "b.txt".to_string(),
        }],
    )
    .unwrap_err();
    assert!(error.to_string().contains("already owned"));
    assert_eq!(txn.get_inode("a.txt").unwrap(), Some(a));
    assert_eq!(txn.get_path(a).unwrap().as_deref(), Some("a.txt"));
    assert_eq!(txn.get_inode("b.txt").unwrap(), Some(b));
    assert_eq!(txn.get_path(b).unwrap().as_deref(), Some("b.txt"));
    txn.abort().unwrap();
}
