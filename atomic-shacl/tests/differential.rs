//! The adversarial differential corpus — the spike's actual deliverable.
//!
//! For each fixture we run the trusted hand-coded gate (the oracle) AND the real
//! oxirs SHACL path over the SAME node, and compare `conforms` + the set of
//! `(focus_node, path)` findings. The corpus targets the verified divergence
//! traps (trim asymmetry, present-empty vs omitted, nested AC via sh:node, the
//! two SHACL-SPARQL conditionals, the superseded-no-forward-edge anti-fixture).
//!
//! This file compiles only with `--features oxirs-engine`.
#![cfg(feature = "oxirs-engine")]

use std::collections::BTreeSet;

use atomic_canonical::gate::{validate_intent, validate_memory, ValidationReport};
use atomic_canonical::memory::MemoryNode;
use atomic_canonical::node::{AcceptanceCriterion, CanonicalNode, Proof, ScopeItem, CONTEXT_URL};
use atomic_shacl::engine::{validate_intent_shacl, validate_memory_shacl, FindingKey};

// ---------- fixture builders (dummy proof: the gate only checks presence) ----------

fn dummy_proof() -> Proof {
    Proof {
        type_: "DataIntegrityProof".into(),
        cryptosuite: "eddsa-jcs-2022".into(),
        verification_method: "did:atomic:x#k".into(),
        proof_purpose: "assertionMethod".into(),
        proof_value: "zStub".into(),
    }
}

fn base_intent() -> CanonicalNode {
    CanonicalNode {
        context: CONTEXT_URL.into(),
        type_: "Intent".into(),
        id: "urn:atomic:intent:c".into(),
        human_key: "C-1".into(),
        title: "t".into(),
        status: "todo".into(),
        priority: None,
        view: None,
        motivated_by: None,
        informed_by: vec![],
        has_acceptance_criterion: vec![],
        has_task: vec![],
        has_scope_in: vec![],
        has_scope_out: vec![],
        has_constraint: vec![],
        depends_on: vec![],
        why: Some("a reason".into()),
        content_hash: None,
        attributed_to: Some("did:atomic:lee".into()),
        created_at: "2026-06-25T00:00:00Z".into(),
        proof: Some(dummy_proof()),
    }
}

fn ac(id: &str, status: &str, vby: Option<&str>, ev: Option<&str>) -> AcceptanceCriterion {
    AcceptanceCriterion {
        type_: "AcceptanceCriterion".into(),
        id: id.into(),
        text: "t".into(),
        ac_status: status.into(),
        verified_by: vby.map(Into::into),
        evidence: ev.map(Into::into),
    }
}

fn scope(id: &str) -> ScopeItem {
    ScopeItem {
        type_: "ScopeItem".into(),
        id: id.into(),
        text: "s".into(),
    }
}

fn base_memory() -> MemoryNode {
    MemoryNode {
        context: CONTEXT_URL.into(),
        type_: "Memory".into(),
        id: "urn:atomic:memory:c".into(),
        memory_kind: "constraint".into(),
        text: "a durable constraint".into(),
        about: vec![],
        status: "active".into(),
        supersedes: None,
        previous_revision: None,
        content_hash: None,
        attributed_to: Some("did:atomic:lee".into()),
        created_at: "2026-05-02T09:14:00Z".into(),
        proof: Some(dummy_proof()),
    }
}

// ---------- normalization of the gate's report to the same key space ----------

fn gate_keys(report: &ValidationReport) -> BTreeSet<FindingKey> {
    report
        .results
        .iter()
        .map(|v| (v.focus_node.clone(), v.path.clone()))
        .collect()
}

fn shacl_conforms_intent(node: &CanonicalNode) -> bool {
    validate_intent_shacl(node).expect("shacl intent").conforms
}
fn shacl_conforms_memory(node: &MemoryNode) -> bool {
    validate_memory_shacl(node).expect("shacl memory").conforms
}

// ============================================================================
// WHAT WORKS: Core presence (sh:minCount) + closed-set (sh:in) rules.
// oxirs 0.3.1 reproduces the gate's `conforms` verdict on every one of these,
// INCLUDING the trim asymmetry and the load-bearing directionality ABSENCE.
// ============================================================================

#[test]
fn core_presence_and_closed_set_rules_match_gate_on_conformance() {
    // (label, mutate, )   — each asserts gate.conforms == shacl.conforms.
    let mut cases: Vec<(&str, CanonicalNode)> = Vec::new();

    cases.push(("baseline conforms", base_intent()));

    let mut n = base_intent();
    n.status = "shipped".into();
    cases.push(("unknown status rejected", n));

    let mut n = base_intent();
    n.status = String::new();
    cases.push(("empty status rejected", n));

    // Trim asymmetry: whitespace-only why is REJECTED (gate .trim()).
    let mut n = base_intent();
    n.why = Some("   ".into());
    cases.push(("whitespace-only why rejected", n));

    let mut n = base_intent();
    n.why = None;
    cases.push(("missing why rejected", n));

    // Trim asymmetry: whitespace-only attributedTo is ACCEPTED (gate no trim).
    let mut n = base_intent();
    n.attributed_to = Some("   ".into());
    cases.push(("whitespace-only attributedTo accepted", n));

    // present-empty attributedTo is rejected (== omitted).
    let mut n = base_intent();
    n.attributed_to = Some(String::new());
    cases.push(("present-empty attributedTo rejected", n));

    let mut n = base_intent();
    n.attributed_to = None;
    n.proof = None;
    cases.push(("unattested rejected", n));

    for (label, node) in &cases {
        let gate = validate_intent(node).conforms;
        let shacl = shacl_conforms_intent(node);
        assert_eq!(
            gate, shacl,
            "INTENT conformance divergence on '{label}': gate={gate} shacl={shacl}"
        );
        // Where non-conforming, the focus node must at least match.
        if !gate {
            let g = gate_keys(&validate_intent(node));
            let s = &validate_intent_shacl(node).unwrap().findings;
            let g_focus: BTreeSet<_> = g.iter().map(|(f, _)| f.clone()).collect();
            let s_focus: BTreeSet<_> = s.iter().map(|(f, _)| f.clone()).collect();
            assert_eq!(g_focus, s_focus, "INTENT focus-node divergence on '{label}'");
        }
    }

    // Memory Core rules + the directionality ABSENCE anti-fixture.
    let mut m_cases: Vec<(&str, MemoryNode)> = Vec::new();
    m_cases.push(("baseline conforms", base_memory()));
    let mut m = base_memory();
    m.memory_kind = "architecture".into();
    m_cases.push(("unknown memoryKind rejected", m));
    let mut m = base_memory();
    m.status = "archived".into();
    m_cases.push(("unknown status rejected", m));
    let mut m = base_memory();
    m.text = "   ".into();
    m_cases.push(("whitespace-only text rejected", m));
    let mut m = base_memory();
    m.attributed_to = None;
    m.proof = None;
    m.text = "   ".into();
    m_cases.push(("bare memory rejected", m));
    // ANTI-FIXTURE (directionality): a retired memory with no forward edge conforms.
    let mut m = base_memory();
    m.status = "superseded".into();
    m.supersedes = None;
    m.previous_revision = None;
    m_cases.push(("superseded no-forward-edge conforms", m));

    for (label, node) in &m_cases {
        let gate = validate_memory(node).conforms;
        let shacl = shacl_conforms_memory(node);
        assert_eq!(
            gate, shacl,
            "MEMORY conformance divergence on '{label}': gate={gate} shacl={shacl}"
        );
    }
}

// ============================================================================
// CONFIRMED BLOCKERS: rules oxirs 0.3.1 SILENTLY DOES NOT ENFORCE.
// Each asserts the gate REJECTS but SHACL wrongly CONFORMS — a signing-path
// false-negative. These tests PASS today (they lock in the defect). If a future
// oxirs release fixes the construct, the corresponding assert flips and the test
// FAILS, signalling "re-evaluate: this rule can now be promoted."
// ============================================================================

#[test]
fn blocker_sh_node_nested_acceptance_criterion_not_enforced() {
    // AC with an unknown acStatus: gate rejects, oxirs ignores sh:node → conforms.
    let mut n = base_intent();
    n.has_acceptance_criterion = vec![ac("urn:atomic:ac:1", "not-a-status", None, None)];
    assert!(!validate_intent(&n).conforms, "gate must reject unknown acStatus");
    assert!(
        shacl_conforms_intent(&n),
        "BLOCKER FIXED? oxirs now enforces sh:node nested AC validation — re-evaluate"
    );
}

#[test]
fn blocker_sparql_intent5_scope_in_implies_scope_out_not_enforced() {
    let mut n = base_intent();
    n.has_scope_in = vec![scope("urn:atomic:scope:in1")];
    // no scope-out
    assert!(
        !validate_intent(&n).conforms,
        "gate must reject scope-in without scope-out"
    );
    assert!(
        shacl_conforms_intent(&n),
        "BLOCKER FIXED? oxirs now enforces sh:sparql INTENT-5 — re-evaluate"
    );
}

#[test]
fn blocker_sparql_intent7_met_ac_requires_evidence_not_enforced() {
    let mut n = base_intent();
    n.has_acceptance_criterion = vec![ac("urn:atomic:ac:1", "met", None, None)];
    assert!(
        !validate_intent(&n).conforms,
        "gate must reject a met AC without evidence"
    );
    assert!(
        shacl_conforms_intent(&n),
        "BLOCKER FIXED? oxirs now enforces sh:sparql INTENT-7 — re-evaluate"
    );
}

#[test]
fn fidelity_gap_oxirs_leaves_result_path_unpopulated() {
    // The gate names the failing property; oxirs 0.3.1 reports result_path=None,
    // so distinct violations on one node collapse and cannot be mapped 1:1.
    let mut n = base_intent();
    n.attributed_to = None;
    n.proof = None;
    let shacl = validate_intent_shacl(&n).expect("shacl");
    assert!(!shacl.conforms);
    assert!(
        shacl.findings.iter().all(|(_, path)| path.is_none()),
        "BLOCKER FIXED? oxirs now populates result_path — path mapping can tighten"
    );
    // Two distinct gate violations (attributedTo, proof) collapse to one SHACL key.
    let gate = gate_keys(&validate_intent(&n));
    assert_eq!(gate.len(), 2, "gate distinguishes attributedTo vs proof");
    assert_eq!(
        shacl.findings.len(),
        1,
        "oxirs collapses both missing-property violations into one (focus, None) key"
    );
}
