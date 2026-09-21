//! Regression tests for the view-switch file-loss bug.
//!
//! Bug (ANGS-A20 wireup): a materialization **name-conflict** made
//! `visible_file_paths` drop a path that WAS in the target view's inherited
//! state; `switch_view`'s Phase 2 then classified it as "old view only"
//! and **deleted the file from the working tree**. The deletion repeatedly
//! removed source files that the target view inherits (and left a stuck
//! "modified" state that blocked further switches).
//!
//! Root cause: `TREE` is single-valued per path, so when two inodes visibly
//! claim the same path (a name conflict), `visible_file_paths` only exposed
//! the single binding — whose introducing change may not be visible on the
//! target view even though the path IS in the target's inherited state.

use super::*;
use crate::apply::CrossViewInsertOptions;
use crate::record::RecordOptions;
use atomic_core::change::ChangeHeader;

fn record_all(repo: &Repository, message: &str) {
    let header = ChangeHeader::new(message);
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repo.record(header, options).unwrap();
}

/// Two views independently create `f.txt` at distinct inodes (so a
/// name-conflict is genuinely recorded on dev), then a child view is
/// created inheriting that conflicted lineage. Switching dev → child must
/// keep the file — it IS in the child's inherited state — and the conflict
/// must be surfaced, not silently turned into a deletion.
#[test]
fn switch_to_inheriting_child_keeps_name_conflicted_file() {
    let (temp_dir, mut repo) = create_temp_repo();

    // Seed unrelated base so the child view has something to inherit.
    let seed = temp_dir.path().join("seed.txt");
    std::fs::write(&seed, "seed\n").unwrap();
    repo.add("seed.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");

    repo.create_view_from("feature", "dev").unwrap();

    // feature independently creates f.txt with its own first line.
    let new_file = temp_dir.path().join("f.txt");
    repo.switch_view("feature").unwrap();
    std::fs::write(&new_file, "from-feature\nbody\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "feature creates f.txt");

    // dev independently creates f.txt with a DIFFERENT first line.
    repo.switch_view("dev").unwrap();
    std::fs::write(&new_file, "from-dev\nbody\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "dev creates f.txt");

    // Insert feature → dev: two inodes claim f.txt → real name conflict.
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    let on_disk = std::fs::read_to_string(&new_file).unwrap();
    assert!(
        on_disk.contains(">>>>>>>"),
        "precondition: expected name-conflict markers on disk, got:\n{on_disk}"
    );
    assert!(
        !repo.list_conflicts().unwrap().is_empty(),
        "precondition: conflict must be persisted on dev"
    );

    // Child view inheriting the conflicted dev lineage.
    repo.create_view_from("child", "dev").unwrap();

    // Switch dev → child. THE BUG: this deleted the file.
    repo.switch_view("child").unwrap();

    assert!(
        new_file.exists(),
        "switch deleted a name-conflicted file that is in the child's \
         inherited state"
    );

    // The conflict must be KEPT and REPORTED on the child, not silently
    // materialized as a clean single-version file. (Per-view persistence of
    // the CONFLICTS table entry is a separate concern; the switch contract
    // is: keep the file and keep its conflict markers.)
    let on_disk = std::fs::read_to_string(&new_file).unwrap();
    assert!(
        on_disk.contains(">>>>>>>"),
        "name-conflicted file must keep its conflict markers on the child, \
         got:\n{on_disk}"
    );
}
