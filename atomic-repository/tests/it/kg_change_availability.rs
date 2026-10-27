use atomic_core::{
    change::{Change, ChangeHeader, ProvenanceGraph},
    kg::{iri, ontology::Predicate as P},
    pristine::{KgMutTxnT, KgTxnT, MutTxnT},
};
use atomic_repository::Repository;

#[test]
fn ledger_reference_absent_then_arrived_preserves_identity_and_rebuilds() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_kg().unwrap();
    let change = Change::empty(ChangeHeader::new("arrived immutable header"));
    let mut bytes = Vec::new();
    let hash = change.serialize(&mut bytes).unwrap();
    let id = iri::mint_change(&hash);
    // Graph registration is not evidence that the immutable object exists.
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.register_change(&hash).unwrap();
        txn.commit().unwrap();
    }
    repo.save_provenance_graph(
        &ProvenanceGraph::builder("availability-session", "opencode")
            .add_change_explained(hash)
            .build(),
    )
    .unwrap();

    let assert_facts = |availability: &str, completeness: &str| {
        let node = repo.vault_kg_node(&id).unwrap().unwrap();
        let metadata = node.metadata.as_ref().unwrap();
        assert_eq!(metadata["object_availability"], availability);
        assert_eq!(metadata["projection_completeness"], completeness);
        assert_eq!(node.id, id);
        if completeness == "projected" {
            assert_eq!(node.summary.as_deref(), Some("arrived immutable header"));
            assert!(metadata.get("created_at").is_some());
        } else {
            assert!(metadata.get("created_at").is_none());
        }
        assert!(repo
            .workflow_snapshot()
            .unwrap()
            .edges
            .iter()
            .any(|edge| edge.kind == P::GeneratedChange && edge.to_id == id));
        assert!(!repo
            .workflow_snapshot()
            .unwrap()
            .edges
            .iter()
            .any(|edge| edge.kind == P::DirectlyOnView && edge.from_id == id));
    };
    assert!(!repo.has_change(&hash));
    assert_facts("absent", "reference-only");
    repo.rebuild_kg(true).unwrap();
    assert_facts("absent", "reference-only");

    // The core ledger projector cannot inspect the object store. Replaying it
    // alone must say unknown, rather than guessing absence from registration.
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.del_kg_node(&id).unwrap();
        for (record, turns) in txn.kg_session_rows().unwrap() {
            txn.project_session_kg(&record, &turns).unwrap();
        }
        let node = txn.get_kg_node(&id).unwrap().unwrap();
        assert_eq!(node.metadata.unwrap()["object_availability"], "unknown");
        txn.commit().unwrap();
    }
    for _ in 0..2 {
        repo.import_change_bytes(&hash, &bytes).unwrap();
        assert_facts("available", "projected");
    }
    repo.rebuild_kg(true).unwrap();
    assert_facts("available", "projected");

    // Ledger replay must not downgrade an already verified projection.
    repo.save_provenance_graph(
        &ProvenanceGraph::builder("availability-session", "opencode")
            .add_change_explained(hash)
            .build(),
    )
    .unwrap();
    assert_facts("available", "projected");
}

#[test]
fn unverified_object_existence_cannot_upgrade_availability() {
    use atomic_core::types::Base32;

    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_kg().unwrap();
    let mut bytes = Vec::new();
    let hash = Change::empty(ChangeHeader::new("expected object"))
        .serialize(&mut bytes)
        .unwrap();
    repo.save_provenance_graph(
        &ProvenanceGraph::builder("corrupt-availability", "opencode")
            .add_change_explained(hash)
            .build(),
    )
    .unwrap();
    let id = iri::mint_change(&hash);
    let encoded = hash.to_base32();
    let path = repo
        .changes_dir()
        .join(&encoded[..2])
        .join(format!("{encoded}.change"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut wrong_bytes = Vec::new();
    Change::empty(ChangeHeader::new("different valid object"))
        .serialize(&mut wrong_bytes)
        .unwrap();
    std::fs::write(path, wrong_bytes).unwrap();
    assert!(repo.has_change(&hash));
    assert!(repo.rebuild_kg(true).is_err());
    let node = repo.vault_kg_node(&id).unwrap().unwrap();
    assert_eq!(node.metadata.unwrap()["object_availability"], "absent");
    // A failed rebuild is atomic; only verified arrival can replace the last
    // successful store observation with available/projected.
    repo.import_change_bytes(&hash, &bytes).unwrap();
    let node = repo.vault_kg_node(&id).unwrap().unwrap();
    assert_eq!(node.metadata.unwrap()["object_availability"], "available");
}
