use atomic_agent::{
    event::{ProvenanceJournalEnvelope, ProvenanceJournalEvent},
    provenance::accumulator::ProvenanceAccumulator,
};
use atomic_core::{change::ProvenanceGraph, kg::iri};
use atomic_repository::Repository;

#[test]
fn journal_actions_in_one_second_preserve_milliseconds_through_the_ledger_and_kg() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let goal = ProvenanceJournalEnvelope::new(
        "goal",
        "subsecond",
        1,
        1,
        1_792_920_379_100,
        ProvenanceJournalEvent::Goal {
            prompt: "Preserve precision".into(),
        },
    );
    let response = ProvenanceJournalEnvelope::new(
        "response",
        "subsecond",
        1,
        1,
        1_792_920_379_589,
        ProvenanceJournalEvent::Response {
            text: "Done".into(),
        },
    )
    .with_causal_parents(["goal"]);
    let accumulator =
        ProvenanceAccumulator::replay_provenance_journal(vec![response, goal]).unwrap();
    let prepared = accumulator.prepare_provenance_graph("opencode", "OpenCode", "openrouter", &[]);
    assert_eq!(prepared.graph.timestamp, 1_792_920_379_589);
    assert_eq!(prepared.graph.nodes[0].timestamp, 1_792_920_379_100);
    assert_eq!(prepared.graph.nodes[1].timestamp, 1_792_920_379_589);
    assert_eq!(
        prepared.graph.serialize().unwrap(),
        accumulator
            .prepare_provenance_graph("opencode", "OpenCode", "openrouter", &[])
            .graph
            .serialize()
            .unwrap()
    );
    let hash = repo.save_provenance_graph(&prepared.graph).unwrap();
    let (_, turns) = repo.get_session_ledger("subsecond").unwrap().unwrap();
    assert_eq!(turns[0].timestamp, 1_792_920_379_589);
    assert_eq!(turns[0].provenance_hash, hash);
    let id = iri::mint_turn(&repo.project_id().unwrap(), "subsecond", 0);
    let turn = repo.vault_kg_node(&id).unwrap().unwrap();
    assert_eq!(
        turn.metadata.unwrap()["created_at"],
        "2026-10-25T09:26:19.589+00:00"
    );
    assert_eq!(
        repo.pristine().read_txn().unwrap().kg_generation().unwrap(),
        2
    );
}

#[test]
fn mixed_unit_histories_keep_order_start_time_and_original_payloads() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let earlier = ProvenanceGraph::builder("mixed", "agent")
        .timestamp_millis(1_792_920_379_589)
        .build();
    let earlier_bytes = earlier.serialize().unwrap();
    let earlier_hash = repo.save_provenance_graph(&earlier).unwrap();
    // Import a legacy seconds record whose instant is later despite its
    // smaller raw integer. Causal edges must still take priority over clocks.
    let later = ProvenanceGraph::builder("mixed", "agent")
        .timestamp(1_792_920_380)
        .previous(earlier_hash)
        .build();
    let later_bytes = later.serialize().unwrap();
    let later_hash = repo.save_provenance_graph(&later).unwrap();
    let (record, turns) = repo.get_session_ledger("mixed").unwrap().unwrap();
    assert_eq!(record.started_at, earlier.timestamp);
    assert_eq!(turns[0].timestamp, earlier.timestamp);
    assert_eq!(turns[1].timestamp, later.timestamp);
    let graphs = repo.find_provenance_for_session("mixed").unwrap();
    assert_eq!(graphs[0].0, earlier_hash);
    assert_eq!(graphs[1].0, later_hash);
    assert_eq!(
        repo.load_provenance_graph(&earlier_hash)
            .unwrap()
            .serialize()
            .unwrap(),
        earlier_bytes
    );
    assert_eq!(
        repo.load_provenance_graph(&later_hash)
            .unwrap()
            .serialize()
            .unwrap(),
        later_bytes
    );
    // Session-list sorting must compare instants, not integer magnitudes.
    repo.save_provenance_graph(
        &ProvenanceGraph::builder("newest", "agent")
            .timestamp(1_792_920_381)
            .build(),
    )
    .unwrap();
    assert_eq!(
        repo.list_session_ledgers(2).unwrap()[0].0.session_id,
        "newest"
    );
}

#[test]
fn empty_session_lifecycle_starts_in_milliseconds() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let before = chrono::Utc::now().timestamp_millis();
    repo.upsert_session_lifecycle("empty", None, None, Some(1_792_920_379_589))
        .unwrap();
    let after = chrono::Utc::now().timestamp_millis();
    let (record, turns) = repo.get_session_ledger("empty").unwrap().unwrap();
    assert!(turns.is_empty());
    assert!((before..=after).contains(&record.started_at));
    assert_eq!(record.ended_at, Some(1_792_920_379_589));
    assert_eq!(
        record.activity_datetime(&turns).unwrap().to_rfc3339(),
        "2026-10-25T09:26:19.589+00:00"
    );
}
