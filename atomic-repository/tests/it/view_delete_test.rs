//! Last-reference analysis for view deletion.

use atomic_core::pristine::{GraphTxnT, MutTxnT, ViewScope, ViewTxnT};
use atomic_core::types::Hash;
use atomic_repository::Repository;

#[test]
fn deletion_preview_excludes_inherited_shared_and_transitively_retained_changes() {
    let temp = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(temp.path()).unwrap();
    let inherited = Hash::of(b"inherited");
    let unique = Hash::of(b"unique");
    let shared = Hash::of(b"shared");
    let dependency = Hash::of(b"dependency");
    let intermediate = Hash::of(b"intermediate");
    let dependent = Hash::of(b"dependent");
    let mut txn = repo.pristine().write_txn().unwrap();
    let mut parent = txn.get_view(repo.current_view()).unwrap().unwrap();
    let mut draft = txn
        .create_view("wip", ViewScope::Draft, Some(parent.id))
        .unwrap();
    let mut other = txn
        .create_view("other", ViewScope::Draft, Some(parent.id))
        .unwrap();

    for hash in [
        inherited,
        unique,
        shared,
        dependency,
        intermediate,
        dependent,
    ] {
        let id = txn.register_change(&hash).unwrap();
        txn.put_change_deps(id, &[]).unwrap();
    }
    txn.put_change(
        &mut parent,
        txn.get_internal(&inherited).unwrap().unwrap(),
        &inherited,
    )
    .unwrap();
    for hash in [unique, shared, dependency] {
        txn.put_change(&mut draft, txn.get_internal(&hash).unwrap().unwrap(), &hash)
            .unwrap();
    }
    for hash in [shared, dependent] {
        txn.put_change(&mut other, txn.get_internal(&hash).unwrap().unwrap(), &hash)
            .unwrap();
    }
    txn.put_change_deps(
        txn.get_internal(&dependent).unwrap().unwrap(),
        &[intermediate],
    )
    .unwrap();
    txn.put_change_deps(
        txn.get_internal(&intermediate).unwrap().unwrap(),
        &[dependency],
    )
    .unwrap();
    txn.update_view(&parent).unwrap();
    txn.update_view(&draft).unwrap();
    txn.update_view(&other).unwrap();
    txn.commit().unwrap();

    assert_eq!(repo.view_deletion_orphans("wip").unwrap(), vec![unique]);
    assert!(repo.view_exists("wip").unwrap(), "preview must not mutate");
    repo.delete_view("wip").unwrap();
    let txn = repo.pristine().read_txn().unwrap();
    assert!(
        txn.get_internal(&unique).unwrap().is_some(),
        "deletion retains change data"
    );
    assert_eq!(
        repo.views_containing_change(&inherited).unwrap(),
        vec![repo.current_view()]
    );
    assert_eq!(
        repo.views_containing_change(&shared).unwrap(),
        vec!["other"]
    );
}

#[test]
fn deletion_preview_of_an_empty_draft_has_no_orphans() {
    let temp = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(temp.path()).unwrap();
    repo.create_view("empty").unwrap();
    assert!(repo.view_deletion_orphans("empty").unwrap().is_empty());
}
