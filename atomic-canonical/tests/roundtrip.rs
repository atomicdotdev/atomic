//! M0 end-to-end proof: author → lift → JCS/BLAKE3 hash → Ed25519 proof →
//! gate → render, entirely in-crate (no vault wiring).

use atomic_canonical::lift::{lift_intent, parse_markdown};
use atomic_canonical::{lift_and_attest, render, validate_intent, verify, Target};
use atomic_identity::identity::Identity;
use atomic_identity::keypair::KeyPair;
use serde_json::{json, Map, Value};

fn dev_identity() -> (Identity, KeyPair) {
    // M0 non-production dev identity (plaintext keys — see crate docs).
    let kp = KeyPair::generate();
    let id = Identity::new("lee", &kp);
    (id, kp)
}

fn frontmatter() -> Map<String, Value> {
    json!({
        "id": "WORD-5",
        "title": "Add name prompt modal",
        "status": "todo",
        "priority": "medium",
        "view": "proud-moon-a08a"
    })
    .as_object()
    .unwrap()
    .clone()
}

const BODY: &str = "\
:::why{}
We're choosing a local-only modal over a persisted profile. The name doesn't
need to survive a reload yet.
:::

:::acceptance-criterion{#WORD-5-ac-1 status=met verifiedBy=did:atomic:lee evidence=urn:atomic:change:01J8ZE7G2W}
On first load, the app presents a modal asking for the player's name.
:::

:::task{#WORD-5-1 status=done satisfies=WORD-5-ac-1}
Add name capture state, submit handling, modal markup, and top-of-page display.
::file-ref{path=src/App.tsx}
:::";

#[test]
fn full_round_trip_lifts_attests_gates_verifies_renders() {
    let (id, kp) = dev_identity();
    let node = lift_and_attest(&frontmatter(), BODY, &id, &kp).unwrap();

    // Lifted structure.
    assert_eq!(node.type_, "Intent");
    assert_eq!(node.id, "urn:atomic:intent:word-5");
    assert_eq!(node.human_key, "WORD-5");
    assert_eq!(node.has_acceptance_criterion.len(), 1);
    assert_eq!(node.has_task.len(), 1);
    assert_eq!(node.has_acceptance_criterion[0].id, "urn:atomic:ac:WORD-5-ac-1");
    assert_eq!(node.has_task[0].touches_file, vec!["src/App.tsx".to_string()]);
    assert!(node.why.as_deref().unwrap().contains("local-only modal"));

    // Attested.
    assert!(node.content_hash.as_deref().unwrap().starts_with("blake3:"));
    assert!(node.proof.is_some());
    assert_eq!(node.proof.as_ref().unwrap().cryptosuite, "eddsa-jcs-2022");

    // Gate passes (met AC carries verifiedBy + evidence).
    let report = validate_intent(&node);
    assert!(report.conforms, "expected conforms, got:\n{report}");

    // Signature + hash verify.
    verify(&node, &kp.public).expect("verification should succeed");

    // Render regenerates the status line from the spine.
    let text = render(&node, Target::Cli);
    assert!(text.contains("Status:    todo"), "render:\n{text}");
    assert!(text.contains("[x] On first load"));
}

#[test]
fn gate_rejects_met_criterion_without_evidence() {
    let (id, kp) = dev_identity();
    let body = "\
:::why{}
reason exists
:::

:::acceptance-criterion{#ac-1 status=met}
Checked but unproven.
:::";
    let node = lift_and_attest(&frontmatter(), body, &id, &kp).unwrap();
    let report = validate_intent(&node);
    assert!(!report.conforms);
    assert!(report
        .results
        .iter()
        .any(|v| v.shape == "AcceptanceCriterionShape"
            && v.message.contains("verifiedBy and evidence")));
}

#[test]
fn gate_requires_a_reason_to_be_present() {
    let (id, kp) = dev_identity();
    // No :::why directive.
    let body = ":::acceptance-criterion{#ac-1 status=open}\nsomething\n:::";
    let node = lift_and_attest(&frontmatter(), body, &id, &kp).unwrap();
    let report = validate_intent(&node);
    assert!(!report.conforms);
    assert!(report.results.iter().any(|v| v.path.as_deref() == Some("why")));
}

#[test]
fn gate_rejects_unknown_status() {
    let (id, kp) = dev_identity();
    let mut fm = frontmatter();
    fm.insert("status".into(), Value::String("shipped".into()));
    let node = lift_and_attest(&fm, ":::why{}\nr\n:::", &id, &kp).unwrap();
    let report = validate_intent(&node);
    assert!(!report.conforms);
    assert!(report.results.iter().any(|v| v.path.as_deref() == Some("status")));
}

#[test]
fn content_hash_is_deterministic() {
    let a = lift_intent(&frontmatter(), BODY).unwrap();
    let b = lift_intent(&frontmatter(), BODY).unwrap();
    assert_eq!(a.compute_content_hash(), b.compute_content_hash());
}

#[test]
fn tampering_a_signed_field_breaks_verification() {
    let (id, kp) = dev_identity();
    let mut node = lift_and_attest(&frontmatter(), BODY, &id, &kp).unwrap();

    // Flip status after signing — the stored contentHash no longer matches.
    node.status = "done".to_string();
    let err = verify(&node, &kp.public).unwrap_err();
    assert!(
        matches!(err, atomic_canonical::CanonicalError::HashMismatch { .. }),
        "expected HashMismatch, got {err:?}"
    );
}

#[test]
fn wrong_key_fails_verification() {
    let (id, kp) = dev_identity();
    let node = lift_and_attest(&frontmatter(), BODY, &id, &kp).unwrap();
    let other = KeyPair::generate();
    assert!(verify(&node, &other.public).is_err());
}

#[test]
fn parse_markdown_then_lift() {
    let doc = format!(
        "---\nid: WORD-9\ntitle: \"Doc parse\"\nstatus: backlog\n---\n{BODY}"
    );
    let (fm, body) = parse_markdown(&doc).unwrap();
    assert_eq!(fm.get("id").unwrap().as_str(), Some("WORD-9"));
    let node = lift_intent(&fm, &body).unwrap();
    assert_eq!(node.human_key, "WORD-9");
    assert_eq!(node.id, "urn:atomic:intent:word-9");
    assert_eq!(node.has_acceptance_criterion.len(), 1);
}
