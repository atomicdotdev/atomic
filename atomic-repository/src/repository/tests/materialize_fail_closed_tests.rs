use super::*;
use crate::record::RecordOptions;
use atomic_core::change::ChangeHeader;
use atomic_core::pristine::{
    GraphTxnT, MutTxnT, StoredConflict, StoredConflictKind, TreeTxnT, ViewTxnT,
};
use atomic_core::types::{ChangePosition, EdgeFlags, Hash, NodeId, Position, SerializedGraphEdge};

const GOOD_PATH: &str = "a-good.txt";
const BAD_PATH: &str = "z-bad.txt";
const BAD_DEST_POS: u64 = 55_000_000;
const GOOD_SENTINEL: &[u8] = b"sentinel good working copy\n";
const BAD_SENTINEL: &[u8] = b"sentinel bad working copy\n";

type FileIndexSnapshot = Vec<(String, i64, u32, u64, Hash)>;
type ConflictSnapshot = Vec<(u64, Vec<StoredConflict>)>;
type PristineSnapshot = (FileIndexSnapshot, ConflictSnapshot);

fn pristine_snapshot(repo: &Repository) -> PristineSnapshot {
    let txn = repo.pristine.read_txn().unwrap();
    let view = txn.get_view("dev").unwrap().unwrap();
    let mut file_index = txn.iter_file_index().unwrap();
    file_index.sort_by(|left, right| left.0.cmp(&right.0));
    let mut conflicts = txn.iter_conflicts(view.id).unwrap();
    conflicts.sort_by_key(|(inode, _)| *inode);
    (file_index, conflicts)
}

fn assert_injected_destination_error(error: &RepositoryError, change_id: NodeId) {
    let error_text = format!("{error:?}\n{error}");
    assert!(
        error_text.contains("BlockNotFound") || error_text.contains("block not found"),
        "expected BlockNotFound, got {error_text}"
    );
    assert!(
        error_text.contains(&change_id.get().to_string())
            && error_text.contains(&BAD_DEST_POS.to_string()),
        "error must identify the injected destination, proving it was traversed: {error_text}"
    );
}

fn assert_full_materialize_is_fail_closed(
    materialize: impl FnOnce(&TestRepository) -> Result<MaterializeResult, RepositoryError>,
) {
    let (temp, repo) = create_temp_repo();
    let good = temp.path().join(GOOD_PATH);
    let bad = temp.path().join(BAD_PATH);

    std::fs::write(&good, b"recorded good\n").unwrap();
    std::fs::write(&bad, b"recorded bad\n").unwrap();
    repo.add(GOOD_PATH, TrackingOptions::default()).unwrap();
    repo.add(BAD_PATH, TrackingOptions::default()).unwrap();
    let outcome = repo
        .record(
            ChangeHeader::new("record materialize fail-closed fixture"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();

    let (change_id, good_inode, bad_inode, bad_position, view_id) = {
        let txn = repo.pristine.read_txn().unwrap();
        let change_id = txn.get_internal(outcome.hash()).unwrap().unwrap();
        let good_inode = txn.get_inode(GOOD_PATH).unwrap().unwrap();
        let bad_inode = txn.get_inode(BAD_PATH).unwrap().unwrap();
        let bad_position = txn.inode_position(bad_inode).unwrap().unwrap();
        let view_id = txn.get_view("dev").unwrap().unwrap().id;
        (change_id, good_inode, bad_inode, bad_position, view_id)
    };

    std::fs::write(&good, GOOD_SENTINEL).unwrap();
    std::fs::write(&bad, BAD_SENTINEL).unwrap();
    repo.update_file_index(&[
        (
            GOOD_PATH.to_string(),
            101,
            202,
            GOOD_SENTINEL.len() as u64,
            Hash::of(b"good index sentinel"),
        ),
        (
            BAD_PATH.to_string(),
            303,
            404,
            BAD_SENTINEL.len() as u64,
            Hash::of(b"bad index sentinel"),
        ),
    ])
    .unwrap();

    let bad_destination = Position::new(change_id, ChangePosition::new(BAD_DEST_POS));
    let bad_edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, bad_destination, change_id);
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        let source = bad_position.inode_node();
        txn.put_graph(source, bad_edge).unwrap();
        txn.put_inode_graph(bad_inode, source, bad_edge).unwrap();
        txn.put_conflicts(
            view_id,
            good_inode.get(),
            &[StoredConflict {
                kind: StoredConflictKind::Name,
                path: "conflict sentinel".to_string(),
                line: Some(77),
                sides: vec!["sentinel side".to_string()],
            }],
        )
        .unwrap();
        txn.commit().unwrap();
    }

    let pristine_before = pristine_snapshot(&repo);

    let linear_error = repo
        .get_file_content(BAD_PATH)
        .expect_err("linear fast path must reject an unresolved persisted destination");
    assert_injected_destination_error(&linear_error, change_id);
    assert_eq!(std::fs::read(&good).unwrap(), GOOD_SENTINEL);
    assert_eq!(std::fs::read(&bad).unwrap(), BAD_SENTINEL);
    assert_eq!(pristine_snapshot(&repo), pristine_before);

    let materialize_error =
        materialize(&repo).expect_err("corrupt persisted edge must fail materialization");
    assert_injected_destination_error(&materialize_error, change_id);
    assert_eq!(std::fs::read(&good).unwrap(), GOOD_SENTINEL);
    assert_eq!(std::fs::read(&bad).unwrap(), BAD_SENTINEL);
    assert_eq!(pristine_snapshot(&repo), pristine_before);
}

#[test]
fn sequential_full_materialize_preserves_working_copy_and_caches_on_graph_error() {
    assert_full_materialize_is_fail_closed(|repo| repo.materialize_sequential());
}

#[test]
fn parallel_full_materialize_preserves_working_copy_and_caches_on_graph_error() {
    assert_full_materialize_is_fail_closed(|repo| repo.materialize_parallel(None));
}

#[test]
fn switch_preflight_rejects_graph_error_before_pointer_or_file_mutation() {
    let (temp, mut repo) = create_temp_repo();
    let source_path = temp.path().join("source.txt");
    let target_path = temp.path().join(BAD_PATH);

    std::fs::write(&source_path, b"recorded source\n").unwrap();
    repo.add("source.txt", TrackingOptions::default()).unwrap();
    repo.record(
        ChangeHeader::new("record source view"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();

    repo.create_view("feature").unwrap();
    repo.switch_view("feature").unwrap();
    std::fs::write(&target_path, b"recorded target\n").unwrap();
    repo.add(BAD_PATH, TrackingOptions::default()).unwrap();
    let target_outcome = repo
        .record(
            ChangeHeader::new("record corruptible target"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    let (change_id, bad_inode, bad_position) = {
        let txn = repo.pristine.read_txn().unwrap();
        let change_id = txn.get_internal(target_outcome.hash()).unwrap().unwrap();
        let bad_inode = txn.get_inode(BAD_PATH).unwrap().unwrap();
        let bad_position = txn.inode_position(bad_inode).unwrap().unwrap();
        (change_id, bad_inode, bad_position)
    };

    repo.switch_view("dev").unwrap();
    assert!(!target_path.exists());
    let dev_view_id = {
        let txn = repo.pristine.read_txn().unwrap();
        txn.get_view("dev").unwrap().unwrap().id
    };

    std::fs::write(&source_path, GOOD_SENTINEL).unwrap();
    repo.update_file_index(&[(
        "source.txt".to_string(),
        515,
        616,
        GOOD_SENTINEL.len() as u64,
        Hash::of(b"switch index sentinel"),
    )])
    .unwrap();

    let bad_destination = Position::new(change_id, ChangePosition::new(BAD_DEST_POS));
    let bad_edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, bad_destination, change_id);
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        let source = bad_position.inode_node();
        txn.put_graph(source, bad_edge).unwrap();
        txn.put_inode_graph(bad_inode, source, bad_edge).unwrap();
        txn.put_conflicts(
            dev_view_id,
            bad_inode.get(),
            &[StoredConflict {
                kind: StoredConflictKind::Order,
                path: "switch conflict sentinel".to_string(),
                line: Some(88),
                sides: vec!["switch sentinel side".to_string()],
            }],
        )
        .unwrap();
        txn.commit().unwrap();
    }

    let pristine_before = pristine_snapshot(&repo);
    let pointer_before = std::fs::read_to_string(temp.path().join(".atomic/current_view")).unwrap();
    let error = repo
        .switch_view("feature")
        .expect_err("target graph corruption must refuse before switching");
    assert_injected_destination_error(&error, change_id);

    assert_eq!(repo.current_view(), "dev");
    assert_eq!(
        std::fs::read_to_string(temp.path().join(".atomic/current_view")).unwrap(),
        pointer_before
    );
    assert_eq!(std::fs::read(&source_path).unwrap(), GOOD_SENTINEL);
    assert!(!target_path.exists());
    assert_eq!(pristine_snapshot(&repo), pristine_before);
}
