//! File identities are selected by causal visibility, not ambient path equality.

use atomic_core::change::GraphOp;
use atomic_core::pristine::{GraphTxnT, MutTxnT, TreeTxnT, ViewScope, ViewTxnT};
use atomic_core::types::{Hash, Inode, Position};
use atomic_repository::apply::InsertOptions;
use atomic_repository::{RecordOptions, Repository};

fn record(repo: &Repository, message: &str) -> Hash {
    let working_copy = repo.require_working_copy_id().unwrap();
    let outcome = repo
        .record_with_message(working_copy, message, RecordOptions::default())
        .unwrap();
    assert!(!outcome.has_errors(), "{:?}", outcome.errors());
    *outcome.hash()
}

fn identity(repo: &Repository, path: &str) -> (Inode, Position<Hash>) {
    let txn = repo.pristine().read_txn().unwrap();
    let inode = txn.get_inode(path).unwrap().expect("tracked path");
    let pos = txn.inode_position(inode).unwrap().expect("recorded inode");
    let hash = txn.get_external(pos.change).unwrap().unwrap();
    (inode, Position::new(hash, pos.pos))
}

fn seed(repo: &Repository) {
    std::fs::write(repo.root().join("seed.txt"), "seed\n").unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "seed.txt",
        Default::default(),
    )
    .unwrap();
    record(repo, "seed");
}

#[test]
fn descendant_add_and_edit_preserve_inherited_inode() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    seed(&repo);
    repo.create_view_from("parent", "dev").unwrap();
    repo.switch_view(repo.require_working_copy_id().unwrap(), "parent")
        .unwrap();
    std::fs::write(dir.path().join("f.txt"), "ancestor\n").unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "f.txt",
        Default::default(),
    )
    .unwrap();
    let creator = record(&repo, "create inherited file");
    let original = identity(&repo, "f.txt");

    // Empty own change set: this must be ordinary ancestor closure, not a
    // copied change log or an ambient fallback to a sibling identity.
    let mut txn = repo.pristine().write_txn().unwrap();
    let parent = txn.get_view("parent").unwrap().unwrap();
    txn.create_view("child", ViewScope::Draft, Some(parent.id))
        .unwrap();
    txn.commit().unwrap();
    repo.switch_view(repo.require_working_copy_id().unwrap(), "child")
        .unwrap();
    assert_eq!(identity(&repo, "f.txt"), original);
    std::fs::write(dir.path().join("f.txt"), "descendant\n").unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "f.txt",
        Default::default(),
    )
    .unwrap();
    let edit = record(&repo, "edit inherited file");
    assert_eq!(identity(&repo, "f.txt"), original);
    let change = repo.load_change(&edit).unwrap();
    assert!(change.dependencies().contains(&creator));
    assert!(!change
        .hunks()
        .iter()
        .any(|h| matches!(h, GraphOp::FileAdd { path, .. } if path == "f.txt")));
}

#[test]
fn siblings_create_distinct_identities_even_with_identical_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    seed(&repo);
    repo.create_view_from("left", "dev").unwrap();
    repo.create_view_from("right", "dev").unwrap();
    repo.switch_view(repo.require_working_copy_id().unwrap(), "left")
        .unwrap();
    std::fs::write(dir.path().join("f.txt"), "same bytes\n").unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "f.txt",
        Default::default(),
    )
    .unwrap();
    let left = record(&repo, "left creates");
    let left_identity = identity(&repo, "f.txt");
    repo.switch_view(repo.require_working_copy_id().unwrap(), "right")
        .unwrap();
    std::fs::write(dir.path().join("f.txt"), "same bytes\n").unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "f.txt",
        Default::default(),
    )
    .unwrap();
    record(&repo, "right independently creates");
    let right_identity = identity(&repo, "f.txt");
    assert_ne!(left_identity.0, right_identity.0);
    assert_ne!(left_identity.1, right_identity.1);
    repo.insert_change_rec(&left, InsertOptions::default())
        .unwrap();
    repo.materialize(repo.require_working_copy_id().unwrap())
        .unwrap();
    let bytes = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
    assert!(bytes.contains("(name conflict)"), "{bytes}");
}

#[test]
fn inserting_the_same_creation_preserves_its_inode_and_graph_identity() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    seed(&repo);
    repo.create_view_from("left", "dev").unwrap();
    repo.create_view_from("right", "dev").unwrap();
    repo.switch_view(repo.require_working_copy_id().unwrap(), "left")
        .unwrap();
    std::fs::write(dir.path().join("f.txt"), "original\n").unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "f.txt",
        Default::default(),
    )
    .unwrap();
    let creator = record(&repo, "create once");
    let original = identity(&repo, "f.txt");
    repo.switch_view(repo.require_working_copy_id().unwrap(), "right")
        .unwrap();
    repo.insert_change_rec(&creator, InsertOptions::default())
        .unwrap();
    repo.materialize(repo.require_working_copy_id().unwrap())
        .unwrap();
    assert_eq!(identity(&repo, "f.txt"), original);
    repo.insert_change_rec(&creator, InsertOptions::default())
        .unwrap();
    assert_eq!(identity(&repo, "f.txt"), original);
    assert_eq!(
        std::fs::read(dir.path().join("f.txt")).unwrap(),
        b"original\n"
    );
}

#[test]
fn name_resolution_changes_namespace_edges_not_file_content() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    seed(&repo);
    repo.create_view_from("left", "dev").unwrap();
    repo.create_view_from("right", "dev").unwrap();
    repo.switch_view(repo.require_working_copy_id().unwrap(), "left")
        .unwrap();
    std::fs::write(dir.path().join("f.txt"), "left\n").unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "f.txt",
        Default::default(),
    )
    .unwrap();
    let left = record(&repo, "left creates");
    repo.switch_view(repo.require_working_copy_id().unwrap(), "right")
        .unwrap();
    std::fs::write(dir.path().join("f.txt"), "right\n").unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "f.txt",
        Default::default(),
    )
    .unwrap();
    record(&repo, "right creates");
    let retained = identity(&repo, "f.txt");
    repo.insert_change_rec(&left, InsertOptions::default())
        .unwrap();
    repo.materialize(repo.require_working_copy_id().unwrap())
        .unwrap();
    std::fs::write(dir.path().join("f.txt"), "right\n").unwrap();
    let resolution = record(&repo, "select right name");
    assert_eq!(identity(&repo, "f.txt"), retained);
    let change = repo.load_change(&resolution).unwrap();
    let mut namespace_edges = 0;
    for op in change.hunks() {
        if let GraphOp::SolveNameConflict { name, .. } = op {
            for edge in &name.edges {
                assert!(
                    edge.flag.is_folder(),
                    "name resolution tombstoned a content edge: {edge:?}"
                );
                namespace_edges += 1;
            }
        }
    }
    assert!(
        namespace_edges > 0,
        "resolution must be represented in the canonical graph"
    );
    assert!(
        !change.file_ops().iter().any(|ops| matches!(
            ops.trunk_op(),
            Some(atomic_core::crdt::TrunkOp::Delete { .. })
        )),
        "removing a name must not delete its semantic trunk"
    );
}

#[test]
fn namespace_resolution_and_concurrent_rename_preserve_both_identities() {
    for rename_first in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        seed(&repo);
        repo.create_view_from("left", "dev").unwrap();
        repo.create_view_from("right", "dev").unwrap();
        repo.switch_view(repo.require_working_copy_id().unwrap(), "left")
            .unwrap();
        std::fs::write(dir.path().join("f.txt"), "left\n").unwrap();
        repo.add(
            repo.require_working_copy_id().unwrap(),
            "f.txt",
            Default::default(),
        )
        .unwrap();
        let left = record(&repo, "left creates");
        let left_identity = identity(&repo, "f.txt");
        repo.switch_view(repo.require_working_copy_id().unwrap(), "right")
            .unwrap();
        std::fs::write(dir.path().join("f.txt"), "right\n").unwrap();
        repo.add(
            repo.require_working_copy_id().unwrap(),
            "f.txt",
            Default::default(),
        )
        .unwrap();
        record(&repo, "right creates");
        let right_identity = identity(&repo, "f.txt");
        repo.insert_change_rec(&left, InsertOptions::default())
            .unwrap();
        repo.materialize(repo.require_working_copy_id().unwrap())
            .unwrap();

        let rename = |repo: &mut Repository| {
            repo.switch_view(repo.require_working_copy_id().unwrap(), "left")
                .unwrap();
            std::fs::rename(dir.path().join("f.txt"), dir.path().join("saved.txt")).unwrap();
            let hash = record(repo, "concurrent rename");
            assert_eq!(identity(repo, "saved.txt"), left_identity);
            repo.switch_view(repo.require_working_copy_id().unwrap(), "right")
                .unwrap();
            hash
        };
        let mut rename_hash = None;
        if rename_first {
            rename_hash = Some(rename(&mut repo));
        }
        std::fs::write(dir.path().join("f.txt"), "right\n").unwrap();
        record(&repo, "select right name");
        assert_eq!(
            identity(&repo, "f.txt"),
            right_identity,
            "rename_first={rename_first}"
        );
        let rename_hash = rename_hash.unwrap_or_else(|| rename(&mut repo));
        repo.insert_change_rec(&rename_hash, InsertOptions::default())
            .unwrap();
        repo.materialize(repo.require_working_copy_id().unwrap())
            .unwrap();
        assert_eq!(identity(&repo, "f.txt"), right_identity);
        assert_eq!(identity(&repo, "saved.txt"), left_identity);
        assert_eq!(std::fs::read(dir.path().join("f.txt")).unwrap(), b"right\n");
        assert_eq!(
            std::fs::read(dir.path().join("saved.txt")).unwrap(),
            b"left\n"
        );
        repo.create_view_from("combined", "right").unwrap();
        repo.switch_view(repo.require_working_copy_id().unwrap(), "combined")
            .unwrap();
        assert_eq!(identity(&repo, "f.txt"), right_identity);
        assert_eq!(identity(&repo, "saved.txt"), left_identity);
        assert_eq!(
            std::fs::read(dir.path().join("saved.txt")).unwrap(),
            b"left\n"
        );
    }
}
