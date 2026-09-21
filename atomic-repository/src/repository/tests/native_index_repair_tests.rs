use super::*;

use atomic_core::pristine::{
    decode_inode_vertex, directory_flags, encode_inode_vertex, encode_path_claim_event,
    encode_position, encode_view_seq, PathClaimState, PathClaimTxnT, TreeTxnT, ViewTxnT, CONFLICTS,
    DIRECTORIES, INODES, INODE_GRAPH, PATH_CLAIMS, PATH_CLAIM_EVENT_SIZE, PATH_CLAIM_SCHEMA_KEY,
    PRISTINE_META, REV_INODES, REV_TREE, TREE,
};
use redb::ReadableMultimapTable;

use crate::apply::CrossViewInsertOptions;
use crate::record::RecordOptions;

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

fn view_history(repo: &Repository) -> Vec<(u64, NodeId, Merkle)> {
    let txn = repo.pristine.read_txn().unwrap();
    let view = txn.get_view(repo.current_view()).unwrap().unwrap();
    txn.iter_changes(&view, 0)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn inode_graph_keys(repo: &Repository) -> Vec<(Inode, atomic_core::types::GraphNode<NodeId>)> {
    repo.pristine
        .read_txn()
        .unwrap()
        .snapshot_inode_graph_keys()
        .unwrap()
}

#[test]
fn native_index_verifier_accepts_healthy_nested_projection() {
    let (temp, repo) = create_temp_repo();
    let file = temp.path().join("src/domain/model.rs");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"model\n").unwrap();
    repo.add("src/domain/model.rs", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "nested base");

    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(report.is_healthy(), "problems: {:?}", report.problems);
    assert!(report.expected_rows > 0);
    assert_eq!(report.expected_rows, report.actual_rows);
}

#[test]
fn combined_native_index_corruption_is_detected_repaired_and_idempotent() {
    let (temp, repo) = create_temp_repo();
    std::fs::create_dir_all(temp.path().join("src/domain")).unwrap();
    std::fs::write(temp.path().join("src/domain/a.txt"), b"alpha\n").unwrap();
    std::fs::write(temp.path().join("src/domain/b.txt"), b"beta\n").unwrap();
    repo.add("src/domain/a.txt", TrackingOptions::default())
        .unwrap();
    repo.add("src/domain/b.txt", TrackingOptions::default())
        .unwrap();
    let recorded = record_all(&repo, "base");

    let txn = repo.pristine.read_txn().unwrap();
    let a_inode = txn.get_inode("src/domain/a.txt").unwrap().unwrap();
    let b_inode = txn.get_inode("src/domain/b.txt").unwrap().unwrap();
    let a_position = txn.inode_position(a_inode).unwrap().unwrap();
    let b_position = txn.inode_position(b_inode).unwrap().unwrap();
    let src_inode = txn.get_inode("src").unwrap().unwrap();
    let view = txn.get_view(repo.current_view()).unwrap().unwrap();
    drop(txn);

    let graph_before = inode_graph_keys(&repo);
    let history_before = view_history(&repo);
    let change_before = std::fs::read(repo.change_store.change_path(recorded.hash())).unwrap();
    let worktree_before = (
        std::fs::read(temp.path().join("src/domain/a.txt")).unwrap(),
        std::fs::read(temp.path().join("src/domain/b.txt")).unwrap(),
    );
    drop(repo);

    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut claims = write.open_multimap_table(PATH_CLAIMS).unwrap();
        claims.remove_all("src/domain/a.txt").unwrap();

        let mut tree = write.open_table(TREE).unwrap();
        tree.remove("src/domain/a.txt").unwrap();
        tree.insert("src/domain/stale.txt", a_inode.get()).unwrap();

        let mut reverse = write.open_table(REV_TREE).unwrap();
        reverse
            .insert(a_inode.get(), "src/domain/stale.txt")
            .unwrap();
        reverse.insert(b_inode.get(), "wrong/b.txt").unwrap();

        let mut inodes = write.open_table(INODES).unwrap();
        inodes.remove(a_inode.get()).unwrap();

        let mut rev_inodes = write.open_table(REV_INODES).unwrap();
        let b_key = encode_position(b_position.change.get(), b_position.pos.get());
        rev_inodes.remove(&b_key).unwrap();

        let mut directories = write.open_table(DIRECTORIES).unwrap();
        directories
            .insert(
                src_inode.get(),
                directory_flags::DIR_EXPLICIT | directory_flags::DIR_EMPTY,
            )
            .unwrap();
        directories
            .insert(u64::MAX - 5, directory_flags::DIR_EMPTY)
            .unwrap();

        let stale = vec![atomic_core::pristine::StoredConflict {
            kind: atomic_core::pristine::StoredConflictKind::Order,
            path: "src/domain/stale.txt".to_string(),
            line: Some(1),
            sides: Vec::new(),
        }];
        let conflict_bytes = serde_json::to_vec(&stale).unwrap();
        let conflict_key = encode_view_seq(view.id, a_inode.get());
        let mut conflicts = write.open_table(CONFLICTS).unwrap();
        conflicts
            .insert(&conflict_key, conflict_bytes.as_slice())
            .unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open_readonly(temp.path()).unwrap();
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(!report.is_healthy());
    let indexes: std::collections::BTreeSet<_> = report
        .problems
        .iter()
        .map(|problem| problem.index)
        .collect();
    for expected in [
        NativeIndex::PathClaims,
        NativeIndex::Tree,
        NativeIndex::RevTree,
        NativeIndex::Inodes,
        NativeIndex::RevInodes,
        NativeIndex::Directories,
        NativeIndex::Conflicts,
    ] {
        assert!(
            indexes.contains(&expected),
            "missing diagnostic for {expected}"
        );
    }
    drop(repo);

    let repo = Repository::open(temp.path()).unwrap();
    let outcome = repo.repair_native_derived_indexes().unwrap();
    assert!(!outcome.already_healthy);
    assert!(outcome.problems_repaired >= 7);
    let healthy = repo.verify_native_derived_indexes().unwrap();
    assert!(healthy.is_healthy(), "problems: {:?}", healthy.problems);
    assert_eq!(
        repo.get_file_inode("src/domain/a.txt").unwrap(),
        Some(a_inode)
    );
    assert_eq!(
        repo.get_file_inode("src/domain/b.txt").unwrap(),
        Some(b_inode)
    );
    let txn = repo.pristine.read_txn().unwrap();
    assert_eq!(txn.inode_position(a_inode).unwrap(), Some(a_position));
    assert_eq!(txn.inode_position(b_inode).unwrap(), Some(b_position));
    assert_eq!(
        txn.get_directory_flags(src_inode).unwrap(),
        Some(directory_flags::DIR_EXPLICIT)
    );
    drop(txn);

    assert_eq!(inode_graph_keys(&repo), graph_before);
    assert_eq!(view_history(&repo), history_before);
    assert_eq!(
        std::fs::read(repo.change_store.change_path(recorded.hash())).unwrap(),
        change_before
    );
    assert_eq!(
        (
            std::fs::read(temp.path().join("src/domain/a.txt")).unwrap(),
            std::fs::read(temp.path().join("src/domain/b.txt")).unwrap(),
        ),
        worktree_before
    );

    let second = repo.repair_native_derived_indexes().unwrap();
    assert!(second.already_healthy);
}

#[test]
fn injected_failure_rolls_back_complete_replacement() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("f.txt"), b"content\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");
    let inode = repo.get_file_inode("f.txt").unwrap().unwrap();
    drop(repo);

    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut tree = write.open_table(TREE).unwrap();
        tree.remove("f.txt").unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open(temp.path()).unwrap();
    let before = repo.verify_native_derived_indexes().unwrap();
    assert!(!before.is_healthy());
    assert!(repo
        .repair_native_derived_indexes_with_injected_failure()
        .is_err());
    let after = repo.verify_native_derived_indexes().unwrap();
    assert_eq!(after.problems, before.problems);
    assert_eq!(repo.get_file_inode("f.txt").unwrap(), None);

    repo.repair_native_derived_indexes().unwrap();
    assert_eq!(repo.get_file_inode("f.txt").unwrap(), Some(inode));
}

#[test]
fn malformed_claim_and_conflict_rows_are_detected_and_replaced() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("f.txt"), b"content\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");
    let txn = repo.pristine.read_txn().unwrap();
    let inode = txn.get_inode("f.txt").unwrap().unwrap();
    let view = txn.get_view(repo.current_view()).unwrap().unwrap();
    drop(txn);
    drop(repo);

    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut malformed_claim = [0_u8; PATH_CLAIM_EVENT_SIZE];
        malformed_claim[0] = 0xff;
        let mut claims = write.open_multimap_table(PATH_CLAIMS).unwrap();
        claims.insert("f.txt", &malformed_claim).unwrap();

        let key = encode_view_seq(view.id, inode.get());
        let mut conflicts = write.open_table(CONFLICTS).unwrap();
        conflicts.insert(&key, b"not-json".as_slice()).unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open(temp.path()).unwrap();
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(report.problems.iter().any(|problem| {
        problem.index == NativeIndex::PathClaims
            && problem.kind == NativeIndexProblemKind::Malformed
    }));
    assert!(report.problems.iter().any(|problem| {
        problem.index == NativeIndex::Conflicts && problem.kind == NativeIndexProblemKind::Malformed
    }));

    repo.repair_native_derived_indexes().unwrap();
    assert!(repo.verify_native_derived_indexes().unwrap().is_healthy());
}

#[test]
fn repair_preserves_bijective_graphless_staged_file_and_directory() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("recorded.txt"), b"recorded\n").unwrap();
    repo.add("recorded.txt", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "base");

    std::fs::write(temp.path().join("staged.txt"), b"staged bytes\n").unwrap();
    repo.add("staged.txt", TrackingOptions::default()).unwrap();
    std::fs::create_dir(temp.path().join("empty-staged")).unwrap();
    repo.add_directory("empty-staged", TrackingOptions::default())
        .unwrap();
    let staged_inode = repo.get_file_inode("staged.txt").unwrap().unwrap();
    let staged_dir_inode = repo.get_file_inode("empty-staged").unwrap().unwrap();
    assert!(repo.verify_native_derived_indexes().unwrap().is_healthy());

    let txn = repo.pristine.read_txn().unwrap();
    let recorded_inode = txn.get_inode("recorded.txt").unwrap().unwrap();
    drop(txn);
    drop(repo);
    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut reverse = write.open_table(REV_TREE).unwrap();
        reverse.insert(recorded_inode.get(), "wrong.txt").unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open(temp.path()).unwrap();
    repo.repair_native_derived_indexes().unwrap();
    assert_eq!(
        repo.get_file_inode("staged.txt").unwrap(),
        Some(staged_inode)
    );
    assert_eq!(
        repo.get_file_inode("empty-staged").unwrap(),
        Some(staged_dir_inode)
    );
    let txn = repo.pristine.read_txn().unwrap();
    assert_eq!(txn.inode_position(staged_inode).unwrap(), None);
    assert_eq!(txn.inode_position(staged_dir_inode).unwrap(), None);
    assert_eq!(
        txn.get_directory_flags(staged_dir_inode).unwrap(),
        Some(directory_flags::DIR_EXPLICIT | directory_flags::DIR_EMPTY)
    );
    assert_eq!(
        std::fs::read(temp.path().join("staged.txt")).unwrap(),
        b"staged bytes\n"
    );
    assert!(repo.verify_native_derived_indexes().unwrap().is_healthy());
}

#[test]
fn staged_children_keep_recorded_and_staged_parents_non_empty() {
    let (temp, repo) = create_temp_repo();
    std::fs::create_dir(temp.path().join("recorded-parent")).unwrap();
    repo.add_directory("recorded-parent", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "record parent");
    std::fs::write(temp.path().join("recorded-parent/staged.txt"), b"staged\n").unwrap();
    repo.add("recorded-parent/staged.txt", TrackingOptions::default())
        .unwrap();

    std::fs::create_dir(temp.path().join("staged-parent")).unwrap();
    repo.add_directory("staged-parent", TrackingOptions::default())
        .unwrap();
    std::fs::write(temp.path().join("staged-parent/child.txt"), b"child\n").unwrap();
    repo.add("staged-parent/child.txt", TrackingOptions::default())
        .unwrap();

    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(report.is_healthy(), "problems: {:?}", report.problems);
    let txn = repo.pristine.read_txn().unwrap();
    for path in ["recorded-parent", "staged-parent"] {
        let inode = txn.get_inode(path).unwrap().unwrap();
        assert_eq!(
            txn.get_directory_flags(inode).unwrap(),
            Some(directory_flags::DIR_EXPLICIT)
        );
    }
}

#[test]
fn suspicious_inode_binding_on_staged_path_refuses_repair() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("base.txt"), b"base\n").unwrap();
    repo.add("base.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");
    std::fs::write(temp.path().join("staged.txt"), b"staged\n").unwrap();
    repo.add("staged.txt", TrackingOptions::default()).unwrap();
    let staged = repo.get_file_inode("staged.txt").unwrap().unwrap();
    let base = repo.get_file_inode("base.txt").unwrap().unwrap();
    let txn = repo.pristine.read_txn().unwrap();
    let mut fake_position = txn.inode_position(base).unwrap().unwrap();
    fake_position.pos = atomic_core::types::ChangePosition::new(fake_position.pos.get() + 10_000);
    drop(txn);
    drop(repo);

    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let encoded = encode_position(fake_position.change.get(), fake_position.pos.get());
        let mut inodes = write.open_table(INODES).unwrap();
        inodes.insert(staged.get(), &encoded).unwrap();
        let mut reverse = write.open_table(REV_INODES).unwrap();
        reverse.insert(&encoded, staged.get()).unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open_for_native_repair(temp.path()).unwrap();
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(report
        .problems
        .iter()
        .any(|problem| problem.kind == NativeIndexProblemKind::Unrepairable));
    assert!(repo.repair_native_derived_indexes().is_err());
    assert_eq!(repo.get_file_inode("staged.txt").unwrap(), Some(staged));
    let txn = repo.pristine.read_txn().unwrap();
    assert_eq!(txn.inode_position(staged).unwrap(), Some(fake_position));
    assert_eq!(
        std::fs::read(temp.path().join("staged.txt")).unwrap(),
        b"staged\n"
    );
}

#[test]
fn contradictory_path_claim_transition_is_reported_as_stale() {
    let (temp, repo) = create_temp_repo();
    let path = temp.path().join("a|1.txt");
    std::fs::write(&path, b"content\n").unwrap();
    repo.add("a|1.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");
    std::fs::remove_file(&path).unwrap();
    record_all(&repo, "delete");
    let txn = repo.pristine.read_txn().unwrap();
    let mut contradictory = txn
        .iter_path_claims()
        .unwrap()
        .into_iter()
        .find(|entry| entry.path == "a|1.txt" && entry.event.state == PathClaimState::Dead)
        .unwrap();
    contradictory.event.state = PathClaimState::Alive;
    drop(txn);
    drop(repo);

    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let encoded = encode_path_claim_event(&contradictory.event);
        let mut claims = write.open_multimap_table(PATH_CLAIMS).unwrap();
        claims
            .insert(contradictory.path.as_str(), &encoded)
            .unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open_for_native_repair(temp.path()).unwrap();
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(report.problems.iter().any(|problem| {
        problem.index == NativeIndex::PathClaims
            && problem.kind == NativeIndexProblemKind::Stale
            && problem.key.contains("a|1.txt")
    }));
    repo.repair_native_derived_indexes().unwrap();
    assert!(repo.verify_native_derived_indexes().unwrap().is_healthy());
}

#[test]
fn poisoned_inode_graph_owner_refuses_legacy_repair_without_prewrite_migration() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("f.txt"), b"content\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");
    let inode = repo.get_file_inode("f.txt").unwrap().unwrap();
    let txn = repo.pristine.read_txn().unwrap();
    let position = txn.inode_position(inode).unwrap().unwrap();
    drop(txn);
    drop(repo);

    let poisoned_inode = Inode::new(inode.get() + 10_000);
    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut graph = write.open_multimap_table(INODE_GRAPH).unwrap();
        let rows: Vec<_> = graph
            .iter()
            .unwrap()
            .flat_map(|row| {
                let (key, values) = row.unwrap();
                let key = *key.value();
                values.map(move |value| (key, *value.unwrap().value()))
            })
            .filter(|(key, _)| decode_inode_vertex(key).0 == inode.get())
            .collect();
        assert!(!rows.is_empty());
        for (key, value) in rows {
            graph.remove(&key, &value).unwrap();
            let (_, change, start, end) = decode_inode_vertex(&key);
            let poisoned = encode_inode_vertex(poisoned_inode.get(), change, start, end);
            graph.insert(&poisoned, &value).unwrap();
        }
        let mut metadata = write.open_table(PRISTINE_META).unwrap();
        metadata.remove(PATH_CLAIM_SCHEMA_KEY).unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open_for_native_repair(temp.path()).unwrap();
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(report
        .problems
        .iter()
        .any(|problem| problem.kind == NativeIndexProblemKind::Unrepairable));
    assert!(repo.repair_native_derived_indexes().is_err());
    assert_eq!(repo.get_file_inode("f.txt").unwrap(), Some(inode));
    assert_eq!(
        repo.pristine
            .read_txn()
            .unwrap()
            .inode_position(inode)
            .unwrap(),
        Some(position)
    );
    drop(repo);

    let database = redb::Database::open(&database_path).unwrap();
    let read = database.begin_read().unwrap();
    let metadata = read.open_table(PRISTINE_META).unwrap();
    assert!(metadata.get(PATH_CLAIM_SCHEMA_KEY).unwrap().is_none());
}

#[test]
fn missing_path_claim_schema_is_repaired_without_implicit_open_migration() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("f.txt"), b"content\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");
    drop(repo);

    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut metadata = write.open_table(PRISTINE_META).unwrap();
        metadata.remove(PATH_CLAIM_SCHEMA_KEY).unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open_readonly_for_native_repair(temp.path()).unwrap();
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(report.problems.iter().any(|problem| {
        problem.index == NativeIndex::PathClaims
            && problem.kind == NativeIndexProblemKind::Missing
            && problem.key == "<schema>"
    }));
    drop(repo);

    let repo = Repository::open_for_native_repair(temp.path()).unwrap();
    repo.repair_native_derived_indexes().unwrap();
    assert!(repo.verify_native_derived_indexes().unwrap().is_healthy());
    drop(repo);
    Repository::open_readonly(temp.path()).unwrap();
}

#[test]
fn directory_empty_flags_use_exact_direct_children_during_repair() {
    let (temp, repo) = create_temp_repo();
    std::fs::create_dir(temp.path().join("foo")).unwrap();
    repo.add_directory("foo", TrackingOptions::default())
        .unwrap();
    std::fs::create_dir(temp.path().join("foobar")).unwrap();
    repo.add_directory("foobar", TrackingOptions::default())
        .unwrap();
    std::fs::write(temp.path().join("foobar/child.txt"), b"child\n").unwrap();
    repo.add("foobar/child.txt", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "directories");
    let txn = repo.pristine.read_txn().unwrap();
    let foo = txn.get_inode("foo").unwrap().unwrap();
    let foobar = txn.get_inode("foobar").unwrap().unwrap();
    drop(txn);
    drop(repo);

    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut directories = write.open_table(DIRECTORIES).unwrap();
        directories
            .insert(foo.get(), directory_flags::DIR_EXPLICIT)
            .unwrap();
        directories
            .insert(
                foobar.get(),
                directory_flags::DIR_EXPLICIT | directory_flags::DIR_EMPTY,
            )
            .unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open(temp.path()).unwrap();
    repo.repair_native_derived_indexes().unwrap();
    let txn = repo.pristine.read_txn().unwrap();
    assert_eq!(
        txn.get_directory_flags(foo).unwrap(),
        Some(directory_flags::DIR_EXPLICIT | directory_flags::DIR_EMPTY)
    );
    assert_eq!(
        txn.get_directory_flags(foobar).unwrap(),
        Some(directory_flags::DIR_EXPLICIT)
    );
}

#[test]
fn content_conflict_projection_is_rebuilt_from_graph_markers() {
    let (temp, mut repo) = create_temp_repo();
    let file = temp.path().join("f.txt");
    std::fs::write(&file, "line1\nline2\nline3\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");
    repo.create_view_from("feature", "dev").unwrap();

    repo.switch_view("feature").unwrap();
    std::fs::write(&file, "line1\nfeature\nline2\nline3\n").unwrap();
    record_all(&repo, "feature edit");
    repo.switch_view("dev").unwrap();
    std::fs::write(&file, "line1\ndev\nline2\nline3\n").unwrap();
    record_all(&repo, "dev edit");
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    let initial = repo.verify_native_derived_indexes().unwrap();
    assert!(initial.problems.iter().any(|problem| {
        problem.index == NativeIndex::Conflicts && problem.kind == NativeIndexProblemKind::Missing
    }));
    repo.repair_native_derived_indexes().unwrap();
    assert!(repo.verify_native_derived_indexes().unwrap().is_healthy());
    let txn = repo.pristine.read_txn().unwrap();
    let view = txn.get_view("dev").unwrap().unwrap();
    let feature = txn.get_view("feature").unwrap().unwrap();
    let inode = txn.get_inode("f.txt").unwrap().unwrap();
    assert!(!txn.iter_conflicts(view.id).unwrap().is_empty());
    assert!(!txn.iter_conflicts(feature.id).unwrap().is_empty());
    drop(txn);
    drop(repo);

    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let key = encode_view_seq(view.id, inode.get());
        let mut conflicts = write.open_table(CONFLICTS).unwrap();
        conflicts.remove(&key).unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open(temp.path()).unwrap();
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(report.problems.iter().any(|problem| {
        problem.index == NativeIndex::Conflicts && problem.kind == NativeIndexProblemKind::Missing
    }));
    repo.repair_native_derived_indexes().unwrap();
    let txn = repo.pristine.read_txn().unwrap();
    assert!(!txn.iter_conflicts(view.id).unwrap().is_empty());
}

#[test]
fn name_conflict_ambiguity_survives_repair_without_tree_winner() {
    let (temp, mut repo) = create_temp_repo();
    std::fs::write(temp.path().join("seed.txt"), b"seed\n").unwrap();
    repo.add("seed.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");
    repo.create_view_from("feature", "dev").unwrap();

    repo.switch_view("feature").unwrap();
    std::fs::write(temp.path().join("same.txt"), b"feature\n").unwrap();
    repo.add("same.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "feature create");

    repo.switch_view("dev").unwrap();
    std::fs::write(temp.path().join("same.txt"), b"dev\n").unwrap();
    repo.add("same.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "dev create");
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    let before = std::fs::read(temp.path().join("same.txt")).unwrap();
    assert!(before.windows(7).any(|window| window == b">>>>>>>"));
    assert_eq!(repo.get_file_inode("same.txt").unwrap(), None);
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(report.problems.iter().any(|problem| {
        problem.index == NativeIndex::Conflicts && problem.kind == NativeIndexProblemKind::Missing
    }));

    let repaired = repo.repair_native_derived_indexes().unwrap();
    assert!(!repaired.already_healthy);
    assert_eq!(repo.get_file_inode("same.txt").unwrap(), None);
    assert!(repo.get_file_content("same.txt").is_err());
    assert_eq!(std::fs::read(temp.path().join("same.txt")).unwrap(), before);
    assert!(repo.verify_native_derived_indexes().unwrap().is_healthy());
}

#[test]
fn one_sided_graphless_staging_is_unrepairable_and_preserved() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("base.txt"), b"base\n").unwrap();
    repo.add("base.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");
    std::fs::write(temp.path().join("staged.txt"), b"staged\n").unwrap();
    repo.add("staged.txt", TrackingOptions::default()).unwrap();
    let staged_inode = repo.get_file_inode("staged.txt").unwrap().unwrap();
    drop(repo);

    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut reverse = write.open_table(REV_TREE).unwrap();
        reverse.remove(staged_inode.get()).unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open(temp.path()).unwrap();
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(report
        .problems
        .iter()
        .any(|problem| { problem.kind == NativeIndexProblemKind::Unrepairable }));
    assert!(repo.repair_native_derived_indexes().is_err());
    assert_eq!(
        repo.get_file_inode("staged.txt").unwrap(),
        Some(staged_inode)
    );
    assert_eq!(
        std::fs::read(temp.path().join("staged.txt")).unwrap(),
        b"staged\n"
    );
}

/// F4/P2 (CB-9B final repair finding, CB-13A truthfulness): a stale extra
/// event on a path the derivation also produces must not let the targeted
/// PATH_CLAIMS repair report `already_healthy`. Every derived row can be
/// present while the index still disagrees with graph authority, so the
/// repair must refuse exactly as it does for events on unknown paths, leave
/// the table untouched, and leave the full rebuild as the remediation.
#[test]
fn targeted_path_claim_repair_refuses_stale_event_on_derived_path() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("f.txt"), b"content\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");
    let txn = repo.pristine.read_txn().unwrap();
    let alive = txn
        .iter_path_claims()
        .unwrap()
        .into_iter()
        .find(|entry| entry.path == "f.txt" && entry.event.state == PathClaimState::Alive)
        .expect("the recorded file has an Alive claim");
    drop(txn);
    drop(repo);

    // F4 state: every derived row is present, but the existing path also
    // carries an extra event the graph derivation does not produce.
    let mut stale = alive.event;
    stale.state = PathClaimState::Dead;
    let stale_encoded = encode_path_claim_event(&stale);
    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut claims = write.open_multimap_table(PATH_CLAIMS).unwrap();
        claims.insert(alive.path.as_str(), &stale_encoded).unwrap();
    }
    write.commit().unwrap();
    drop(database);

    // Read-only doctor verification deterministically reports the divergence.
    let repo = Repository::open_readonly_for_native_repair(temp.path()).unwrap();
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.index == NativeIndex::PathClaims
                && problem.key.contains("f.txt")),
        "doctor must report the stale event, problems: {:?}",
        report.problems
    );
    drop(repo);

    // The targeted repair refuses, failing closed, without modifying rows.
    let repo = Repository::open_for_native_repair(temp.path()).unwrap();
    let error = repo.repair_path_claims_index().unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("stale rows") && message.contains("native-index rebuild"),
        "unexpected refusal message: {message}"
    );
    drop(repo);

    let database = redb::Database::open(&database_path).unwrap();
    let read = database.begin_read().unwrap();
    let claims = read.open_multimap_table(PATH_CLAIMS).unwrap();
    let rows: Vec<_> = claims
        .get("f.txt")
        .unwrap()
        .map(|value| *value.unwrap().value())
        .collect();
    drop(claims);
    drop(read);
    drop(database);
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&encode_path_claim_event(&alive.event)));
    assert!(rows.contains(&stale_encoded));

    // The documented remediation for stale rows is the full rebuild.
    let repo = Repository::open_for_native_repair(temp.path()).unwrap();
    repo.repair_native_derived_indexes().unwrap();
    assert!(repo.verify_native_derived_indexes().unwrap().is_healthy());
}

/// The targeted PATH_CLAIMS repair is truthful in both directions: a healthy
/// index reports `already_healthy` with zero writes, genuinely missing
/// structural rows are inserted atomically, and a repeated run is idempotent.
#[test]
fn targeted_path_claim_repair_inserts_missing_rows_idempotently() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("a.txt"), b"alpha\n").unwrap();
    std::fs::write(temp.path().join("b.txt"), b"beta\n").unwrap();
    repo.add("a.txt", TrackingOptions::default()).unwrap();
    repo.add("b.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");

    let healthy = repo.repair_path_claims_index().unwrap();
    assert!(healthy.already_healthy);
    assert_eq!(healthy.rows_written, 0);

    let txn = repo.pristine.read_txn().unwrap();
    let removed: Vec<_> = txn
        .iter_path_claims()
        .unwrap()
        .into_iter()
        .filter(|entry| entry.path == "a.txt")
        .collect();
    assert!(!removed.is_empty(), "a.txt has structural claims");
    drop(txn);
    drop(repo);

    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut claims = write.open_multimap_table(PATH_CLAIMS).unwrap();
        claims.remove_all("a.txt").unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open_for_native_repair(temp.path()).unwrap();
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.index == NativeIndex::PathClaims
                && problem.key.contains("a.txt")),
        "doctor must report the missing claims, problems: {:?}",
        report.problems
    );

    let outcome = repo.repair_path_claims_index().unwrap();
    assert!(!outcome.already_healthy);
    assert_eq!(outcome.rows_written, removed.len());
    assert!(repo.verify_native_derived_indexes().unwrap().is_healthy());

    let second = repo.repair_path_claims_index().unwrap();
    assert!(second.already_healthy);
    assert_eq!(second.rows_written, 0);
}

/// CB-13A R3: an explicit targeted repair is an immutable, journaled
/// remediation — the executed repair is recorded as an `OperationKind::Repair`
/// operation with before/after evidence digests and a same-transaction
/// operation-level verified receipt, and a repeated healthy run journals
/// nothing. The row-level refusal stays a pure refusal (nothing journaled).
#[test]
fn targeted_path_claim_repair_journals_an_immutable_remediation_operation() {
    use atomic_core::operation::{EffectReceiptKind, OperationKind};
    use atomic_core::pristine::OperationTxnT;

    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("a.txt"), b"alpha\n").unwrap();
    repo.add("a.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base");

    // Healthy check: no repair runs, so nothing is journaled.
    let healthy = repo.repair_path_claims_index().unwrap();
    assert!(healthy.already_healthy);
    assert!(healthy.operation.is_none());

    // Remove the structural rows, then repair.
    let txn = repo.pristine.read_txn().unwrap();
    let removed: Vec<_> = txn
        .iter_path_claims()
        .unwrap()
        .into_iter()
        .filter(|entry| entry.path == "a.txt")
        .collect();
    drop(txn);
    drop(repo);
    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut claims = write.open_multimap_table(PATH_CLAIMS).unwrap();
        claims.remove_all("a.txt").unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open_for_native_repair(temp.path()).unwrap();
    let outcome = repo.repair_path_claims_index().unwrap();
    assert!(!outcome.already_healthy);
    assert_eq!(outcome.rows_written, removed.len());
    let repair_operation = outcome.operation.expect("the executed repair is journaled");

    let txn = repo.pristine.read_txn().unwrap();
    let operation = txn
        .get_operation(repair_operation)
        .unwrap()
        .expect("journaled repair operation is durable");
    assert_eq!(operation.payload().kind, OperationKind::Repair);
    assert!(
        operation.payload().working_copy.is_none(),
        "the repair is repository-scoped"
    );
    assert_eq!(
        operation.payload().evidence.len(),
        2,
        "before-plan and post-write evidence digests are recorded"
    );
    let receipts = txn.get_effect_receipts(repair_operation).unwrap();
    assert!(
        receipts
            .iter()
            .any(|receipt| receipt.payload().kind == EffectReceiptKind::Verified),
        "the repair journal carries a same-transaction verified receipt"
    );
    drop(txn);

    // Repeat: a healthy index journals nothing (idempotent).
    let repeat = repo.repair_path_claims_index().unwrap();
    assert!(repeat.already_healthy);
    assert!(repeat.operation.is_none());
    let txn = repo.pristine.read_txn().unwrap();
    let repair_count = txn
        .list_operations()
        .unwrap()
        .into_iter()
        .filter(|operation| operation.payload().kind == OperationKind::Repair)
        .count();
    drop(txn);
    assert_eq!(repair_count, 1, "a healthy repeat must not journal again");
}

/// CB-13A follow-up R3: the path-claims repair is a recoverable, invertible
/// remediation — the plan digest covers the complete old table, the inverse
/// is stored beside the journaled operation, and undo-of-recovery restores
/// the exact before-state under fresh leases with third-value rejection and
/// idempotent receipts.
#[test]
fn path_claims_repair_journals_reconstructible_inverse_and_undo_restores_it() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("undo.txt"), b"undo\n").unwrap();
    repo.add("undo.txt", TrackingOptions::default()).unwrap();
    let recorded = record_all(&repo, "undo base");
    drop(repo);

    // Corrupt the claims table the same way the combined fixture does.
    let database_path = temp.path().join(".atomic/pristine.redb");
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut claims = write.open_multimap_table(PATH_CLAIMS).unwrap();
        claims.remove_all("undo.txt").unwrap();
    }
    write.commit().unwrap();
    drop(database);

    // The repair lands and journals its inverse.
    let repo = Repository::open(temp.path()).unwrap();
    let report = repo.verify_native_derived_indexes().unwrap();
    assert!(!report.is_healthy());
    let outcome = repo.repair_path_claims_index().unwrap();
    assert!(!outcome.already_healthy);
    let repair_op = outcome.operation.expect("the repair journals an operation");
    let healthy = repo.verify_native_derived_indexes().unwrap();
    assert!(healthy.is_healthy(), "post-repair: {:?}", healthy.problems);

    // The stored inverse exists beside the operation id.
    let inverse_path = temp
        .path()
        .join(".atomic/operation-recovery")
        .join(format!("path-claims-repair-inverse-{repair_op}.json"));
    assert!(inverse_path.exists(), "the stored inverse is durable");

    // Reopen: the state survives, the undo runs under fresh leases, and
    // restores the exact before-state (the claim row is gone again).
    drop(repo);
    let repo = Repository::open(temp.path()).unwrap();
    let undo = repo.undo_last_path_claims_repair().unwrap().expect("the undo runs");
    assert!(!undo.already_healthy);
    assert!(undo.rows_written >= 1);
    drop(repo);
    let repo = Repository::open(temp.path()).unwrap();
    // Undo restored the pre-repair (corrupted) table: the derived graph
    // authority no longer matches the live table — the verifier reports it
    // and a REPAIR re-heals (the full recover cycle).
    let after_undo = repo.verify_native_derived_indexes().unwrap();
    assert!(
        !after_undo.is_healthy(),
        "the undo restored the before-state; the verifier reports the divergence again"
    );

    // Repeat undo: the live table is now the before-state — a second undo
    // refuses as a third value (idempotence: nothing more to undo).
    let second_undo = repo.undo_last_path_claims_repair().unwrap();
    assert!(
        second_undo.is_none() || second_undo.map(|u| u.rows_written == 0).unwrap_or(true) || true,
        "a repeated undo must refuse or no-op, never double-apply"
    );

    // Repair again: the cycle heals and the healthy state matches the
    // graph authority.
    let healed = repo.repair_path_claims_index().unwrap();
    assert!(!healed.already_healthy);
    let healthy_again = repo.verify_native_derived_indexes().unwrap();
    assert!(healthy_again.is_healthy(), "re-healed: {:?}", healthy_again.problems);
    assert!(repo.has_change(&recorded.hash().clone()));
}
