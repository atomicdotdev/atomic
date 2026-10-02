use atomic_core::change::session::{SessionManifest, SessionTurn};
use atomic_core::change::ProvenanceGraph;
use atomic_core::pristine::{tables, MutTxnT};
use atomic_core::types::Hash;
use atomic_repository::redb_change_store::{
    ProvenanceCheckpointSource, ProvenanceTurnState, RedbChangeStore,
};
use atomic_repository::Repository;

#[test]
fn acknowledgements_require_published_ledger_and_exact_attempt_generation() {
    let temp = tempfile::tempdir().unwrap();
    let repository = Repository::init(temp.path()).unwrap();
    let store = repository.redb_change_store().unwrap();
    let reserved = store
        .reserve_provenance_turn("fenced-publication", 1, 1)
        .unwrap();
    let graph = ProvenanceGraph::builder("fenced-publication", "codex")
        .timestamp(2)
        .build();
    let hash = Hash::of(&graph.serialize().unwrap());
    let turn = SessionTurn {
        session_id: "fenced-publication".into(),
        turn_number: 0,
        goal: None,
        provenance_hash: hash,
        change_hashes: vec![],
        previous_provenance: None,
        timestamp: graph.timestamp,
        plan_id: None,
        todos: vec![],
    };
    let prepared = store
        .prepare_provenance_checkpoint(
            reserved.provenance_id,
            reserved.generation,
            ProvenanceCheckpointSource {
                agent_name: "codex".into(),
                agent_display_name: "Codex".into(),
                agent_vendor: "openai".into(),
                change_hashes: vec![],
                previous_provenance: None,
                plan_id: None,
                ledger_turn_number: 0,
            },
            2,
        )
        .unwrap();
    store
        .bind_provenance_checkpoint_hash(
            reserved.provenance_id,
            prepared.attempt_generation,
            hash,
            turn.clone(),
            3,
        )
        .unwrap();

    assert!(store
        .acknowledge_provenance_checkpoint(
            reserved.provenance_id,
            prepared.attempt_generation,
            Hash::of(b"not published"),
            4,
        )
        .is_err());

    // Even a valid content-addressed manifest is insufficient without the
    // corresponding immutable ledger row in this repository.
    let manifest = SessionManifest {
        schema_version: 2,
        session_id: turn.session_id.clone(),
        goal_provenance: None,
        turns: vec![turn.clone()],
        parent_session: None,
        fork_turn: None,
    };
    let manifest_hash = manifest.content_hash();
    let txn = repository.pristine().write_txn().unwrap();
    txn.redb_transaction()
        .open_table(tables::SESSION_MANIFESTS)
        .unwrap()
        .insert(manifest_hash.as_bytes(), manifest.to_bytes().as_slice())
        .unwrap();
    txn.commit().unwrap();
    assert!(store
        .acknowledge_provenance_checkpoint(
            reserved.provenance_id,
            prepared.attempt_generation,
            manifest_hash,
            4,
        )
        .is_err());
    assert_eq!(
        store
            .get_provenance_turn(reserved.provenance_id)
            .unwrap()
            .unwrap()
            .state,
        ProvenanceTurnState::Checkpointing
    );

    let publication = repository
        .publish_bound_provenance_checkpoint(
            &graph,
            turn.clone(),
            reserved.provenance_id,
            prepared.attempt_generation,
            5,
        )
        .unwrap();
    let completed = store
        .get_provenance_turn(reserved.provenance_id)
        .unwrap()
        .unwrap();
    assert_eq!(completed.state, ProvenanceTurnState::Completed);
    assert!(store
        .acknowledge_provenance_checkpoint(
            reserved.provenance_id,
            prepared.attempt_generation + 1,
            publication.manifest_hash,
            6,
        )
        .is_err());
    assert_eq!(
        store
            .acknowledge_provenance_checkpoint(
                reserved.provenance_id,
                prepared.attempt_generation,
                publication.manifest_hash,
                6,
            )
            .unwrap(),
        completed
    );

    let read = repository.pristine().read_txn().unwrap();
    assert_eq!(
        RedbChangeStore::completed_provenance_checkpoint_for_in_txn(
            read.redb_transaction(),
            "fenced-publication",
            1,
        )
        .unwrap(),
        Some(turn)
    );
}
