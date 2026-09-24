use super::*;
use crate::apply::CrossViewInsertOptions;
use crate::record::RecordOptions;
use crate::tracking::TrackingOptions;
use crate::unrecord::UnrecordOptions;
use atomic_core::change::GraphOp;
use atomic_core::pristine::{MutTxnT, TreeTxnT, ViewTxnT};
use atomic_core::types::{ChangePosition, Hash, Position};

fn record_all(repo: &Repository, message: &str) -> crate::record::RecordOutcome {
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

fn assert_parent(actual: Position<Option<Hash>>, expected: Position<Option<Hash>>) {
    assert_eq!(actual, expected);
}

#[test]
fn nested_record_uses_inode_anchors_dependencies_and_survives_reopen() {
    let (temp_dir, repo) = create_temp_repo();
    let model_path = temp_dir.path().join("src/domain/model.rs");
    std::fs::create_dir_all(model_path.parent().unwrap()).unwrap();
    std::fs::write(&model_path, b"pub struct Model;\n").unwrap();
    repo.add("src/domain/model.rs", TrackingOptions::default())
        .unwrap();

    let first = record_all(&repo, "add nested model");
    let change = first.change();
    let root = Position {
        change: Some(Hash::NONE),
        pos: ChangePosition::ROOT,
    };

    let (src_anchor, domain_anchor, model_parent) = match change.hunks() {
        [GraphOp::DirAdd {
            add_name: src_name,
            add_inode: src_inode,
            path: src_path,
        }, GraphOp::DirAdd {
            add_name: domain_name,
            add_inode: domain_inode,
            path: domain_path,
        }, GraphOp::FileAdd {
            add_name: model_name,
            path: model_path,
            ..
        }, ..] => {
            assert_eq!(src_path, "src");
            assert_eq!(domain_path, "src/domain");
            assert_eq!(model_path, "src/domain/model.rs");
            assert_parent(src_name.predecessors[0], root);
            assert_parent(src_name.inode, root);

            let src_anchor = Position {
                change: None,
                pos: src_inode.start,
            };
            assert_parent(domain_name.predecessors[0], src_anchor);
            assert_parent(domain_name.inode, src_anchor);

            let domain_anchor = Position {
                change: None,
                pos: domain_inode.start,
            };
            assert_parent(model_name.predecessors[0], domain_anchor);
            assert_parent(model_name.inode, domain_anchor);
            (src_anchor, domain_anchor, model_name.predecessors[0])
        }
        hunks => panic!("unexpected nested add topology: {hunks:#?}"),
    };
    assert_ne!(src_anchor, root);
    assert_ne!(domain_anchor, root);
    assert_eq!(model_parent, domain_anchor);
    assert!(change.dependencies().is_empty());
    assert_eq!(
        repo.get_file_content("src/domain/model.rs").unwrap(),
        Some(b"pub struct Model;\n".to_vec())
    );

    let service_path = temp_dir.path().join("src/domain/service.rs");
    std::fs::write(&service_path, b"pub struct Service;\n").unwrap();
    repo.add("src/domain/service.rs", TrackingOptions::default())
        .unwrap();
    let second = record_all(&repo, "add nested service");
    assert!(second.change().dependencies().contains(first.hash()));

    let persisted_domain_anchor = {
        let txn = repo.pristine.read_txn().unwrap();
        let inode = txn.get_inode("src/domain").unwrap().unwrap();
        let position = txn.inode_position(inode).unwrap().unwrap();
        Position {
            change: Some(*first.hash()),
            pos: position.pos,
        }
    };
    let service_parent = second
        .change()
        .hunks()
        .iter()
        .find_map(|op| match op {
            GraphOp::FileAdd { add_name, path, .. } if path == "src/domain/service.rs" => {
                Some(add_name.predecessors[0])
            }
            _ => None,
        })
        .expect("service FileAdd");
    assert_parent(service_parent, persisted_domain_anchor);

    drop(repo);
    let reopened = Repository::open(temp_dir.path()).unwrap();
    assert_eq!(
        reopened.get_file_content("src/domain/model.rs").unwrap(),
        Some(b"pub struct Model;\n".to_vec())
    );
    assert_eq!(
        reopened.get_file_content("src/domain/service.rs").unwrap(),
        Some(b"pub struct Service;\n".to_vec())
    );

    std::fs::remove_dir_all(temp_dir.path().join("src")).unwrap();
    reopened
        .materialize(reopened.require_working_copy_id().unwrap())
        .unwrap();
    assert_eq!(std::fs::read(model_path).unwrap(), b"pub struct Model;\n");
    assert_eq!(
        std::fs::read(service_path).unwrap(),
        b"pub struct Service;\n"
    );
}

#[test]
fn nested_add_remains_isolated_then_survives_insert_unrecord_and_reinsert() {
    let (temp_dir, mut repo) = create_temp_repo();
    repo.create_view_from("left", "dev").unwrap();
    repo.create_view_from("right", "dev").unwrap();
    repo.switch_view("left").unwrap();

    let path = temp_dir.path().join("src/domain/model.rs");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"left model\n").unwrap();
    repo.add("src/domain/model.rs", TrackingOptions::default())
        .unwrap();
    let recorded = record_all(&repo, "left nested model");
    let hash = *recorded.hash();

    assert_eq!(
        repo.get_file_content_on_view("src/domain/model.rs", "right")
            .unwrap(),
        None
    );
    repo.insert_from_view(CrossViewInsertOptions::new("left", "right"))
        .unwrap();
    assert_eq!(
        repo.get_file_content_on_view("src/domain/model.rs", "right")
            .unwrap(),
        Some(b"left model\n".to_vec())
    );

    repo.switch_view("right").unwrap();
    repo.unrecord(&hash, UnrecordOptions::default()).unwrap();
    assert_eq!(repo.get_file_content("src/domain/model.rs").unwrap(), None);
    std::fs::remove_dir_all(temp_dir.path().join("src")).unwrap();

    repo.reinsert_change(&hash, None).unwrap();
    repo.materialize().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"left model\n");
}

#[test]
fn nested_record_fails_closed_when_parent_inode_position_is_missing() {
    let (temp_dir, repo) = create_temp_repo();
    let first_path = temp_dir.path().join("src/domain/model.rs");
    std::fs::create_dir_all(first_path.parent().unwrap()).unwrap();
    std::fs::write(&first_path, b"model\n").unwrap();
    repo.add("src/domain/model.rs", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "add model");

    let before_count = {
        let txn = repo.pristine.read_txn().unwrap();
        txn.get_view("dev").unwrap().unwrap().change_count
    };
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        let parent_inode = txn.get_inode("src/domain").unwrap().unwrap();
        txn.del_inode(parent_inode).unwrap();
        txn.commit().unwrap();
    }

    let second_path = temp_dir.path().join("src/domain/service.rs");
    std::fs::write(&second_path, b"service\n").unwrap();
    repo.add("src/domain/service.rs", TrackingOptions::default())
        .unwrap();
    let error = repo
        .record(
            ChangeHeader::new("must fail"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("no graph position")
            || error.to_string().contains("has no inode"),
        "unexpected error: {error}"
    );

    let after_count = {
        let txn = repo.pristine.read_txn().unwrap();
        txn.get_view("dev").unwrap().unwrap().change_count
    };
    assert_eq!(after_count, before_count);
}
