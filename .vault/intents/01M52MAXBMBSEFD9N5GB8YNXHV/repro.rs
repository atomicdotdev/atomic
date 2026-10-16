//! Independent adversarial reproductions. Success means the reported bug was reproduced.
use atomic_core::{
    change::{Change, ChangeHeader},
    kg::{iri, ontology::Predicate as P},
    pristine::{MutTxnT, VaultEntryType},
    types::Base32,
};
use atomic_repository::Repository;

const UID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn referenced_child_is_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    let path = format!("intents/{UID}/intent.md");
    repo.vault_store(&path, VaultEntryType::Intent,
        b":::why\nOwner.\n:::\n:::acceptance-criterion{#target-ac status=met verifiedBy=review evidence=test}\nAuthoritative acceptance text.\n:::\n".to_vec(),
        serde_json::json!({"uid":UID,"id":"TEST::owner::1","title":"Owner","status":"in_progress"}).to_string()).unwrap();
    let id = iri::mint_child(&repo.project_id().unwrap(), UID, iri::ChildKind::AcceptanceCriterion, "target-ac").unwrap();
    let before = repo.vault_kg_node(&id).unwrap().unwrap();
    assert_eq!(before.metadata.as_ref().unwrap()["status"], "met");
    repo.vault_store("memory/reference.md", VaultEntryType::Memory, b"Reference only.".to_vec(),
        serde_json::json!({"derivedFrom":["urn:atomic:ac:target-ac"]}).to_string()).unwrap();
    let after = repo.vault_kg_node(&id).unwrap().unwrap();
    assert_ne!(before, after);
    assert_eq!(after.source, "vault_reference");
    assert_eq!(after.metadata.as_ref().unwrap()["derived_from_vault_path"], "memory/reference.md");
    assert!(after.metadata.as_ref().unwrap().get("status").is_none());
    println!("REPRO R1: reference overwrote owner AC: before={before:?}; after={after:?}");
    repo.rebuild_kg(true).unwrap();
    let rebuilt = repo.vault_kg_node(&id).unwrap().unwrap();
    assert!(rebuilt.metadata.as_ref().unwrap().get("status").is_none());
    println!("REPRO R1: forced rebuild also loses authoritative AC status");
}

fn late_change_does_not_repair_reference() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    let change = Change::new(ChangeHeader::new("late dependency"), vec![], vec![], vec![]);
    let mut bytes = Vec::new();
    let hash = change.serialize(&mut bytes).unwrap();
    repo.vault_store("memory/source.md", VaultEntryType::Memory, b"Source.".to_vec(),
        serde_json::json!({"derivedFrom":[format!("urn:atomic:change:{}", hash.to_base32())]}).to_string()).unwrap();
    let project = repo.project_id().unwrap();
    let source = iri::mint_memory(&project, "source");
    let finding = iri::mint_finding(&project, "UnresolvedReference", &source);
    let has_link = || repo.workflow_snapshot().unwrap().edges.iter().any(|edge|
        edge.from_id == source && edge.to_id == iri::mint_change(&hash) && edge.kind == P::ProvWasDerivedFrom);
    assert!(repo.vault_kg_node(&finding).unwrap().is_some());
    repo.import_change_bytes(&hash, &bytes).unwrap();
    assert!(repo.vault_kg_node(&iri::mint_change(&hash)).unwrap().is_some());
    assert!(!has_link());
    assert!(repo.vault_kg_node(&finding).unwrap().is_some());
    println!("REPRO R2: received change is projected, but existing vault reference remains unresolved");
    repo.vault_reindex_kg().unwrap();
    assert!(has_link());
    assert!(repo.vault_kg_node(&finding).unwrap().is_none());
    println!("REPRO R2: explicit vault reindex repairs the same authoritative data");
}

fn namespace_identity_case_collision() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    repo.vault_store("memory/A.md", VaultEntryType::Memory, b"Uppercase target".to_vec(), "{}".into()).unwrap();
    repo.vault_store("memory/a.md", VaultEntryType::Memory, b"Lowercase target".to_vec(), "{}".into()).unwrap();
    let project = repo.project_id().unwrap();
    let canonical = iri::mint_memory(&project, "A");
    repo.vault_store("memory/source.md", VaultEntryType::Memory, b"Source".to_vec(),
        serde_json::json!({"derivedFrom":[canonical]}).to_string()).unwrap();
    let edges = repo.workflow_snapshot().unwrap().edges;
    let edge = edges.iter().find(|e| e.from_id == iri::mint_memory(&project, "source") && e.kind == P::ProvWasDerivedFrom).unwrap();
    assert_eq!(edge.to_id, iri::mint_memory(&project, "a"));
    println!("REPRO R3: exact canonical IRI {} was redirected to {}", canonical, edge.to_id);
}

fn publication_rejects_invalid_signature_fixture() {
    use atomic_core::{kg::ontology::Class as C, KgEdge, KgNode, types::Hash};
    use atomic_shacl::{Profile, ValidationRequest};
    let attestation = iri::mint_attestation(&Hash::of(b"bad signature"));
    let did = format!("did:atomic:{}", Hash::of(b"identity fingerprint").to_base32());
    let nodes = [KgNode::new(&attestation, C::Attestation, "bad", "review"), KgNode::new(&did, C::Identity, "signer", "review")];
    let edges = [KgEdge::new(&attestation, &did, P::SignedBy), KgEdge::new(&attestation, "https://atomic.dev/ns/workflow#Failed", P::VerificationResult)];
    let data = atomic_shacl::snapshot_ntriples(&nodes, &edges).unwrap();
    let output = atomic_shacl::validate_ntriples(&data, "review-negative-fixture", &ValidationRequest { profile: Profile::Publication, ..Default::default() }).unwrap();
    assert!(!output.conforms);
    assert!(!output.results.is_empty());
    println!("PASS: publication rejects explicit Failed signature status");
}

fn offline_verification_rejects_forgery_and_poisoned_cache() {
    use atomic_core::{change::{AttestAgent, Attestation}, types::Hash};
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let key = atomic_identity::KeyPair::generate();
    let wrong_key = atomic_identity::KeyPair::generate();
    let did = format!("did:atomic:{}", Hash::of(key.public.as_bytes()).to_base32());
    let mut attestation = Attestation::builder("review-session", AttestAgent::new("review", "Review", "review"))
        .add_change(Hash::of(b"covered")).build();
    attestation.sign_with_did(&did, key.secret.as_bytes());
    let hash = repo.save_attestation(&attestation).unwrap();
    assert!(repo.cache_signer_key(&did, &wrong_key.public).is_err());
    // Even a raw poisoned cache row must be checked again at use time.
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_signer_key(&did, &wrong_key.public.to_base32()).unwrap();
        txn.commit().unwrap();
    }
    repo.rebuild_kg(true).unwrap();
    let status = |hash| repo.workflow_snapshot().unwrap().edges.into_iter().find(|edge|
        edge.from_id == iri::mint_attestation(&hash) && edge.kind == P::VerificationResult).unwrap().to_id;
    assert!(status(hash).ends_with("#Unverifiable"));
    repo.cache_signer_key(&did, &key.public).unwrap();
    assert!(status(hash).ends_with("#Verified"));
    attestation.notes = Some("tampered after signing".into());
    let forged = repo.save_attestation(&attestation).unwrap();
    assert!(status(forged).ends_with("#Failed"));
    println!("PASS: offline verifier rejects poisoned cache and signed-payload tampering; valid key verifies");
}

fn main() {
    referenced_child_is_overwritten();
    late_change_does_not_repair_reference();
    namespace_identity_case_collision();
    publication_rejects_invalid_signature_fixture();
    offline_verification_rejects_forgery_and_poisoned_cache();
}
