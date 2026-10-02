//! Exercise real process death around repository publication, then reopen redb.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use atomic_core::change::session::SessionTurn;
use atomic_core::change::{ChangeHeader, ProvenanceGraph};
use atomic_core::pristine::tables::*;
use atomic_core::pristine::{GraphTxnT, MutTxnT, ViewTxnT};
use atomic_core::types::Hash;
use atomic_repository::history::HistoryOptions;
use atomic_repository::record::RecordOptions;
use atomic_repository::redb_change_store::{ProvenanceCheckpointSource, ProvenanceTurnState};
use atomic_repository::Repository;
use redb::ReadableTableMetadata;

fn graph() -> ProvenanceGraph {
    ProvenanceGraph::builder("atomic-checkpoint", "codex")
        .timestamp(1_000)
        .build()
}

fn turn(graph: &ProvenanceGraph) -> SessionTurn {
    SessionTurn {
        session_id: graph.session_id.clone(),
        turn_number: 0,
        goal: None,
        provenance_hash: Hash::of(&graph.serialize().unwrap()),
        change_hashes: graph.changes_explained.clone(),
        previous_provenance: graph.previous,
        timestamp: graph.timestamp,
        plan_id: graph.plan_id.clone(),
        todos: graph.todos.clone(),
    }
}

fn rows<K: redb::Key + 'static, V: redb::Value + 'static>(
    repo: &Repository,
    definition: redb::TableDefinition<K, V>,
) -> u64 {
    let txn = repo.pristine().read_txn().unwrap();
    match txn.redb_transaction().open_table(definition) {
        Ok(table) => table.len().unwrap(),
        Err(redb::TableError::TableDoesNotExist(_)) => 0,
        Err(error) => panic!("{error}"),
    }
}

fn graph_rows(repo: &Repository) -> u64 {
    let txn = repo.pristine().read_txn().unwrap();
    txn.redb_transaction()
        .open_multimap_table(GRAPH)
        .unwrap()
        .len()
        .unwrap()
}

// This helper is invoked in a separate process: no global environment mutation
// and no destructor rollback on the crash path.
#[test]
fn publication_child() {
    let Some(root) = std::env::var_os("ATOMIC_PUBLICATION_TEST_ROOT") else {
        return;
    };
    let repo = Repository::open_existing(root).unwrap();
    let result = if std::env::var("ATOMIC_PUBLICATION_TEST_KIND").unwrap() == "record" {
        repo.record(
            ChangeHeader::builder().message("atomic record").build(),
            RecordOptions::default(),
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
    } else {
        let graph = graph();
        let store = repo.redb_change_store().unwrap();
        let reserved = store
            .get_provenance_turn_for(&graph.session_id, 1)
            .unwrap()
            .unwrap();
        let generation = reserved
            .checkpoint_attempt
            .as_ref()
            .unwrap()
            .attempt_generation;
        repo.publish_bound_provenance_checkpoint(
            &graph,
            turn(&graph),
            reserved.provenance_id,
            generation,
            9,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
    };
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("injected publication failure"));
}

fn run_child(root: &std::path::Path, kind: &str, point: &str, crash: bool) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "publication_child", "--nocapture"])
        .env("ATOMIC_PUBLICATION_TEST_ROOT", root)
        .env("ATOMIC_PUBLICATION_TEST_KIND", kind)
        .env("ATOMIC_PUBLICATION_FAILPOINT", point)
        .env(
            "ATOMIC_PUBLICATION_FAILPOINT_ACTION",
            if crash { "exit" } else { "error" },
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "{point} timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(if crash { 86 } else { 0 }),
        "{point}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn record_rolls_back_all_tables_on_error_and_process_exit() {
    for crash in [false, true] {
        for point in [
            "record-after-object",
            "record-after-graph",
            "record-before-commit",
            "record-after-commit",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let repo = Repository::init(temp.path()).unwrap();
            std::fs::write(temp.path().join("file.txt"), "durable content\n").unwrap();
            repo.add("file.txt", Default::default()).unwrap();
            let original_graph_rows = graph_rows(&repo);
            drop(repo);
            run_child(temp.path(), "record", point, crash);
            let repo = Repository::open_existing(temp.path()).unwrap();
            let committed = point == "record-after-commit";
            let count = u64::from(committed);
            assert_eq!(rows(&repo, CHANGE_BYTES), count, "{point}");
            assert_eq!(rows(&repo, CHANGE_META), count, "{point}");
            assert_eq!(rows(&repo, REPOSITORY_OUTBOX), count, "{point}");
            assert_eq!(rows(&repo, REPOSITORY_OUTBOX_KEYS), count, "{point}");
            let history = repo.log(HistoryOptions::default()).unwrap();
            assert_eq!(history.len(), count as usize, "{point}");
            if !committed {
                assert_eq!(graph_rows(&repo), original_graph_rows, "{point}");
            }
            let txn = repo.pristine().read_txn().unwrap();
            assert_eq!(
                txn.get_view(repo.current_view())
                    .unwrap()
                    .unwrap()
                    .change_count,
                count
            );
            if committed {
                let hash = history[0].hash;
                assert!(txn.get_internal(&hash).unwrap().is_some());
                assert!(
                    !repo.change_store().change_path(&hash).exists(),
                    "crash precedes export"
                );
                assert_eq!(
                    repo.get_file_content("file.txt").unwrap().unwrap(),
                    b"durable content\n"
                );
                assert_eq!(
                    repo.load_change(&hash).unwrap().hashed.header.message,
                    "atomic record"
                );
            }
        }
    }
}

fn prepare_checkpoint(repo: &Repository) {
    let store = repo.redb_change_store().unwrap();
    let reserved = store
        .reserve_provenance_turn("atomic-checkpoint", 1, 1)
        .unwrap();
    let source = ProvenanceCheckpointSource {
        agent_name: "codex".into(),
        agent_display_name: "Codex".into(),
        agent_vendor: "openai".into(),
        change_hashes: vec![],
        previous_provenance: None,
        plan_id: None,
        ledger_turn_number: 0,
    };
    let attempt = store
        .prepare_provenance_checkpoint(reserved.provenance_id, reserved.generation, source, 2)
        .unwrap();
    let graph = graph();
    let turn = turn(&graph);
    store
        .bind_provenance_checkpoint_hash(
            reserved.provenance_id,
            attempt.attempt_generation,
            turn.provenance_hash,
            turn,
            3,
        )
        .unwrap();
}

#[test]
fn checkpoint_completion_object_ledger_and_outbox_commit_together() {
    for crash in [false, true] {
        for point in [
            "checkpoint-after-object",
            "checkpoint-after-ledger",
            "checkpoint-before-commit",
            "checkpoint-after-commit",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let repo = Repository::init(temp.path()).unwrap();
            prepare_checkpoint(&repo);
            drop(repo);
            run_child(temp.path(), "checkpoint", point, crash);
            let repo = Repository::open_existing(temp.path()).unwrap();
            let committed = point == "checkpoint-after-commit";
            let count = u64::from(committed);
            for actual in [
                rows(&repo, PROVENANCE_OBJECTS),
                rows(&repo, SESSION_TURNS),
                rows(&repo, SESSION_HEADS),
                rows(&repo, SESSION_MANIFESTS),
                rows(&repo, SESSION_CHECKPOINT_RECEIPTS),
                rows(&repo, REPOSITORY_OUTBOX),
            ] {
                assert_eq!(actual, count, "{point}");
            }
            let store = repo.redb_change_store().unwrap();
            let reserved = store
                .get_provenance_turn_for("atomic-checkpoint", 1)
                .unwrap()
                .unwrap();
            assert_eq!(
                matches!(reserved.state, ProvenanceTurnState::Completed),
                committed,
                "{point}"
            );
            let graph = graph();
            let turn = turn(&graph);
            assert_eq!(
                repo.pristine()
                    .read_txn()
                    .unwrap()
                    .get_internal(&turn.provenance_hash)
                    .unwrap()
                    .is_some(),
                committed
            );
            if committed {
                assert!(!repo
                    .change_store()
                    .provenance_path(&turn.provenance_hash)
                    .exists());
                assert_eq!(
                    repo.load_provenance_graph(&turn.provenance_hash)
                        .unwrap()
                        .session_id,
                    graph.session_id
                );
            }
            // The same prepared publication is safe after either rollback or a lost response.
            let generation = reserved
                .checkpoint_attempt
                .as_ref()
                .unwrap()
                .attempt_generation;
            let publication = repo
                .publish_bound_provenance_checkpoint(
                    &graph,
                    turn.clone(),
                    reserved.provenance_id,
                    generation,
                    10,
                )
                .unwrap();
            let retry = repo
                .publish_bound_provenance_checkpoint(
                    &graph,
                    turn,
                    reserved.provenance_id,
                    generation,
                    11,
                )
                .unwrap();
            assert_eq!(publication, retry);
            assert_eq!(rows(&repo, REPOSITORY_OUTBOX), 1);
        }
    }
}

#[test]
fn old_checkpoint_retry_returns_original_receipt_without_rewinding_head() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let a = graph();
    let first = repo.publish_provenance_checkpoint(&a, turn(&a)).unwrap();
    let b = ProvenanceGraph::builder("atomic-checkpoint", "codex")
        .timestamp(2_000)
        .previous(first.turn.provenance_hash)
        .build();
    let second = repo.publish_provenance_checkpoint(&b, turn(&b)).unwrap();
    drop(repo);
    let repo = Repository::open_existing(temp.path()).unwrap();
    assert_eq!(
        repo.publish_provenance_checkpoint(&a, turn(&a)).unwrap(),
        first
    );
    assert_eq!(
        repo.get_session_head("atomic-checkpoint").unwrap(),
        Some(second.manifest_hash)
    );
    assert_eq!(rows(&repo, REPOSITORY_OUTBOX), 2);
    assert_eq!(rows(&repo, SESSION_TURNS), 2);
    let mut conflicting = turn(&a);
    conflicting.goal = Some("changed retry".into());
    assert!(repo.publish_provenance_checkpoint(&a, conflicting).is_err());
    assert_eq!(rows(&repo, REPOSITORY_OUTBOX), 2);
    // Upgrade compatibility: older publications have manifests but no receipt.
    let txn = repo.pristine().write_txn().unwrap();
    txn.redb_transaction()
        .delete_table(SESSION_CHECKPOINT_RECEIPTS)
        .unwrap();
    txn.commit().unwrap();
    assert_eq!(
        repo.publish_provenance_checkpoint(&a, turn(&a)).unwrap(),
        first
    );
    assert_eq!(
        repo.get_session_head("atomic-checkpoint").unwrap(),
        Some(second.manifest_hash)
    );
}

#[test]
fn publication_rejects_stale_fence_and_mismatched_graph_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    prepare_checkpoint(&repo);
    let store = repo.redb_change_store().unwrap();
    let reserved = store
        .get_provenance_turn_for("atomic-checkpoint", 1)
        .unwrap()
        .unwrap();
    let generation = reserved.checkpoint_attempt.unwrap().attempt_generation;
    let graph = graph();
    assert!(repo
        .publish_bound_provenance_checkpoint(
            &graph,
            turn(&graph),
            reserved.provenance_id,
            generation + 1,
            10
        )
        .is_err());
    let mut wrong = turn(&graph);
    wrong.provenance_hash = Hash::of(b"wrong");
    assert!(repo
        .publish_bound_provenance_checkpoint(&graph, wrong, reserved.provenance_id, generation, 10)
        .is_err());
    assert_eq!(rows(&repo, PROVENANCE_OBJECTS), 0);
    assert_eq!(rows(&repo, SESSION_TURNS), 0);
    assert_eq!(rows(&repo, REPOSITORY_OUTBOX), 0);
}

#[test]
fn record_revalidates_prepared_closure_before_storing_the_object() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    std::fs::write(temp.path().join("file"), b"prepared version").unwrap();
    repo.add("file", Default::default()).unwrap();
    let prepared = repo
        .record(
            ChangeHeader::new("prepared"),
            RecordOptions::default()
                .save_to_store(false)
                .apply_after_record(false),
        )
        .unwrap();
    std::fs::write(temp.path().join("file"), b"intervening version").unwrap();
    repo.record(ChangeHeader::new("intervening"), RecordOptions::default())
        .unwrap();
    let error = repo
        .write_recorded(&prepared, Default::default())
        .unwrap_err();
    assert!(
        error.to_string().contains("preparation closure changed"),
        "{error}"
    );
    assert!(!repo.has_change(prepared.hash()));
    assert_eq!(rows(&repo, CHANGE_BYTES), 1);
    assert_eq!(rows(&repo, REPOSITORY_OUTBOX), 1);
    assert_eq!(
        repo.get_file_content("file").unwrap().unwrap(),
        b"intervening version"
    );
}
