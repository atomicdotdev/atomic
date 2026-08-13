//! Integration test for the order-invariant view [`SetId`]
//! (`Repository::view_set_id`, intent ATOM::norman::10, AC-3).
//!
//! Proves the payoff: splitting changes out of a view and reinserting them
//! returns the view to the SAME `SetId` it started with (identity round-trips
//! home), while the order-sensitive `Merkle` state legitimately differs.

use std::fs;
use std::path::{Path, PathBuf};

use atomic_core::change::{Author, ChangeHeader};
use atomic_core::types::{Hash, SetId};
use atomic_repository::{InsertOptions, RecordOptions, Repository, SplitOptions};
use tempfile::TempDir;

fn init_repo() -> (Repository, TempDir, PathBuf) {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().to_path_buf();
    let repo = Repository::init(&path).expect("init repository");
    (repo, temp, path)
}

fn write_and_add(repo: &Repository, root: &Path, name: &str, content: &str) {
    fs::write(root.join(name), content).expect("write file");
    repo.add(name, Default::default()).expect("add file");
}

fn record(repo: &Repository, message: &str) -> Hash {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    *repo
        .record(header, RecordOptions::default())
        .expect("record")
        .hash()
}

#[test]
fn view_set_id_round_trips_home_after_split_and_reinsert() {
    let (mut repo, _temp, root) = init_repo();
    let dev = repo.current_view().to_string();

    // Three independent changes (distinct files → no dependencies), so the
    // middle one can be split out and reinserted at the tail, reordering the
    // log without violating any dependency.
    write_and_add(&repo, &root, "a.txt", "alpha\n");
    let _c1 = record(&repo, "add a");
    write_and_add(&repo, &root, "b.txt", "bravo\n");
    let c2 = record(&repo, "add b");
    write_and_add(&repo, &root, "c.txt", "charlie\n");
    let _c3 = record(&repo, "add c");

    let set_before = repo.view_set_id(&dev).expect("set id before");
    let merkle_before = repo.get_view_info(&dev).expect("info before").state;
    assert_ne!(
        set_before,
        SetId::ZERO,
        "non-empty view has a non-zero SetId"
    );

    // Split the middle change out into a draft. dev is now {a, c}.
    repo.split_view(SplitOptions::new("escape", vec![c2]))
        .expect("split c2 into escape");

    let set_after_split = repo.view_set_id(&dev).expect("set id after split");
    assert_ne!(
        set_after_split, set_before,
        "removing a change must change the view's SetId"
    );

    // Reinsert the split-out change back into dev. Its edges are already in the
    // global graph, so this is a metadata append at the tail: dev's log becomes
    // {a, c, b} — the same SET as before, in a DIFFERENT order.
    repo.insert_change(&c2, InsertOptions::default().view(&dev))
        .expect("reinsert c2 into dev");

    let set_after = repo.view_set_id(&dev).expect("set id after reinsert");
    let merkle_after = repo.get_view_info(&dev).expect("info after").state;

    // The order-invariant identity returns home...
    assert_eq!(
        set_after, set_before,
        "SetId must round-trip home: same set of changes ⇒ same SetId"
    );
    // ...while the order-sensitive Merkle legitimately differs (reordered log).
    assert_ne!(
        merkle_after, merkle_before,
        "Merkle is order-sensitive and must differ after reordering the log"
    );
}
