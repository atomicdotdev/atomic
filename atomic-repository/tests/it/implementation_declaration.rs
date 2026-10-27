use atomic_core::{
    change::{Change, ChangeHeader},
    kg::{iri, ontology::Predicate as P},
    pristine::VaultEntryType,
};
use atomic_repository::Repository;

const UID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

#[test]
fn explicit_declaration_projects_and_rebuilds_without_manufacturing_captured_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    let mut bytes = vec![];
    let hash = Change::empty(ChangeHeader::new("Mixed parent change"))
        .serialize(&mut bytes)
        .unwrap();
    let change = iri::mint_change(&hash);
    let path = format!("intents/{UID}/intent.md");
    let evidence = "Explicit author repair; original child publication failed, parent change includes implementation.";
    repo.vault_store(&path, VaultEntryType::Intent,
        format!(":::why\nDeclare without inventing an original turn.\n:::\n::ref{{to={change} edge=declaresImplementation declarationEvidence=\"{evidence}\"}}\n").into_bytes(),
        serde_json::json!({"uid":UID,"id":"TEST::alice::1","title":"Repair declaration","status":"in_progress"}).to_string()).unwrap();
    repo.vault_reconcile_intent_manifest().unwrap();
    let root = iri::mint_intent(&repo.project_id().unwrap(), UID).unwrap();
    let snapshot = repo.workflow_snapshot().unwrap();
    assert!(!snapshot
        .edges
        .iter()
        .any(|e| e.kind == P::DeclaresImplementation));
    assert!(snapshot
        .nodes
        .iter()
        .any(|n| n.label == "UnresolvedReference"));
    repo.import_change_bytes(&hash, &bytes).unwrap();
    for _ in 0..2 {
        repo.rebuild_kg(true).unwrap();
        let graph = repo.intent_graph(&root, 100).unwrap();
        assert_eq!(graph.implementing_changes, vec![change.clone()]);
        assert_eq!(graph.declared_changes, vec![change.clone()]);
        assert!(graph.captured_changes.is_empty());
        let edge = graph
            .edges
            .iter()
            .find(|e| e.kind == P::DeclaresImplementation)
            .unwrap();
        assert_eq!(
            edge.metadata.as_ref().unwrap()["declarationEvidence"],
            evidence
        );
        assert!(!graph
            .edges
            .iter()
            .any(|e| matches!(e.kind, P::Implements | P::HasPlan | P::GeneratedChange)));
        assert!(graph
            .diagnostics
            .iter()
            .any(|d| d.contains("do not repair or prove")));
    }
    assert_eq!(
        repo.load_change(&hash).unwrap().hashed.header.message,
        "Mixed parent change"
    );
}
