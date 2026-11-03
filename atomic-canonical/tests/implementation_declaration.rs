use atomic_canonical::{lift::lift_intent, node::Ref, validate_intent, vocab};
use serde_json::json;

fn body(target: &str, evidence: &str) -> String {
    format!(":::why\nRetain an honest explicit implementation claim.\n:::\n::ref{{to={target} edge=declaresImplementation declarationEvidence=\"{evidence}\"}}\n")
}
fn fm() -> serde_json::Map<String, serde_json::Value> {
    json!({"id":"TEST::alice::1","uid":"01ARZ3NDEKTSV4RRFFQ69G5FAV","title":"Repair claim","status":"in_progress"}).as_object().unwrap().clone()
}

#[test]
fn declaration_is_signed_with_its_evidence_and_is_not_a_dependency() {
    use atomic_identity::{identity::Identity, keypair::KeyPair};
    let target = format!("urn:atomic:change:blake3-256:{}", "A".repeat(52));
    let key = KeyPair::generate();
    let identity = Identity::new("alice", &key);
    let signed = atomic_canonical::lift_and_attest(
        &fm(),
        &body(
            &target,
            "Operator repaired a failed child publication; original plan is unknown.",
        ),
        &identity,
        &key,
    )
    .unwrap();
    assert!(validate_intent(&signed).conforms);
    assert!(!vocab::is_known_dependency_edge("declaresImplementation"));
    let bytes = serde_json::to_vec(&signed).unwrap();
    let mut decoded = serde_json::from_slice::<atomic_canonical::CanonicalNode>(&bytes).unwrap();
    atomic_canonical::verify(&decoded, &key.public).unwrap();
    decoded.depends_on[0].declaration_evidence = Some("Forged evidence".into());
    assert!(atomic_canonical::verify(&decoded, &key.public).is_err());
    decoded.depends_on[0].declaration_evidence = None;
    assert!(validate_intent(&decoded)
        .results
        .iter()
        .any(|r| r.shape == "ImplementationDeclarationShape"));
}

#[test]
fn malformed_targets_and_missing_evidence_are_rejected() {
    let valid = format!("urn:atomic:change:blake3-256:{}", "A".repeat(52));
    for target in [
        "6FEJ".to_string(),
        format!("urn:atomic:change:blake3-256:{}", "a".repeat(52)),
        format!("urn:atomic:change:blake3-256:{}B", "A".repeat(51)),
        format!("{valid}#fragment"),
    ] {
        assert!(lift_intent(&fm(), &body(&target, "evidence")).is_err());
    }
    assert!(lift_intent(&fm(), &body(&valid, "  ")).is_err());
    let missing = format!("::ref{{to={valid} edge=declaresImplementation}}\n");
    assert!(lift_intent(&fm(), &missing).is_err());
}

#[test]
fn legacy_reference_json_remains_byte_shape_compatible() {
    let old = json!({"to":"urn:atomic:intent:old","edge":"depends"});
    let reference: Ref = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(serde_json::to_value(reference).unwrap(), old);
}
