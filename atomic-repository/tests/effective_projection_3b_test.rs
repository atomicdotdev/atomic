use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use atomic_core::change::{Author, ChangeHeader, InodeAttrName};
use atomic_core::pristine::{
    CrdtTxnT, InodeAttrTxnT, MutTxnT, SetIdIndexMutTxnT, SetIdIndexTxnT, TreeTxnT, ViewTxnT,
};
use atomic_core::types::Hash;
use atomic_core::WorkingCopyId;
use atomic_repository::{effective_projection_closure, InsertOptions, RecordOptions, Repository};
use tempfile::TempDir;

const PATHS: [&str; 3] = ["a.txt", "b.txt", "c.txt"];

fn working_copy(repo: &Repository) -> WorkingCopyId {
    repo.require_working_copy_id().expect("working copy id")
}

fn init_repo() -> (Repository, TempDir, PathBuf) {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().to_path_buf();
    let repo = Repository::init(&path).expect("init repository");
    (repo, temp, path)
}

fn write_and_add(repo: &Repository, root: &Path, name: &str, content: &str) {
    fs::write(root.join(name), content).expect("write file");
    repo.add(working_copy(repo), name, Default::default())
        .expect("add file");
}

fn record(repo: &Repository, message: &str) -> Hash {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    *repo
        .record(working_copy(repo), header, RecordOptions::default())
        .expect("record")
        .hash()
}

#[derive(Debug, PartialEq, Eq)]
struct ProjectionSnapshot {
    content: Vec<(String, Vec<u8>)>,
    attributes: Vec<(String, String, String)>,
    semantics: Vec<(String, String, Vec<String>)>,
    conflicts: String,
}

fn projection_snapshot(repo: &Repository, root: &Path, view_name: &str) -> ProjectionSnapshot {
    let txn = repo.pristine().read_txn().expect("read txn");
    let view = txn
        .get_view(view_name)
        .expect("get view")
        .expect("view exists");
    let closure = effective_projection_closure(&txn, &view).expect("projection closure");

    let mut content = Vec::new();
    let mut attributes = Vec::new();
    let mut semantics = Vec::new();
    for path in PATHS {
        content.push((
            path.to_string(),
            fs::read(root.join(path)).expect("content"),
        ));

        let inode = txn.get_inode(path).expect("inode lookup").expect("inode");
        let position = txn
            .inode_position(inode)
            .expect("inode position lookup")
            .expect("inode position");
        let mode = txn
            .resolve_inode_attr(
                position,
                InodeAttrName::Mode,
                closure.attribute_visibility(),
            )
            .expect("mode state");
        let kind = txn
            .resolve_inode_attr(
                position,
                InodeAttrName::Kind,
                closure.attribute_visibility(),
            )
            .expect("kind state");
        attributes.push((path.to_string(), format!("{mode:?}"), format!("{kind:?}")));

        let trunk_id = txn
            .get_trunk_by_path(path)
            .expect("trunk lookup")
            .expect("trunk");
        assert!(closure
            .semantic_visibility()
            .contains(&trunk_id.change_id()));
        let trunk_key = trunk_id.to_bytes();
        let trunk = txn
            .get_crdt_trunk(&trunk_key)
            .expect("trunk row")
            .expect("serialized trunk");
        let mut rows = Vec::new();
        for branch_key in txn
            .iter_trunk_branches(&trunk_key)
            .expect("branch iterator")
        {
            let branch_key = branch_key.expect("branch key");
            let branch = txn
                .get_crdt_branch(&branch_key)
                .expect("branch row")
                .expect("serialized branch");
            rows.push(format!("branch:{branch_key:?}:{branch:?}"));
            for leaf_key in txn.iter_branch_leaves(&branch_key).expect("leaf iterator") {
                let leaf_key = leaf_key.expect("leaf key");
                let leaf = txn
                    .get_crdt_leaf(&leaf_key)
                    .expect("leaf row")
                    .expect("serialized leaf");
                rows.push(format!("leaf:{leaf_key:?}:{leaf:?}"));
            }
        }
        semantics.push((path.to_string(), format!("{trunk:?}"), rows));
    }
    drop(txn);

    ProjectionSnapshot {
        content,
        attributes,
        semantics,
        conflicts: format!(
            "{:?}",
            repo.list_conflicts(working_copy(repo))
                .expect("persisted conflicts")
        ),
    }
}

#[test]
fn every_valid_topological_order_has_equal_projection_state() {
    let (repo, _temp, root) = init_repo();
    let view = repo.current_view().to_string();

    write_and_add(&repo, &root, PATHS[0], "alpha\n");
    let a = record(&repo, "add a");
    write_and_add(&repo, &root, PATHS[1], "bravo\n");
    let b = record(&repo, "add b");
    write_and_add(&repo, &root, PATHS[2], "charlie\n");
    let c = record(&repo, "add c");

    repo.materialize(working_copy(&repo))
        .expect("materialize baseline");
    let expected = projection_snapshot(&repo, &root, &view);
    let expected_set_id = repo.view_set_id(&view).expect("baseline SetId");
    let orders = [
        [a, b, c],
        [a, c, b],
        [b, a, c],
        [b, c, a],
        [c, a, b],
        [c, b, a],
    ];

    for order in orders {
        repo.retain_view_changes(&view, &HashSet::new())
            .expect("clear independent changes");
        for hash in order {
            repo.insert_change(&hash, InsertOptions::default().view(&view))
                .expect("insert independent change");
        }
        repo.materialize(working_copy(&repo))
            .expect("materialize permutation");

        assert_eq!(
            repo.view_set_id(&view).expect("permutation SetId"),
            expected_set_id
        );
        assert_eq!(projection_snapshot(&repo, &root, &view), expected);
    }
}

#[test]
fn set_id_index_is_optional_versioned_and_rebuildable() {
    let (repo, _temp, root) = init_repo();
    let view_name = repo.current_view().to_string();
    write_and_add(&repo, &root, "a.txt", "alpha\n");
    record(&repo, "add a");

    let identity = repo.view_identity(&view_name).expect("canonical identity");
    let view_id = {
        let txn = repo.pristine().read_txn().expect("read txn");
        let view_id = txn
            .get_view(&view_name)
            .expect("view lookup")
            .expect("view")
            .id;
        assert_eq!(txn.get_set_id_index(view_id).expect("legacy lookup"), None);
        view_id
    };

    assert_eq!(repo.rebuild_set_id_index().expect("rebuild"), 1);
    let txn = repo.pristine().read_txn().expect("read rebuilt index");
    let row = txn
        .get_set_id_index(view_id)
        .expect("index lookup")
        .expect("persisted row");
    assert_eq!(row.merkle, identity.merkle);
    assert_eq!(row.set_id, identity.set_id);
    assert_eq!(row.closure_len, identity.closure_len);
    drop(txn);

    let mut txn = repo.pristine().write_txn().expect("write txn");
    txn.clear_set_id_index().expect("clear derived index");
    txn.commit().expect("commit clear");
    assert_eq!(
        repo.view_identity(&view_name).expect("fallback identity"),
        identity
    );
    assert_eq!(
        repo.refresh_view_set_id_index(&view_name)
            .expect("refresh one row"),
        identity
    );
}
