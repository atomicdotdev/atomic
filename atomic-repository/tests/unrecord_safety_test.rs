//! Dependency validation uses stored changes when legacy repositories lack
//! an index, and follows dependencies beyond direct view membership.
use atomic_core::change::{Change, ChangeHeader};
use atomic_core::pristine::{GraphTxnT, MutTxnT, ViewTxnT};
use atomic_core::types::Hash;
use atomic_repository::history::HistoryOptions;
use atomic_repository::unrecord::UnrecordOptions;
use atomic_repository::Repository;
use tempfile::TempDir;

fn save(repo: &Repository, message: &str, deps: Vec<Hash>) -> Hash {
    repo.save_change(&Change::new(
        ChangeHeader::new(message),
        vec![],
        vec![],
        deps,
    ))
    .unwrap()
}

// Deliberately bypass insert_change: this represents a legacy repository
// that has view membership but no dependency index for these changes.
fn add_legacy_member(repo: &Repository, hash: Hash) {
    let mut txn = repo.pristine().write_txn().unwrap();
    let mut view = txn.get_view(repo.current_view()).unwrap().unwrap();
    let id = txn.register_change(&hash).unwrap();
    assert!(!txn.is_change_deps_indexed(id).unwrap());
    txn.put_change(&mut view, id, &hash).unwrap();
    txn.update_view(&view).unwrap();
    txn.commit().unwrap();
}

fn history(repo: &Repository) -> Vec<Hash> {
    repo.log(HistoryOptions::default())
        .unwrap()
        .into_iter()
        .map(|e| e.hash)
        .collect()
}

#[test]
fn unindexed_dependent_blocks_removal_and_dry_run() {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let a = save(&repo, "a", vec![]);
    let b = save(&repo, "b", vec![a]);
    add_legacy_member(&repo, a);
    add_legacy_member(&repo, b);
    for options in [UnrecordOptions::dry_run(), UnrecordOptions::new()] {
        let err = repo.unrecord(&a, options).unwrap_err();
        assert!(err.to_string().contains("depends on it"), "{err}");
        assert_eq!(history(&repo), vec![a, b]);
    }
    repo.unrecord(&b, UnrecordOptions::new()).unwrap();
    repo.unrecord(&a, UnrecordOptions::new()).unwrap();
    assert!(history(&repo).is_empty());
    assert!(repo.has_change(&a));
    assert!(repo.has_change(&b));
}

#[test]
fn transitive_dependency_outside_direct_membership_blocks_removal() {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let a = save(&repo, "a", vec![]);
    let b = save(&repo, "b", vec![a]);
    let c = save(&repo, "c", vec![b]);
    add_legacy_member(&repo, a);
    // b is visible through c's dependency closure, without its own view row.
    add_legacy_member(&repo, c);
    for options in [UnrecordOptions::dry_run(), UnrecordOptions::new()] {
        let err = repo.unrecord(&a, options).unwrap_err();
        assert!(err.to_string().contains("depends on it"), "{err}");
        assert_eq!(history(&repo), vec![a, c]);
    }
}

#[test]
fn unverifiable_dependencies_fail_closed() {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let a = save(&repo, "a", vec![]);
    let b = save(&repo, "b", vec![Hash::of(b"missing object")]);
    add_legacy_member(&repo, a);
    add_legacy_member(&repo, b);
    for options in [UnrecordOptions::dry_run(), UnrecordOptions::new()] {
        assert!(repo.unrecord(&a, options).is_err());
        assert_eq!(history(&repo), vec![a, b]);
    }
}

#[test]
fn inherited_dependent_also_blocks_removal() {
    let dir = TempDir::new().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let parent = repo.current_view().to_string();
    repo.create_view_from("child", &parent).unwrap();
    repo.switch_view("child").unwrap();
    let a = save(&repo, "child change", vec![]);
    add_legacy_member(&repo, a);
    // The shared parent advances after the draft fork. Its new change is
    // inherited, even though it has no row in the child's own history.
    repo.switch_view(&parent).unwrap();
    let b = save(&repo, "parent dependent", vec![a]);
    add_legacy_member(&repo, b);
    repo.switch_view("child").unwrap();
    for options in [UnrecordOptions::dry_run(), UnrecordOptions::new()] {
        let err = repo.unrecord(&a, options).unwrap_err();
        assert!(err.to_string().contains("depends on it"), "{err}");
        assert_eq!(history(&repo), vec![a]);
    }
}

#[test]
fn nonexistent_view_is_not_created_by_unrecord() {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let hash = save(&repo, "a", vec![]);
    add_legacy_member(&repo, hash);
    assert!(repo
        .unrecord(&hash, UnrecordOptions::new().view("missing"))
        .is_err());
    assert!(repo
        .pristine()
        .read_txn()
        .unwrap()
        .get_view("missing")
        .unwrap()
        .is_none());
    assert_eq!(history(&repo), vec![hash]);
}
