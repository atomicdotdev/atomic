//! M0 end-to-end proof: author → lift → JCS/BLAKE3 hash → Ed25519 proof →
//! gate → render, entirely in-crate (no vault wiring).

use atomic_canonical::lift::{lift_intent, parse_markdown};
use atomic_canonical::memory::lift_memory;
use atomic_canonical::{
    lift_and_attest, lift_and_attest_memory, render, render_memory, validate_intent,
    validate_memory, verify, verify_memory, Target,
};
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
    assert_eq!(
        node.has_acceptance_criterion[0].id,
        "urn:atomic:ac:WORD-5-ac-1"
    );
    assert_eq!(
        node.has_task[0].touches_file,
        vec!["src/App.tsx".to_string()]
    );
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
    assert!(report
        .results
        .iter()
        .any(|v| v.path.as_deref() == Some("why")));
}

#[test]
fn gate_rejects_unknown_status() {
    let (id, kp) = dev_identity();
    let mut fm = frontmatter();
    fm.insert("status".into(), Value::String("shipped".into()));
    let node = lift_and_attest(&fm, ":::why{}\nr\n:::", &id, &kp).unwrap();
    let report = validate_intent(&node);
    assert!(!report.conforms);
    assert!(report
        .results
        .iter()
        .any(|v| v.path.as_deref() == Some("status")));
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
fn empty_new_collections_do_not_appear_in_canonical_form() {
    // Regression guard for the hash-safety of the added Vec fields: the WORD-5
    // fixture declares no scope/constraint/ref, so those keys must be OMITTED
    // from the canonical value (skip_serializing_if = Vec::is_empty). If any
    // leaked as an empty array it would silently change the contentHash and
    // break the existing signature tests.
    let node = lift_intent(&frontmatter(), BODY).unwrap();
    let value = node.to_value();
    let obj = value.as_object().unwrap();
    assert!(!obj.contains_key("hasScopeIn"));
    assert!(!obj.contains_key("hasScopeOut"));
    assert!(!obj.contains_key("hasConstraint"));
    assert!(!obj.contains_key("dependsOn"));
}

#[test]
fn lifts_scope_constraints_and_ref_end_to_end() {
    use atomic_canonical::{Constraint, Ref, ScopeItem};

    let (id, kp) = dev_identity();
    let body = "\
:::why{}
reason exists
:::

:::scope-in
`src/App.tsx` state and markup.
:::

:::scope-out
Server-side accounts.
:::

:::constraint
Keep it local.
:::

:::ref{to=urn:atomic:intent:019efe80-1234 edge=blockedBy}
:::";
    let node = lift_and_attest(&frontmatter(), body, &id, &kp).unwrap();

    let _scope: &Vec<ScopeItem> = &node.has_scope_in;
    let _c: &Vec<Constraint> = &node.has_constraint;
    let _r: &Vec<Ref> = &node.depends_on;
    assert_eq!(node.has_scope_in.len(), 1);
    assert_eq!(node.has_scope_out.len(), 1);
    assert_eq!(node.has_constraint.len(), 1);
    assert_eq!(node.depends_on.len(), 1);
    assert_eq!(node.depends_on[0].edge, "blockedBy");

    // Gate conforms (scope-in present, scope-out present) and verify roundtrips.
    assert!(validate_intent(&node).conforms);
    verify(&node, &kp.public).expect("verification should succeed");

    // Render surfaces the new sections.
    let text = render(&node, Target::Cli);
    assert!(text.contains("Out of Scope"), "render:\n{text}");
    assert!(text.contains("blockedBy -> urn:atomic:intent:019efe80-1234"));
}

fn memory_frontmatter() -> Map<String, Value> {
    json!({
        "uid": "01J8ZC4R8T",
        "memoryKind": "constraint",
        "status": "active",
        "about": ["urn:atomic:module:storage", "urn:atomic:module:replication"]
    })
    .as_object()
    .unwrap()
    .clone()
}

const MEMORY_BODY: &str = "\
:::memory{}
Multi-region writes must converge with no single ordering authority. Anything
that reintroduces a writer-of-record is off the table.
:::";

#[test]
fn memory_lift_gate_attest_verify_render_end_to_end() {
    let (id, kp) = dev_identity();

    // Lift structure.
    let lifted = lift_memory(&memory_frontmatter(), MEMORY_BODY).unwrap();
    assert_eq!(lifted.type_, "Memory");
    assert_eq!(lifted.id, "urn:atomic:memory:01J8ZC4R8T");
    assert_eq!(lifted.memory_kind, "constraint");
    assert_eq!(lifted.status, "active");
    assert_eq!(lifted.about.len(), 2);
    assert!(lifted.text.contains("no single ordering authority"));
    // Single authoring site: the :::memory container body is the text, without
    // its wrapper directive lines.
    assert!(!lifted.text.contains(":::"));

    // Attest through the shared path.
    let node = lift_and_attest_memory(&memory_frontmatter(), MEMORY_BODY, &id, &kp).unwrap();
    assert!(node.content_hash.as_deref().unwrap().starts_with("blake3:"));
    assert!(node.proof.is_some());
    assert_eq!(node.proof.as_ref().unwrap().cryptosuite, "eddsa-jcs-2022");

    // Gate conforms.
    assert!(
        validate_memory(&node).conforms,
        "gate:\n{}",
        validate_memory(&node)
    );

    // Verify.
    verify_memory(&node, &kp.public).expect("memory verification should succeed");

    // Render regenerates the status from the spine and shows about + text.
    let text = render_memory(&node, Target::Cli);
    assert!(text.contains("Status:    active"), "render:\n{text}");
    assert!(text.contains("urn:atomic:module:storage"));
    assert!(text.contains("no single ordering authority"));
}

#[test]
fn memory_tampering_and_wrong_key_break_verification() {
    let (id, kp) = dev_identity();
    let mut node = lift_and_attest_memory(&memory_frontmatter(), MEMORY_BODY, &id, &kp).unwrap();

    // Tamper the text after signing → the stored contentHash no longer matches.
    node.text = "a different, forged memory".to_string();
    let err = verify_memory(&node, &kp.public).unwrap_err();
    assert!(
        matches!(err, atomic_canonical::CanonicalError::HashMismatch { .. }),
        "expected HashMismatch, got {err:?}"
    );

    // A pristine memory verifies with the right key but not with the wrong one.
    let pristine = lift_and_attest_memory(&memory_frontmatter(), MEMORY_BODY, &id, &kp).unwrap();
    let other = KeyPair::generate();
    assert!(verify_memory(&pristine, &other.public).is_err());
    verify_memory(&pristine, &kp.public).expect("right key verifies");
}

#[test]
fn memory_falls_back_to_whole_body_without_container() {
    let (id, kp) = dev_identity();
    let node = lift_and_attest_memory(
        &memory_frontmatter(),
        "   a plain constraint with no directive   ",
        &id,
        &kp,
    )
    .unwrap();
    assert_eq!(node.text, "a plain constraint with no directive");
    assert!(validate_memory(&node).conforms);
}

#[test]
fn parse_markdown_then_lift() {
    let doc = format!("---\nid: WORD-9\ntitle: \"Doc parse\"\nstatus: backlog\n---\n{BODY}");
    let (fm, body) = parse_markdown(&doc).unwrap();
    assert_eq!(fm.get("id").unwrap().as_str(), Some("WORD-9"));
    let node = lift_intent(&fm, &body).unwrap();
    assert_eq!(node.human_key, "WORD-9");
    assert_eq!(node.id, "urn:atomic:intent:word-9");
    assert_eq!(node.has_acceptance_criterion.len(), 1);
}

// ── Regression tests for the adversarial-review fixes ────────────────────────

#[test]
fn injected_unknown_field_is_rejected_on_load_intent() {
    // BLOCKER fix: a signed node with an injected key must NOT silently load
    // (deny_unknown_fields), so an attacker can't smuggle a field past verify by
    // relying on serde to strip it before the hash is recomputed.
    let (id, kp) = dev_identity();
    let node = lift_and_attest(&frontmatter(), BODY, &id, &kp).unwrap();
    let mut v = serde_json::to_value(&node).unwrap();
    v.as_object_mut().unwrap().insert(
        "actualStatus".into(),
        Value::String("done-but-forged".into()),
    );
    let reload: Result<atomic_canonical::CanonicalNode, _> = serde_json::from_value(v);
    assert!(reload.is_err(), "an unknown field must be rejected on load");
}

#[test]
fn injected_unknown_field_is_rejected_on_load_memory() {
    let (id, kp) = dev_identity();
    let node = lift_and_attest_memory(&memory_frontmatter(), "a constraint", &id, &kp).unwrap();
    let mut v = serde_json::to_value(&node).unwrap();
    v.as_object_mut()
        .unwrap()
        .insert("forged".into(), Value::String("x".into()));
    let reload: Result<atomic_canonical::MemoryNode, _> = serde_json::from_value(v);
    assert!(reload.is_err(), "an unknown field must be rejected on load");
}

#[test]
fn tampered_proof_metadata_fails_verification() {
    // MINOR fix: cryptosuite/purpose/type are enforced in verify_value even
    // though signing_view strips the proof object from the signed bytes.
    let (id, kp) = dev_identity();
    let node = lift_and_attest_memory(&memory_frontmatter(), "a constraint", &id, &kp).unwrap();
    let mut v = serde_json::to_value(&node).unwrap();
    v.as_object_mut()
        .unwrap()
        .get_mut("proof")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("cryptosuite".into(), Value::String("bogus-suite".into()));
    assert!(
        atomic_canonical::proof::verify_value(&v, &kp.public).is_err(),
        "a mutated cryptosuite must fail verification"
    );
}

#[test]
fn ref_edge_is_restricted_to_dependency_subset() {
    // MINOR fix: a :::ref sits on the dependency chain, so a non-dependency edge
    // (e.g. verifiedBy) must be rejected even though it is a valid edge elsewhere.
    let good = format!("{BODY}\n\n:::ref{{to=urn:atomic:intent:other edge=blockedBy}}\n:::");
    let node = lift_intent(&frontmatter(), &good).unwrap();
    assert_eq!(node.depends_on.len(), 1);
    assert_eq!(node.depends_on[0].edge, "blockedBy");

    let bad = format!("{BODY}\n\n:::ref{{to=urn:atomic:intent:other edge=verifiedBy}}\n:::");
    assert!(
        lift_intent(&frontmatter(), &bad).is_err(),
        "verifiedBy is not a dependency edge"
    );
}

#[test]
fn memory_body_with_directive_like_prose_lifts_verbatim() {
    // MAJOR fix: open memory prose may begin a line with :: / ::: without being
    // rejected by the closed-vocabulary directive parser.
    let body =
        "Prefer explicit types.\n::<T> turbofish is required here.\nWe discussed :::why once.";
    let node = lift_memory(&memory_frontmatter(), body).unwrap();
    assert!(node.text.contains("turbofish"), "text: {}", node.text);
    assert!(node.text.contains(":::why"));
}
