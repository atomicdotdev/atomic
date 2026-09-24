//! Integration tests for persistent conflict surfacing.
//!
//! Validates Phases 1–2 of the merge-conflict plan:
//!   1. An `insert` that produces conflict markers persists conflict state,
//!      and `atomic status` reports the file as `Conflicted`.
//!   2. `record` refuses to capture a file that still contains conflict
//!      markers, and a clean record of a resolved file clears the state.

use super::*;
use crate::apply::CrossViewInsertOptions;
use crate::record::{RecordError, RecordOptions};
use crate::status::{FileStatus, StatusOptions};
use crate::unrecord::UnrecordOptions;
use atomic_core::change::{ChangeHeader, GraphOp};
use atomic_core::pristine::{PathClaimTxnT, TreeTxnT, ViewTxnT};

fn record_all(repo: &Repository, message: &str) -> Result<RecordOutcome, RecordError> {
    let header = ChangeHeader::new(message);
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repo.record(repo.require_working_copy_id().unwrap(), header, options)
}

/// Build a repo where feature and dev insert different lines at the same
/// position, then insert feature → dev and materialize on dev. Returns the
/// on-disk path of the conflicted file.
fn make_conflicted_repo() -> (TempDir, TestRepository, std::path::PathBuf) {
    let (temp_dir, mut repo) = create_temp_repo();
    let file = temp_dir.path().join("f.txt");

    std::fs::write(&file, "line1\nline2\nline3\nline4\nline5\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("feature", "dev").unwrap();

    // feature: insert AAA-inserted after line1
    repo.switch_view("feature").unwrap();
    std::fs::write(
        &file,
        "line1\nAAA-inserted $Id$\nline2\nline3\nline4\nline5\n",
    )
    .unwrap();
    record_all(&repo, "edit A").unwrap();

    // dev: insert BBB-inserted after line1 (same position → conflict)
    repo.switch_view("dev").unwrap();
    std::fs::write(
        &file,
        "line1\nBBB-inserted $Id$\nline2\nline3\nline4\nline5\n",
    )
    .unwrap();
    record_all(&repo, "edit B").unwrap();

    std::fs::write(
        temp_dir.path().join(".gitattributes"),
        "f.txt ident filter=reject-conflict\n",
    )
    .unwrap();
    std::fs::write(
        temp_dir.path().join(".atomic/config.toml"),
        "[view]\ndefault = \"dev\"\n\n[filters.drivers.reject-conflict]\nclean = \"cat\"\nsmudge = \"exit 19\"\nrequired = true\n",
    )
    .unwrap();
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    (temp_dir, repo, file)
}

fn conflicted_entry_paths(repo: &Repository) -> Vec<String> {
    let status = repo
        .status(
            repo.require_working_copy_id().unwrap(),
            StatusOptions::default(),
        )
        .unwrap();
    status
        .entries()
        .iter()
        .filter(|e| e.status() == FileStatus::Conflicted)
        .map(|e| e.path().to_string_lossy().to_string())
        .collect()
}

fn persisted_conflict_count(repo: &Repository, view: &str) -> usize {
    let txn = repo.pristine.read_txn().unwrap();
    let v = txn.get_view(view).unwrap().unwrap();
    txn.iter_conflicts(v.id).unwrap().len()
}

#[test]
fn test_conflict_is_persisted_and_surfaced_in_status() {
    let (_temp, repo, file) = make_conflicted_repo();

    // The materialized file must actually carry markers for this test to
    // be meaningful (otherwise the merge auto-resolved and there is no
    // conflict to surface).
    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(
        on_disk.contains(">>>>>>>"),
        "expected conflict markers on disk, got:\n{on_disk}"
    );
    assert_eq!(on_disk.matches("$Id$").count(), 2);
    assert!(!on_disk.contains("$Id:"));

    // Persisted in the CONFLICTS table for dev.
    assert!(
        persisted_conflict_count(&repo, "dev") >= 1,
        "conflict should be persisted for dev"
    );

    // Surfaced by status.
    let conflicted = conflicted_entry_paths(&repo);
    assert!(
        conflicted.iter().any(|p| p == "f.txt"),
        "status should report f.txt as Conflicted, got {conflicted:?}"
    );
}

#[test]
fn test_list_conflicts_reports_current_view_details() {
    let (_temp, repo, file) = make_conflicted_repo();

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(on_disk.contains(">>>>>>>"), "precondition: markers on disk");

    let conflicts = repo.list_conflicts().unwrap();
    assert_eq!(conflicts.len(), 1, "expected exactly one conflicted file");
    let (path, records) = &conflicts[0];
    assert_eq!(path, "f.txt");
    assert!(
        !records.is_empty(),
        "file should carry at least one conflict"
    );
    assert!(
        records[0].line.is_some(),
        "conflict should record a line number"
    );

    // Once resolved (markers removed) the file drops out of list_conflicts,
    // honouring the honesty invariant even before the resolution is recorded.
    std::fs::write(
        &file,
        "line1\nAAA-inserted\nBBB-inserted\nline2\nline3\nline4\nline5\n",
    )
    .unwrap();
    assert!(
        repo.list_conflicts().unwrap().is_empty(),
        "resolved file should not be listed as conflicted"
    );
}

/// Rubric A12 (ATOM::30): two views independently CREATE the same path as
/// distinct inodes. Because `TREE` is single-valued, the later recorder used to
/// shadow the first, and inserting one side's create into the other silently
/// materialized only one inode's content with a clean `status`. The fix makes
/// materialize walk `REV_TREE`, detect that two inodes are visible+alive at the
/// path, and render a name conflict wrapping BOTH bodies — surfaced honestly.
#[test]
fn test_name_conflict_same_path_creates_are_surfaced() {
    let (temp_dir, mut repo) = create_temp_repo();

    // Seed the repo with an unrelated base change.
    let seed = temp_dir.path().join("seed.txt");
    std::fs::write(&seed, "seed\n").unwrap();
    repo.add("seed.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("feature", "dev").unwrap();

    let new_file = temp_dir.path().join("new.txt");

    // feature independently creates new.txt.
    repo.switch_view("feature").unwrap();
    std::fs::write(&new_file, "from-feature $Id$\n").unwrap();
    repo.add("new.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "feature creates new.txt").unwrap();

    // dev independently creates new.txt with different content (distinct inode).
    repo.switch_view("dev").unwrap();
    std::fs::write(&new_file, "from-base $Id$\n").unwrap();
    repo.add("new.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "dev creates new.txt").unwrap();

    // Insert feature's create into dev: now two inodes claim new.txt on dev.
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    assert!(
        conflicted_entry_paths(&repo)
            .iter()
            .any(|path| path == "new.txt"),
        "status must surface PATH_CLAIMS conflict before materialization"
    );

    // Synthesized marker files are evidence, not repository objects. They must
    // bypass both ident expansion and external smudge drivers.
    std::fs::write(
        temp_dir.path().join(".gitattributes"),
        "new.txt ident filter=reject-conflict\n",
    )
    .unwrap();
    std::fs::write(
        temp_dir.path().join(".atomic/config.toml"),
        "[view]\ndefault = \"dev\"\n\n[filters.drivers.reject-conflict]\nsmudge = \"exit 19\"\nrequired = true\n",
    )
    .unwrap();
    repo.materialize().unwrap();

    // Both bodies survive, wrapped in a name-conflict block (no silent loss).
    let on_disk = std::fs::read_to_string(&new_file).unwrap();
    assert!(
        on_disk.contains(">>>>>>>"),
        "name conflict must surface markers, got:\n{on_disk}"
    );
    assert!(
        on_disk.contains("from-feature"),
        "feature's create must be preserved:\n{on_disk}"
    );
    assert!(
        on_disk.contains("from-base"),
        "dev's create must be preserved:\n{on_disk}"
    );
    assert_eq!(on_disk.matches("$Id$").count(), 2);
    assert!(!on_disk.contains("$Id:"));
    assert_eq!(
        on_disk.matches("from-feature").count(),
        1,
        "feature side must appear exactly once:\n{on_disk}"
    );
    assert_eq!(
        on_disk.matches("from-base").count(),
        1,
        "dev side must appear exactly once:\n{on_disk}"
    );

    std::fs::remove_file(&new_file).unwrap();
    repo.materialize_sequential().unwrap();
    assert_eq!(
        std::fs::read_to_string(&new_file).unwrap(),
        on_disk,
        "sequential and parallel materialization must render identical sides"
    );
    assert!(
        repo.get_file_content("new.txt").is_err(),
        "content APIs must not choose one claimant"
    );
    {
        let txn = repo.pristine.read_txn().unwrap();
        assert_eq!(txn.get_inode("new.txt").unwrap(), None);
        txn.validate_tree_bijection().unwrap();
    }

    // Honesty: persisted for dev AND surfaced by status.
    assert!(
        persisted_conflict_count(&repo, "dev") >= 1,
        "name conflict should be persisted for dev"
    );
    let conflicted = conflicted_entry_paths(&repo);
    assert!(
        conflicted.iter().any(|p| p == "new.txt"),
        "status should report new.txt as Conflicted, got {conflicted:?}"
    );

    // The legacy primary/reverse-only representation must survive a switch
    // round-trip without collapsing either visible claimant.
    repo.switch_view("feature").unwrap();
    std::fs::remove_file(&new_file).unwrap();
    repo.materialize().unwrap();
    let feature_replay = std::fs::read_to_string(&new_file).unwrap();
    assert!(feature_replay.contains(">>>>>>>"));
    assert!(feature_replay.contains("from-feature"));
    assert!(feature_replay.contains("from-base"));

    repo.switch_view("dev").unwrap();
    std::fs::remove_file(&new_file).unwrap();
    repo.materialize().unwrap();
    let replayed = std::fs::read_to_string(&new_file).unwrap();
    assert!(replayed.contains(">>>>>>>"));
    assert!(replayed.contains("from-feature"));
    assert!(replayed.contains("from-base"));
}

#[test]
fn test_recorded_name_resolution_emits_real_solve_with_complete_dependencies() {
    let (temp_dir, mut repo) = create_temp_repo();
    let directory = temp_dir.path().join("nested");
    std::fs::create_dir(&directory).unwrap();
    repo.add_directory("nested", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "base directory").unwrap();
    repo.create_view_from("feature", "dev").unwrap();

    let path = directory.join("same.txt");
    repo.switch_view("feature").unwrap();
    std::fs::write(&path, "feature\n").unwrap();
    repo.add("nested/same.txt", TrackingOptions::default())
        .unwrap();
    let feature = record_all(&repo, "feature create").unwrap();
    let feature_inode = repo.get_file_inode("nested/same.txt").unwrap().unwrap();

    repo.switch_view("dev").unwrap();
    std::fs::write(&path, "dev\n").unwrap();
    repo.add("nested/same.txt", TrackingOptions::default())
        .unwrap();
    let dev = record_all(&repo, "dev create").unwrap();
    let dev_inode = repo.get_file_inode("nested/same.txt").unwrap().unwrap();
    assert_ne!(feature_inode, dev_inode);
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();
    assert!(std::fs::read_to_string(&path).unwrap().contains(">>>>>>>"));

    std::fs::write(&path, "feature\n").unwrap();
    let resolution = record_all(&repo, "resolve name").unwrap();
    assert!(resolution.errors().is_empty());
    assert!(resolution.was_applied());
    let solves: Vec<_> = resolution
        .change()
        .hunks()
        .iter()
        .filter_map(|operation| match operation {
            GraphOp::SolveNameConflict { name, path } => Some((name, path)),
            _ => None,
        })
        .collect();
    assert_eq!(solves.len(), 1);
    assert_eq!(solves[0].1, "nested/same.txt");
    assert!(!solves[0].0.edges.is_empty());
    assert!(resolution.change().dependencies().contains(feature.hash()));
    assert!(resolution.change().dependencies().contains(dev.hash()));

    repo.materialize().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"feature\n");
    assert!(repo.list_conflicts().unwrap().is_empty());

    let resolution_hash = *resolution.hash();
    repo.unrecord(&resolution_hash, UnrecordOptions::default())
        .unwrap();
    repo.materialize().unwrap();
    let reopened_conflict = std::fs::read_to_string(&path).unwrap();
    assert!(reopened_conflict.contains("feature\n"));
    assert!(reopened_conflict.contains("dev\n"));
    assert!(reopened_conflict.contains(">>>>>>>"));

    repo.reinsert_change(&resolution_hash, None).unwrap();
    repo.materialize().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"feature\n");
    drop(repo);
    let reopened = Repository::open(temp_dir.path()).unwrap();
    reopened
        .materialize(reopened.require_working_copy_id().unwrap())
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"feature\n");
}

#[test]
fn test_directory_name_conflict_is_typed_without_tree_winner() {
    let (temp, mut repo) = create_temp_repo();
    let seed = temp.path().join("seed.txt");
    std::fs::write(&seed, "seed\n").unwrap();
    repo.add("seed.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    repo.create_view_from("feature-dir", "dev").unwrap();

    let directory = temp.path().join("same-dir");
    repo.switch_view("feature-dir").unwrap();
    std::fs::create_dir(&directory).unwrap();
    repo.add_directory("same-dir", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "feature directory").unwrap();

    repo.switch_view("dev").unwrap();
    if directory.exists() {
        std::fs::remove_dir(&directory).unwrap();
    }
    std::fs::create_dir(&directory).unwrap();
    repo.add_directory("same-dir", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "dev directory").unwrap();
    repo.insert_from_view(CrossViewInsertOptions::new("feature-dir", "dev"))
        .unwrap();

    repo.materialize_sequential().unwrap();
    assert!(directory.is_dir());
    let status = repo.status(StatusOptions::default()).unwrap();
    assert!(status.entries().iter().any(|entry| {
        entry.path().to_string_lossy() == "same-dir" && entry.status() == FileStatus::Conflicted
    }));
    assert!(repo
        .list_conflicts()
        .unwrap()
        .iter()
        .any(|(path, records)| {
            path == "same-dir"
                && records
                    .iter()
                    .any(|record| record.kind == atomic_core::pristine::StoredConflictKind::Name)
        }));
    let txn = repo.pristine.read_txn().unwrap();
    assert_eq!(txn.get_inode("same-dir").unwrap(), None);
    txn.validate_tree_bijection().unwrap();
}

/// Guard against false positives: an ordinary single-create file (only one
/// inode ever claims the path) must never be flagged as a name conflict.
#[test]
fn test_single_create_is_not_a_name_conflict() {
    let (temp_dir, repo) = create_temp_repo();

    let file = temp_dir.path().join("solo.txt");
    std::fs::write(&file, "only one\n").unwrap();
    repo.add("solo.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "create solo.txt").unwrap();
    repo.materialize().unwrap();

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(
        !on_disk.contains(">>>>>>>"),
        "single-create file must not carry conflict markers, got:\n{on_disk}"
    );
    assert_eq!(on_disk, "only one\n", "content must be byte-exact");
    let conflicted = conflicted_entry_paths(&repo);
    assert!(
        conflicted.is_empty(),
        "no file should be Conflicted, got {conflicted:?}"
    );
}

/// Rubric A15 (ATOM::31): a binary file modification must record as a whole-
/// file replace that DELETES the base. It used to route through
/// `globalize_replace`'s pure-insertion branch (empty `deleted_lines`), never
/// deleting the base — so a single edit shadowed it silently and a concurrent
/// merge leaked the base bytes OUTSIDE the conflict markers.
#[test]
fn test_binary_edit_deletes_base_and_roundtrips() {
    let (temp_dir, repo) = create_temp_repo();
    let file = temp_dir.path().join("b.bin");

    // Base binary content (null byte → detected as binary).
    std::fs::write(&file, b"\x00\x01\x02BASE\x03\x04\n").unwrap();
    repo.add("b.bin", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    // Modify it, record, and re-materialize from the graph.
    std::fs::write(&file, b"\x00\x01\x02NEWNEW\x03\x04\n").unwrap();
    record_all(&repo, "edit").unwrap();
    repo.materialize().unwrap();

    // The graph must round-trip to EXACTLY the new bytes — no base residue.
    let on_disk = std::fs::read(&file).unwrap();
    assert_eq!(
        on_disk, b"\x00\x01\x02NEWNEW\x03\x04\n",
        "binary edit must round-trip byte-exact with the base deleted, got {on_disk:?}"
    );
}

/// A15 under a merge: two views edit the same binary file at the same
/// position. The conflict must surface, and the body must contain ONLY the two
/// edited versions — the base bytes must not leak outside the markers.
#[test]
fn test_binary_concurrent_edit_conflict_has_no_base_residue() {
    let (temp_dir, mut repo) = create_temp_repo();
    let file = temp_dir.path().join("b.bin");

    std::fs::write(&file, b"\x00\x01\x02BASE\x03\x04\n").unwrap();
    repo.add("b.bin", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("feature", "dev").unwrap();

    repo.switch_view("feature").unwrap();
    std::fs::write(&file, b"\x00\x01\x02AAAA\x03\x04\n").unwrap();
    record_all(&repo, "edit A").unwrap();

    repo.switch_view("dev").unwrap();
    std::fs::write(&file, b"\x00\x01\x02BBBB\x03\x04\n").unwrap();
    record_all(&repo, "edit B").unwrap();

    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    let on_disk = std::fs::read(&file).unwrap();
    // Both edited versions survive.
    assert!(
        on_disk.windows(4).any(|w| w == b"AAAA"),
        "side A must be present"
    );
    assert!(
        on_disk.windows(4).any(|w| w == b"BBBB"),
        "side B must be present"
    );
    // The base must NOT leak anywhere in the conflict body.
    assert!(
        !on_disk.windows(4).any(|w| w == b"BASE"),
        "base bytes must not leak outside the conflict markers, got {on_disk:?}"
    );
    // Honesty: surfaced as a conflict.
    let conflicted = conflicted_entry_paths(&repo);
    assert!(
        conflicted.iter().any(|p| p == "b.bin"),
        "status should report b.bin Conflicted, got {conflicted:?}"
    );
}

#[test]
fn test_record_refuses_markers_then_clears_on_resolution() {
    let (_temp, repo, file) = make_conflicted_repo();

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(on_disk.contains(">>>>>>>"), "precondition: markers on disk");

    // record must refuse while markers are present.
    match record_all(&repo, "attempt to record conflict") {
        Err(RecordError::ConflictMarkersPresent { path, .. }) => {
            assert_eq!(path, "f.txt");
        }
        other => panic!("expected ConflictMarkersPresent, got {other:?}"),
    }

    // Resolve: write clean content (both edits, no markers) and record.
    std::fs::write(
        &file,
        "line1\nAAA-inserted\nBBB-inserted\nline2\nline3\nline4\nline5\n",
    )
    .unwrap();
    record_all(&repo, "resolve conflict").expect("clean record should succeed");

    // The persisted conflict for dev is cleared, and status is clean of it.
    assert_eq!(
        persisted_conflict_count(&repo, "dev"),
        0,
        "recording the resolved file should clear its conflict entry"
    );
    let conflicted = conflicted_entry_paths(&repo);
    assert!(
        !conflicted.iter().any(|p| p == "f.txt"),
        "f.txt should no longer be Conflicted after resolution, got {conflicted:?}"
    );
}

/// Marker-shaped bytes in a file's *source* are content, not a conflict.
///
/// Documentation and harness fixtures legitimately contain conflict-marker
/// examples. A clean materialize must not persist an order conflict for them,
/// because that would make `status` report `Conflicted` forever and make
/// `record` refuse every turn. Detection must stay driven by the output
/// layer's own conflict regions, so a genuine merge conflict is still caught
/// (see `test_conflict_is_persisted_and_surfaced_in_status`).
#[test]
fn marker_shaped_source_content_is_not_a_persisted_conflict() {
    let (temp_dir, repo) = create_temp_repo();
    let file = temp_dir.path().join("fixture.md");
    // Build the marker-shaped content from an array so that no line of *this
    // Rust source* begins with a marker (which would make this very test file
    // a marker-shaped fixture).
    let fixture = [
        "Conflict marker example:",
        ">>>>>>> 1",
        "left side",
        "======= 1",
        "right side",
        "<<<<<<< 1",
        "",
    ]
    .join("\n");
    std::fs::write(&file, fixture.as_bytes()).unwrap();
    repo.add("fixture.md", TrackingOptions::default()).unwrap();
    // The fixture's bytes are intentional marker examples, so recording it
    // is the explicit `allow_conflict_markers` workflow.
    let header = ChangeHeader::new("add marker-shaped fixture");
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true)
        .allow_conflict_markers(true);
    repo.record(header, options).unwrap();

    repo.materialize().unwrap();

    assert_eq!(
        persisted_conflict_count(&repo, "dev"),
        0,
        "marker-shaped source content must not be persisted as an order conflict"
    );
    assert!(
        conflicted_entry_paths(&repo).is_empty(),
        "status must not report the marker fixture as Conflicted"
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        fixture.as_bytes(),
        "the fixture bytes must be preserved verbatim"
    );
}

/// Build a two-incarnation name conflict at `new.txt` on `dev` by creating the
/// same path independently on `feature` and `dev`, then inserting feature.
fn make_two_incarnation_name_conflict() -> (TempDir, TestRepository, std::path::PathBuf) {
    let (temp_dir, mut repo) = create_temp_repo();
    let seed = temp_dir.path().join("seed.txt");
    std::fs::write(&seed, "seed\n").unwrap();
    repo.add("seed.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("feature", "dev").unwrap();
    let new_file = temp_dir.path().join("new.txt");

    repo.switch_view("feature").unwrap();
    std::fs::write(&new_file, "from-feature\n").unwrap();
    repo.add("new.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "feature creates new.txt").unwrap();

    repo.switch_view("dev").unwrap();
    std::fs::write(&new_file, "from-base\n").unwrap();
    repo.add("new.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "dev creates new.txt").unwrap();

    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    (temp_dir, repo, new_file)
}

/// A name conflict whose working tree already holds exactly one incarnation's
/// bytes must be resolved by a normal managed record even when no conflict row
/// was ever persisted. "Content-clean" must not mean the two live claims are
/// silently ignored; the winning side is chosen by byte equality (never by
/// timestamp) and the losing claims are superseded, not erased.
#[test]
fn content_clean_name_conflict_is_resolved_by_record() {
    let (_temp, repo, new_file) = make_two_incarnation_name_conflict();

    // No materialize ran, so there is no persisted conflict row; the working
    // tree simply keeps one side's bytes.
    std::fs::write(&new_file, "from-base\n").unwrap();
    let on_disk = std::fs::read_to_string(&new_file).unwrap();
    assert!(
        on_disk.contains("from-base"),
        "one side's bytes are on disk"
    );
    assert!(!on_disk.contains("<<<<<<<"), "no markers remain on disk");
    assert!(
        repo.get_file_content("new.txt").is_err(),
        "the ambiguity is still unresolved before the record"
    );

    record_all(&repo, "resolve clean name conflict").unwrap();

    assert_eq!(
        repo.get_file_content("new.txt").unwrap().as_deref(),
        Some(b"from-base\n".as_slice()),
        "the winner is the side whose bytes the working tree held"
    );
    assert!(
        !conflicted_entry_paths(&repo).iter().any(|p| p == "new.txt"),
        "the name conflict must be cleared after the record"
    );

    // History is retained: both incarnations' claim rows still exist (the loser
    // is superseded, not deleted).
    let txn = repo.pristine.read_txn().unwrap();
    let claim_rows = txn
        .iter_path_claims()
        .unwrap()
        .into_iter()
        .filter(|entry| entry.path == "new.txt")
        .count();
    assert!(
        claim_rows >= 2,
        "losing claims must remain as history, got {claim_rows}"
    );
}

/// Resolving the conflict on `dev` must leave every other path untouched and
/// keep both incarnations' history; only the intended path is affected.
#[test]
fn resolving_a_name_conflict_leaves_other_paths_untouched() {
    let (_temp, repo, new_file) = make_two_incarnation_name_conflict();

    std::fs::write(&new_file, "from-base\n").unwrap();
    record_all(&repo, "resolve clean name conflict on dev").unwrap();

    // The control path is untouched, and no unrelated path became conflicted.
    assert_eq!(
        repo.get_file_content("seed.txt").unwrap().as_deref(),
        Some(b"seed\n".as_slice()),
        "an unrelated tracked path is unchanged"
    );
    let conflicted = conflicted_entry_paths(&repo);
    assert!(
        !conflicted.iter().any(|p| p == "seed.txt"),
        "an unrelated path is not conflicted: {conflicted:?}"
    );
}

/// A third value that matches no incarnation (or more than one) must be
/// refused, never silently resolved to a lineage.
#[test]
fn third_value_name_conflict_is_refused_not_chosen() {
    let (_temp, repo, new_file) = make_two_incarnation_name_conflict();

    std::fs::write(&new_file, "third-value\n").unwrap();
    let outcome = record_all(&repo, "third value must not resolve");
    // Either the record refuses with NothingToRecord or it reports the path as
    // an error — in both cases the ambiguity must remain.
    if let Ok(outcome) = outcome {
        assert!(
            !outcome.recorded_files().iter().any(|p| p == "new.txt"),
            "a third value must not be recorded as a winner"
        );
    }
    assert!(
        repo.get_file_content("new.txt").is_err(),
        "a third value must leave the name conflict unresolved"
    );
}

/// Review ::15 blocker support (user decision "Preserve current files"):
/// the explicit `resolve_name_conflicts` resolution resolves the survivor's
/// identity through the RAW canonical TREE binding for the path. With three
/// incarnations where two are byte-equal, the TREE-bound (non-lowest)
/// claimant survives — the identity is the documented binding, not an
/// arbitrary lowest-inode pick (review ::26 N1).
///
/// Failing-before: the old selection fell back to the lowest inode among
/// the byte-equal sides (the reviewer's probe: first=Inode(2) second=Inode(3)
/// survivor=Inode(2) without consulting TREE). Passing-after: the survivor
/// is the TREE-bound incarnation.
#[test]
fn explicit_preserve_content_resolution_selects_tree_bound_incarnation() {
    let (temp_dir, mut repo) = create_temp_repo();
    let file = temp_dir.path().join("same.txt");

    // Two-FileAdd shape: the file is untracked at base; each branch adds its
    // own incarnation with IDENTICAL bytes (the multi-match shape).
    std::fs::write(temp_dir.path().join("seed.txt"), "seed\n").unwrap();
    repo.add("seed.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("fork1", "dev").unwrap();
    repo.switch_view("fork1").unwrap();
    std::fs::write(&file, "shared bytes\n").unwrap();
    repo.add("same.txt", TrackingOptions::default()).unwrap();
    let fork1 = record_all(&repo, "fork1 create").unwrap();
    let fork1_inode = repo.get_file_inode("same.txt").unwrap().unwrap();

    repo.create_view_from("fork2", "dev").unwrap();
    repo.switch_view("fork2").unwrap();
    std::fs::write(&file, "shared bytes\n").unwrap();
    repo.add("same.txt", TrackingOptions::default()).unwrap();
    let fork2 = record_all(&repo, "fork2 create").unwrap();
    let fork2_inode = repo.get_file_inode("same.txt").unwrap().unwrap();
    assert_ne!(fork1_inode, fork2_inode);

    // Union both incarnations into dev. The multi-claimant deferred-tree
    // replay UNSETS the raw TREE row for the conflicted path (the probe:
    // two competing Sets → None) — the same state as the reviewer's N1
    // probe and as the live graph's conflicted paths BEFORE a later normal
    // record re-binds the row.
    repo.switch_view("dev").unwrap();
    repo.insert_from_view(CrossViewInsertOptions::new("fork1", "dev"))
        .unwrap();
    repo.insert_from_view(CrossViewInsertOptions::new("fork2", "dev"))
        .unwrap();

    // Simulate the recorded-history state where a later normal record has
    // re-bound the raw TREE row to the NON-LOWEST byte-equal incarnation
    // (fork2 — the live-graph shape: repair-inserted claim rows coexist
    // with a later record's binding). Isolated-fixture setup only; the
    // resolution itself runs through the production get_inode lookup.
    {
        use atomic_core::pristine::MutTxnT;
        let mut txn = repo.pristine.write_txn().unwrap();
        txn.put_tree("same.txt", fork2_inode).unwrap();
        txn.commit().unwrap();
    }
    let tree_inode = repo
        .pristine
        .read_txn()
        .unwrap()
        .get_inode("same.txt")
        .unwrap()
        .expect("the raw TREE binding must exist after the setup bind");
    assert_eq!(
        tree_inode, fork2_inode,
        "precondition: the TREE binding holds the fork2 (non-lowest) incarnation"
    );

    // The switch to dev removed same.txt from the worktree (dev's tree held
    // no same.txt at switch time); restore the user's working content.
    std::fs::write(&file, b"shared bytes\n").unwrap();

    // The user's preserve-content state: the working file holds the
    // byte-equal content.
    let resolution = repo.record(
        ChangeHeader::new("preserve-content resolution"),
        RecordOptions::new().resolve_name_conflicts(vec!["same.txt".to_string()]),
    );
    let resolution = match resolution {
        Ok(outcome) => outcome,
        Err(error) => panic!("the TREE-bound resolution must record: {error}"),
    };
    assert!(resolution.errors().is_empty());
    assert!(resolution.was_applied());

    // IDENTITY oracle: the SolveNameConflict op's surviving claimant inode
    // is the TREE-bound (non-lowest) incarnation, NOT an arbitrary pick.
    let survivor_inode = {
        use atomic_core::pristine::GraphTxnT;
        let op = resolution
            .change()
            .hunks()
            .iter()
            .find_map(|operation| match operation {
                GraphOp::SolveNameConflict { name, .. } => Some(name.clone()),
                _ => None,
            })
            .expect("the resolution change carries the SolveNameConflict op");
        let txn = repo.pristine.read_txn().unwrap();
        let internal = txn
            .get_internal(op.inode.change.as_ref().expect("external hash"))
            .unwrap()
            .expect("surviving claimant change registered");
        txn.position_inode(atomic_core::types::Position::new(internal, op.inode.pos))
            .unwrap()
            .expect("the surviving claimant position resolves to an inode")
    };
    assert_eq!(
        survivor_inode, fork2_inode,
        "the survivor must be the TREE-bound (non-lowest) incarnation, not an \
         arbitrary lowest-inode pick"
    );

    // Byte preservation + history retention.
    repo.materialize().unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"shared bytes\n".to_vec());
    assert!(repo.load_change(fork1.hash()).is_ok());
    assert!(repo.load_change(fork2.hash()).is_ok());
    let deps = resolution.change().dependencies();
    for change in [fork1.hash(), fork2.hash()] {
        assert!(
            deps.contains(change),
            "both competing incarnations are dependency-covered"
        );
    }
}

#[test]
fn explicit_preserve_content_resolution_refuses_missing_tree_binding() {
    let (temp_dir, mut repo) = create_temp_repo();
    let file = temp_dir.path().join("same.txt");

    std::fs::write(temp_dir.path().join("seed.txt"), "seed\n").unwrap();
    repo.add("seed.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    repo.create_view_from("feature", "dev").unwrap();

    repo.switch_view("feature").unwrap();
    std::fs::write(&file, "shared bytes\n").unwrap();
    repo.add("same.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "feature create").unwrap();

    repo.switch_view("dev").unwrap();
    std::fs::write(&file, "shared bytes\n").unwrap();
    repo.add("same.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "dev create").unwrap();
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();
    std::fs::write(&file, "shared bytes\n").unwrap();

    // The reviewer's probe precondition: NO raw TREE binding for the path.
    use atomic_core::pristine::TreeTxnT;
    let tree_inode = repo
        .pristine
        .read_txn()
        .unwrap()
        .get_inode("same.txt")
        .unwrap();
    assert!(
        tree_inode.is_none(),
        "precondition: the reviewer's probe shape has no TREE binding, got {tree_inode:?}"
    );

    let refused = repo.record(
        ChangeHeader::new("preserve-content resolution"),
        RecordOptions::new().resolve_name_conflicts(vec!["same.txt".to_string()]),
    );
    let error = match refused {
        Err(error) => error,
        Ok(outcome) => panic!(
            "the missing-binding resolve must refuse; recorded {} hunks",
            outcome.change().hunks().len()
        ),
    };
    let message = error.to_string();
    assert!(
        message.contains("no canonical TREE binding exists"),
        "the refusal must name the missing-binding precondition: {message}"
    );

    // Fail-closed: no change recorded, the working file untouched.
    drop(repo);
    let reopened = Repository::open(temp_dir.path()).unwrap();
    let history = reopened.effective_history(Some("dev")).unwrap();
    assert_eq!(
        history.len(),
        3,
        "base + two creates only: the refused resolution added nothing"
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"shared bytes\n".to_vec(),
        "the working file is untouched by the refusal"
    );
}

/// Review ::26 N1 (stale binding): when the raw TREE binding exists but
/// points at an incarnation OUTSIDE the working-byte-equal claimant set,
/// the preserve-content resolution FAILS CLOSED — the binding is stale or
/// unrelated and must not be presented as the survivor. Nothing is
/// recorded; graph and files are unchanged.
#[test]
fn explicit_preserve_content_resolution_refuses_stale_tree_binding() {
    let (temp_dir, mut repo) = create_temp_repo();
    let file = temp_dir.path().join("same.txt");

    std::fs::write(temp_dir.path().join("seed.txt"), "seed\n").unwrap();
    repo.add("seed.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    // Four incarnations: fork1 differs; fork2/fork3 are byte-equal; fork4
    // (recorded LAST, holding the TREE binding) differs from both.
    let mut forks = Vec::new();
    for (name, bytes) in [
        ("fork1", "fork1 bytes\n".as_bytes()),
        ("fork2", b"shared bytes\n"),
        ("fork3", b"shared bytes\n"),
        ("fork4", b"fork4 bytes\n"),
    ] {
        repo.create_view_from(name, "dev").unwrap();
        repo.switch_view(name).unwrap();
        std::fs::write(&file, bytes).unwrap();
        repo.add("same.txt", TrackingOptions::default()).unwrap();
        let change = record_all(&repo, format!("{name} create").as_str()).unwrap();
        let inode = repo.get_file_inode("same.txt").unwrap().unwrap();
        forks.push((name.to_string(), change, inode));
    }

    repo.switch_view("dev").unwrap();
    for (name, _, _) in &forks {
        repo.insert_from_view(CrossViewInsertOptions::new(name, "dev"))
            .unwrap();
    }
    repo.materialize().unwrap();

    // The multi-claimant deferred-tree replay unset the raw TREE row; bind
    // it to the LAST recorded (fork4) incarnation — which is NOT among the
    // working-byte-equal claimant set (fork2/fork3) — simulating the
    // live-graph shape where a later unrelated record holds the binding.
    {
        use atomic_core::pristine::MutTxnT;
        let mut txn = repo.pristine.write_txn().unwrap();
        txn.put_tree("same.txt", forks[3].2).unwrap();
        txn.commit().unwrap();
    }
    let tree_inode = repo
        .pristine
        .read_txn()
        .unwrap()
        .get_inode("same.txt")
        .unwrap()
        .expect("the raw TREE binding must exist after the setup bind");
    assert_eq!(
        tree_inode, forks[3].2,
        "precondition: the TREE binding holds the last (stale-for-the-match) incarnation"
    );

    // Working bytes = the shared bytes → two byte-equal sides; the
    // canonical binding (fork4) is outside that set → refuse.
    std::fs::write(&file, "shared bytes\n").unwrap();
    let refused = repo.record(
        ChangeHeader::new("preserve-content resolution"),
        RecordOptions::new().resolve_name_conflicts(vec!["same.txt".to_string()]),
    );
    let error = match refused {
        Err(error) => error,
        Ok(outcome) => panic!(
            "the stale-binding resolve must refuse; recorded {} hunks",
            outcome.change().hunks().len()
        ),
    };
    let message = error.to_string();
    assert!(
        message.contains("stale or unrelated"),
        "the refusal must name the stale-binding precondition: {message}"
    );

    // Fail-closed: no change recorded, the working file untouched.
    drop(repo);
    let reopened = Repository::open(temp_dir.path()).unwrap();
    let history = reopened.effective_history(Some("dev")).unwrap();
    assert_eq!(
        history.len(),
        5,
        "base + four creates only: the refused resolution added nothing"
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"shared bytes\n".to_vec(),
        "the working file is untouched by the refusal"
    );
}

/// The explicit resolve REFUSES a path whose working bytes match NO alive
/// claimant (unrecorded working content is never silently chosen).
#[test]
fn explicit_preserve_content_resolution_refuses_zero_match() {
    let (temp_dir, mut repo) = create_temp_repo();
    let file = temp_dir.path().join("same.txt");

    // Two-FileAdd shape: the file is untracked at base; each branch adds its
    // own incarnation with DIFFERENT bytes.
    std::fs::write(temp_dir.path().join("seed.txt"), "seed\n").unwrap();
    repo.add("seed.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    repo.create_view_from("feature", "dev").unwrap();

    repo.switch_view("feature").unwrap();
    std::fs::write(&file, "feature bytes\n").unwrap();
    repo.add("same.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "feature create").unwrap();

    repo.switch_view("dev").unwrap();
    std::fs::write(&file, "dev bytes\n").unwrap();
    repo.add("same.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "dev create").unwrap();
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    // Working bytes that match NO claimant's recorded content.
    std::fs::write(&file, "unrecorded working bytes\n").unwrap();
    let refused = repo.record(
        ChangeHeader::new("zero-match resolve"),
        RecordOptions::new().resolve_name_conflicts(vec!["same.txt".to_string()]),
    );
    let error = match refused {
        Err(error) => error,
        Ok(outcome) => panic!(
            "the zero-match resolve must refuse; recorded {} hunks",
            outcome.change().hunks().len()
        ),
    };
    let message = error.to_string();
    assert!(
        message.contains("preserve-content resolution refused at 'same.txt'"),
        "the refusal must name the preserve-content precondition: {message}"
    );
    // Nothing was recorded or mutated: the refusal is fail-closed (no
    // change, no resolution op).
    drop(repo);
    let reopened = Repository::open(temp_dir.path()).unwrap();
    let history = reopened.effective_history(Some("dev")).unwrap();
    assert_eq!(
        history.len(),
        3,
        "base + two creates only: the refused record added nothing"
    );
}

#[test]
fn probe_tree_binding_states() {
    use atomic_core::pristine::TreeTxnT;
    let (temp_dir, mut repo) = create_temp_repo();
    let file = temp_dir.path().join("same.txt");
    let tree = |label: &str, repo: &TestRepository| {
        let v = repo
            .pristine
            .read_txn()
            .unwrap()
            .get_inode("same.txt")
            .unwrap();
        println!("PROBE {label}: TREE={v:?}");
    };

    std::fs::write(temp_dir.path().join("seed.txt"), "seed\n").unwrap();
    repo.add("seed.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    tree("after base", &repo);

    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    std::fs::write(&file, "shared bytes\n").unwrap();
    repo.add("same.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "feature create").unwrap();
    tree("after feature add+record (on feature)", &repo);

    repo.create_view_from("feature2", "dev").unwrap();
    repo.switch_view("feature2").unwrap();
    std::fs::write(&file, "shared bytes\n").unwrap();
    repo.add("same.txt", TrackingOptions::default()).unwrap();
    let second = record_all(&repo, "feature2 create").unwrap();
    tree("after feature2 add+record (on feature2)", &repo);

    repo.switch_view("dev").unwrap();
    tree("after switch to dev (pre-inserts)", &repo);

    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    tree("after insert feature→dev", &repo);

    repo.insert_from_view(CrossViewInsertOptions::new("feature2", "dev"))
        .unwrap();
    tree("after insert feature2→dev", &repo);

    repo.materialize().unwrap();
    tree("after materialize (conflicted)", &repo);
    let _ = second;
}
