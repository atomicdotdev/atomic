//! Real repository producer → snapshot → RDF → PurRDF integration.
use atomic_core::MutTxnT;
use atomic_core::{
    change::{AttestAgent, Attestation, Change, ChangeHeader, ProvenanceGraph},
    kg::{
        iri,
        ontology::{Class as C, Predicate as P},
    },
    pristine::{KgTxnT, VaultEntryType},
};
use atomic_identity::KeyPair;
use atomic_repository::{InsertOptions, Repository};
use atomic_shacl::*;

fn validate(repo: &Repository, profile: Profile) -> ValidationOutput {
    let snapshot = repo.workflow_snapshot().unwrap();
    let data = snapshot_ntriples(&snapshot.nodes, &snapshot.edges).unwrap();
    validate_ntriples(
        &data,
        &snapshot.identity,
        &ValidationRequest {
            profile,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn ingested_sidecars_close_history_without_inventing_view_membership() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    let parent = repo.current_view().to_owned();
    let uid = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    repo.vault_store(&format!("intents/{uid}/intent.md"), VaultEntryType::Intent,
        b":::why\nA real workflow fixture.\n:::\n".to_vec(),
        serde_json::json!({"uid":uid,"id":"TEST::alice::1","kind":"feature","status":"in_progress","title":"Workflow fixture"}).to_string()).unwrap();
    repo.vault_reconcile_intent_manifest().unwrap();
    repo.vault_reindex_kg().unwrap();
    let project = repo.project_id().unwrap();
    let intent = iri::mint_intent(&project, uid).unwrap();

    let change = Change::new(ChangeHeader::new("fixture change"), vec![], vec![], vec![]);
    let mut bytes = Vec::new();
    let hash = change.serialize(&mut bytes).unwrap();
    repo.import_change_bytes(&hash, &bytes).unwrap();
    let change_id = iri::mint_change(&hash);
    // Downloaded objects are globally present, but absent from every own log.
    assert!(!repo
        .workflow_snapshot()
        .unwrap()
        .edges
        .iter()
        .any(|edge| edge.from_id == change_id && edge.kind == P::DirectlyOnView));
    let graph = ProvenanceGraph::builder("fixture-session", "opencode")
        .plan_id("TEST::alice::1")
        .add_change_explained(hash)
        .build();
    let provenance = repo.save_provenance_graph(&graph).unwrap();
    let snapshot = repo.workflow_snapshot().unwrap();
    assert!(snapshot.edges.iter().any(|edge| edge.from_id == change_id
        && edge.kind == P::Implements
        && edge.to_id == intent));
    assert!(snapshot.edges.iter().any(|edge| edge.from_id == change_id
        && edge.kind == P::ChangeExplainedBy
        && edge.to_id == iri::mint_provenance(&provenance)));
    assert!(snapshot
        .nodes
        .iter()
        .any(|node| node.id == iri::mint_finding(&project, "MissingAttestation", &change_id)));
    let report = validate(&repo, Profile::Projection);
    assert!(report.conforms, "{}", report.report_ntriples);
    assert!(!validate(&repo, Profile::Publication).conforms);

    let key = KeyPair::generate();
    let did = atomic_canonical::did::did_for_public_key(&key.public);
    let mut attestation = Attestation::builder(
        "fixture-session",
        AttestAgent::new("fixture", "Fixture", "1"),
    )
    .add_change(hash)
    .provenance_root(provenance)
    .build();
    attestation.sign_with_did(&did, key.secret.as_bytes());
    let attest_hash = repo.save_attestation(&attestation).unwrap();
    let attest_id = iri::mint_attestation(&attest_hash);
    let status = |repo: &Repository| {
        repo.workflow_snapshot()
            .unwrap()
            .edges
            .into_iter()
            .find(|edge| edge.from_id == attest_id && edge.kind == P::VerificationResult)
            .unwrap()
            .to_id
    };
    assert!(status(&repo).ends_with("#Unverifiable"));
    assert!(repo
        .cache_signer_key(&did, &KeyPair::generate().public)
        .is_err());
    repo.cache_signer_key(&did, &key.public).unwrap();
    assert!(status(&repo).ends_with("#Verified"));
    let report = validate(&repo, Profile::Publication);
    assert!(report.conforms, "{}", report.report_ntriples);
    assert!(!repo
        .workflow_snapshot()
        .unwrap()
        .nodes
        .iter()
        .any(|node| node.id == iri::mint_finding(&project, "MissingAttestation", &change_id)));

    repo.create_draft_view("child", &parent).unwrap();
    repo.insert_change(&hash, InsertOptions::default().view(&parent))
        .unwrap();
    let edges = repo.workflow_snapshot().unwrap().edges;
    assert!(edges.iter().any(|edge| edge.from_id == change_id
        && edge.kind == P::DirectlyOnView
        && edge.to_id == iri::mint_view(&project, &parent)));
    assert!(!edges.iter().any(|edge| edge.from_id == change_id
        && edge.kind == P::DirectlyOnView
        && edge.to_id == iri::mint_view(&project, "child")));

    let before = repo.workflow_snapshot().unwrap();
    assert!(repo.rebuild_kg(true).unwrap());
    let after = repo.workflow_snapshot().unwrap();
    assert_eq!(
        before.identity, after.identity,
        "incremental and full projection must agree"
    );
    assert!(!repo.rebuild_kg(false).unwrap());
    let txn = repo.pristine().read_txn().unwrap();
    for node in &after.nodes {
        let mut types = node.types.clone();
        types.sort();
        assert_eq!(txn.kg_node_types(&node.id).unwrap(), types);
        assert!(node.types.iter().all(|class| C::from_iri(class).is_some()));
    }
    assert!(txn
        .get_kg_node(&iri::mint_turn(&project, "fixture-session", 0))
        .unwrap()
        .unwrap()
        .has_type(C::ManagedTurn));
}

#[test]
fn old_generation_is_fenced_and_aborted_rebuild_never_publishes_partial_state() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    repo.vault_store(
        "memory/upgrade.md",
        VaultEntryType::Memory,
        b"upgrade".to_vec(),
        "{}".into(),
    )
    .unwrap();
    let original = repo.workflow_snapshot().unwrap().identity;
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.begin_kg_rebuild().unwrap();
        // Dropping an uncommitted transaction is a simulated interrupted build.
    }
    assert_eq!(repo.workflow_snapshot().unwrap().identity, original);
    drop(repo);
    {
        let database = redb::Database::open(dir.path().join(".atomic/atomic.redb")).unwrap();
        let txn = database.begin_write().unwrap();
        txn.open_table(atomic_core::pristine::tables::KG_INDEX_META)
            .unwrap()
            .remove("kg_generation")
            .unwrap();
        txn.commit().unwrap();
    }
    let repo = Repository::open_readonly(dir.path()).unwrap();
    assert!(repo.list_views().is_ok());
    assert!(repo
        .workflow_snapshot()
        .unwrap_err()
        .to_string()
        .contains("KgNeedsRebuild"));
    drop(repo);
    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo
        .workflow_snapshot()
        .unwrap()
        .nodes
        .iter()
        .any(|node| node.has_type(C::Memory)));
    assert!(!repo.rebuild_kg(false).unwrap());
}

#[test]
fn incomplete_manifest_history_conforms_only_through_precise_gap_findings() {
    use atomic_core::{
        change::session::{SessionManifest, SessionTurn},
        types::Hash,
    };
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let change = Hash::of(b"historical change");
    let provenance = Hash::of(b"absent sidecar");
    repo.ingest_session_manifest(&SessionManifest {
        schema_version: 2,
        session_id: "historical".into(),
        goal_provenance: None,
        parent_session: None,
        fork_turn: None,
        turns: vec![SessionTurn {
            session_id: "historical".into(),
            turn_number: 0,
            goal: None,
            provenance_hash: provenance,
            change_hashes: vec![change],
            previous_provenance: None,
            timestamp: 1735689600,
            plan_id: None,
            todos: vec![],
            boundary_start: None,
            boundary_end: None,
            outcome: None,
        }],
    })
    .unwrap();
    repo.rebuild_session_index().unwrap();
    let snapshot = repo.workflow_snapshot().unwrap();
    let id = iri::mint_change(&change);
    for code in ["MissingPlanId", "MissingProvenance", "MissingAttestation"] {
        let finding = iri::mint_finding(&repo.project_id().unwrap(), code, &id);
        assert!(snapshot.edges.iter().any(|edge| edge.from_id == finding
            && edge.to_id == id
            && edge.kind == P::AboutResource));
    }
    assert!(!snapshot
        .nodes
        .iter()
        .any(|node| node.id == iri::mint_provenance(&provenance)));
    let report = validate(&repo, Profile::Projection);
    assert!(report.conforms, "{}", report.report_ntriples);
    assert!(!validate(&repo, Profile::Publication).conforms);
    let mut txn = repo.pristine().write_txn().unwrap();
    atomic_core::pristine::KgMutTxnT::del_kg_node(
        &mut txn,
        &iri::mint_finding(&repo.project_id().unwrap(), "MissingProvenance", &id),
    )
    .unwrap();
    txn.commit().unwrap();
    assert!(!validate(&repo, Profile::Projection).conforms);
}

#[test]
fn signed_vault_documents_project_typed_lineage_without_rewriting_signed_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    let uid = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let frontmatter = serde_json::json!({"uid":uid,"id":"TEST::alice::1","title":"Signed fixture","status":"backlog","kind":"bug"});
    let body = ":::why\nPreserve a signed representation.\n:::\n";
    repo.vault_store(
        &format!("intents/{uid}/intent.md"),
        VaultEntryType::Intent,
        body.as_bytes().to_vec(),
        frontmatter.to_string(),
    )
    .unwrap();
    let node = atomic_canonical::lift::lift_intent(frontmatter.as_object().unwrap(), body).unwrap();
    let key = KeyPair::generate();
    let prepared = atomic_canonical::proof::prepare_attestation(node.to_value(), &key.public);
    let signature = atomic_identity::Signer::new(&key).sign(&prepared.signing_bytes);
    let signed = atomic_canonical::proof::attach_proof(prepared.value, &key.public, &signature);
    let bytes = serde_json::to_vec_pretty(&signed).unwrap();
    let path = "attestations/signed/attested.md";
    repo.vault_store(
        path,
        VaultEntryType::Attestation,
        bytes.clone(),
        "{}".into(),
    )
    .unwrap();
    let subject = iri::mint_intent(&repo.project_id().unwrap(), uid).unwrap();
    let snapshot = repo.workflow_snapshot().unwrap();
    assert!(snapshot
        .nodes
        .iter()
        .find(|node| node.id == subject)
        .unwrap()
        .has_type(C::BugIntent));
    assert!(snapshot.edges.iter().any(|edge| edge.from_id == subject
        && edge.kind == P::AuthoredBy
        && edge.to_id == atomic_canonical::did::did_for_public_key(&key.public)));
    repo.rebuild_kg(true).unwrap();
    assert_eq!(
        repo.vault_retrieve(path).unwrap().unwrap().content_bytes,
        bytes
    );
    let node: atomic_canonical::CanonicalNode = serde_json::from_slice(&bytes).unwrap();
    atomic_canonical::verify(&node, &key.public).unwrap();
}
