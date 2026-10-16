use atomic_core::{
    change::{Change, ChangeHeader, ProvenanceGraph},
    kg::{
        iri,
        ontology::{Class as C, Predicate as P},
    },
    pristine::{KgTxnT, VaultEntryType},
    types::Merkle,
};
use atomic_repository::{Repository, UnrecordOptions, ViewManifest};

const UID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

#[test]
fn later_intent_ingestion_repairs_session_links_and_deletion_restores_gap_findings() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    let hash = atomic_core::types::Hash::of(b"historical generated change");
    repo.save_provenance_graph(
        &ProvenanceGraph::builder("late-plan", "opencode")
            .plan_id(UID)
            .add_change_explained(hash)
            .build(),
    )
    .unwrap();
    let project = repo.project_id().unwrap();
    let change = iri::mint_change(&hash);
    let missing = iri::mint_finding(&project, "MissingPlanId", &change);
    assert!(repo.vault_kg_node(&missing).unwrap().is_some());
    let path = format!("intents/{UID}/intent.md");
    repo.vault_store(&path, VaultEntryType::Intent, b":::why\nResolve an old turn.\n:::\n".to_vec(),
        serde_json::json!({"uid":UID,"id":"TEST::alice::1","title":"Late plan","status":"in_progress","kind":"feature"}).to_string()).unwrap();
    repo.vault_reconcile_intent_manifest().unwrap();
    repo.vault_reindex_kg().unwrap();
    let intent = iri::mint_intent(&project, UID).unwrap();
    assert!(repo
        .workflow_snapshot()
        .unwrap()
        .edges
        .iter()
        .any(|edge| edge.from_id == change && edge.kind == P::Implements && edge.to_id == intent));
    assert!(repo.vault_kg_node(&missing).unwrap().is_none());
    repo.vault_delete(&path).unwrap();
    assert!(repo.vault_kg_node(&intent).unwrap().is_none());
    assert!(repo.vault_kg_node(&missing).unwrap().is_some());
    assert!(!repo
        .workflow_snapshot()
        .unwrap()
        .edges
        .iter()
        .any(|edge| edge.from_id == change && edge.kind == P::Implements));
}

#[test]
fn legacy_memory_references_resolve_case_insensitively_and_repair_when_targets_arrive() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    repo.vault_store(
        "memory/source.md",
        VaultEntryType::Memory,
        b"source".to_vec(),
        serde_json::json!({"derivedFrom":["urn:atomic:memory:TaRgEt"]}).to_string(),
    )
    .unwrap();
    let project = repo.project_id().unwrap();
    let source = iri::mint_memory(&project, "source");
    assert!(repo
        .vault_kg_node(&iri::mint_finding(&project, "UnresolvedReference", &source))
        .unwrap()
        .is_some());
    repo.vault_store(
        "memory/target.md",
        VaultEntryType::Memory,
        b"target".to_vec(),
        "{}".into(),
    )
    .unwrap();
    let snapshot = repo.workflow_snapshot().unwrap();
    assert!(snapshot.edges.iter().any(|edge| edge.from_id == source
        && edge.to_id == iri::mint_memory(&project, "target")
        && edge.kind == P::ProvWasDerivedFrom));
    assert!(repo
        .vault_kg_node(&iri::mint_finding(&project, "UnresolvedReference", &source))
        .unwrap()
        .is_none());
    assert!(snapshot
        .nodes
        .iter()
        .all(|node| node.types.iter().all(|class| C::from_iri(class).is_some())));
}

#[test]
fn membership_only_manifest_unrecord_and_reinsert_update_direct_view_facts() {
    let directory = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();
    repo.init_kg().unwrap();
    repo.create_shared_view("mirror").unwrap();
    let change = Change::new(
        ChangeHeader::new("Already stored object"),
        vec![],
        vec![],
        vec![],
    );
    let mut bytes = Vec::new();
    let hash = change.serialize(&mut bytes).unwrap();
    repo.import_change_bytes(&hash, &bytes).unwrap();
    let project = repo.project_id().unwrap();
    let change_id = iri::mint_change(&hash);
    let view = iri::mint_view(&project, "mirror");
    let membership = |repo: &Repository| {
        repo.workflow_snapshot().unwrap().edges.iter().any(|edge| {
            edge.from_id == change_id && edge.to_id == view && edge.kind == P::DirectlyOnView
        })
    };
    assert!(!membership(&repo));
    let manifest = ViewManifest {
        name: "mirror".into(),
        scope: atomic_core::pristine::ViewScope::Shared,
        parent: None,
        changes: vec![hash],
        state: Merkle::ZERO.next(&hash),
    };
    assert_eq!(repo.reconcile_view_manifest(&manifest).unwrap().replayed, 1);
    assert!(membership(&repo));
    assert_eq!(repo.reconcile_view_manifest(&manifest).unwrap().replayed, 0);
    repo.unrecord(
        &hash,
        UnrecordOptions {
            view: Some("mirror".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!membership(&repo));
    assert!(repo
        .pristine()
        .read_txn()
        .unwrap()
        .get_kg_node(&change_id)
        .unwrap()
        .is_some());
    repo.reinsert_change_on_view("mirror", &hash, None).unwrap();
    assert!(membership(&repo));
}
