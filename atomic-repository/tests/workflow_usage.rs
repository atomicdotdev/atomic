use atomic_core::{
    change::{AITool, AIVendor, Change, ChangeHeader, Provenance, ProvenanceGraph, TokenUsage},
    kg::{iri, ontology::Predicate as P},
    pristine::{GraphTxnT, KgMutTxnT, KgTxnT, MutTxnT, VaultEntryType, ViewTxnT},
};
use atomic_repository::{intent_graph::IntentGraph, Repository};

#[test]
fn legacy_projected_changes_gain_usage_in_read_models_without_rebuilding_or_writing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let repo = Repository::init(root).unwrap();
    repo.init_vault().unwrap();
    let uid = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    repo.vault_store(&format!("intents/{uid}/intent.md"), VaultEntryType::Intent,
        b":::why\nInspect usage.\n:::\n".to_vec(),
        serde_json::json!({"uid":uid,"id":"TEST::tester::1","title":"Inspect usage","kind":"feature","status":"in_progress"}).to_string()).unwrap();
    repo.vault_reconcile_intent_manifest().unwrap();
    let mut change = Change::empty(ChangeHeader::new("Real usage record"));
    let mut provenance = Provenance::builder()
        .vendor(AIVendor::Other("openrouter".into()))
        .model("openai/gpt-6.1-sol")
        .tool(AITool::cli("opencode"))
        .request_id("usage-regression")
        .cost_usd(0.086176)
        .build();
    provenance.tokens = TokenUsage::full(54, 2177, 105, 500, 7);
    change.add_provenance(provenance);
    let mut bytes = vec![];
    let hash = change.serialize(&mut bytes).unwrap();
    repo.import_change_bytes(&hash, &bytes).unwrap();
    repo.save_provenance_graph(
        &ProvenanceGraph::builder("usage-session", "opencode")
            .plan_id(uid)
            .add_change_explained(hash)
            .build(),
    )
    .unwrap();
    repo.rebuild_kg(true).unwrap();
    let id = iri::mint_change(&hash);
    let intent = iri::mint_intent(&repo.project_id().unwrap(), uid).unwrap();
    let view = iri::mint_view(&repo.project_id().unwrap(), repo.current_view());
    // Simulate an older KG, with canonical immutable provenance still present.
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        let mut node = txn.get_kg_node(&id).unwrap().unwrap();
        for key in [
            "ai_usage_records",
            "ai_usage_record_version",
            "ai_usage_state",
            "ai_usage",
        ] {
            node.metadata
                .as_mut()
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
        }
        txn.upsert_kg_node(&node).unwrap();
        // Authoritative membership changes before its derived KG edges catch
        // up, as in the real demo. Usage must follow the canonical view filter.
        let mut current = txn.get_view(repo.current_view()).unwrap().unwrap();
        let change_id = txn.get_internal(&hash).unwrap().unwrap();
        txn.put_change(&mut current, change_id, &hash).unwrap();
        txn.update_view(&current).unwrap();
        txn.commit().unwrap();
    }
    let database = root.join(".atomic/atomic.redb");
    let before = std::fs::read(&database).unwrap();
    let snapshot = repo.workflow_snapshot().unwrap();
    assert!(
        !snapshot
            .edges
            .iter()
            .any(|edge| edge.from_id == id && edge.kind == P::DirectlyOnView),
        "usage hydration must not fabricate missing KG membership edges"
    );
    for subject in [&id, &intent, &view] {
        let node = snapshot
            .nodes
            .iter()
            .find(|node| &node.id == subject)
            .unwrap_or_else(|| panic!("missing usage subject {subject}"));
        let usage = &node.metadata.as_ref().unwrap()["ai_usage"];
        assert_eq!(usage["cost"]["amount_micros"], 86_176);
        assert_eq!(usage["tokens"]["input"], 54);
        assert_eq!(usage["tokens"]["output"], 2177);
        assert_eq!(usage["tokens"]["reasoning"], 105);
        assert_eq!(usage["tokens"]["total"], 2336);
        assert_eq!(usage["tokens"]["cache_read"], 500);
        assert_eq!(usage["tokens"]["cache_write"], 7);
        assert_eq!(usage["contributors"][0]["vendor"], "openrouter");
    }
    let capped = IntentGraph::from_snapshot(&snapshot, &intent, 1).unwrap();
    assert!(capped.truncated);
    assert_eq!(
        capped.nodes[0].metadata.as_ref().unwrap()["ai_usage"]["cost"]["amount"],
        "0.086176"
    );
    let mut selected = vec![repo.vault_kg_node(&view).unwrap().unwrap()];
    repo.enrich_workflow_usage(&mut selected).unwrap();
    assert_eq!(
        selected[0].metadata.as_ref().unwrap()["ai_usage"]["coverage"]["changes"],
        1
    );
    assert_eq!(std::fs::read(database).unwrap(), before);
    assert!(
        repo.vault_kg_node(&id)
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .get("ai_usage_records")
            .is_none(),
        "reading must not backfill stored rows"
    );
}
