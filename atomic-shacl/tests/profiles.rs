use atomic_core::{
    kg::{
        iri,
        ontology::{Class as C, Predicate as P},
        project_id::ProjectId,
    },
    types::Hash,
    KgEdge, KgNode,
};
use atomic_shacl::*;

fn fixture() -> (Vec<KgNode>, Vec<KgEdge>, String, String) {
    let project = ProjectId::generate();
    let session = iri::mint_session(&project, "session");
    let turn = iri::mint_turn(&project, "session", 0);
    let intent = iri::mint_intent(&project, "01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let change = iri::mint_change(&Hash::of(b"change"));
    let provenance = iri::mint_provenance(&Hash::of(b"provenance"));
    let attestation = iri::mint_attestation(&Hash::of(b"attestation"));
    let signer = format!("did:atomic:{}", project.to_base32());
    let nodes = vec![
        KgNode::new(&session, C::Session, "session", "fixture"),
        KgNode::new(&turn, C::SessionTurn, "turn", "fixture").with_type(C::ManagedTurn),
        // Only the subclass is asserted: standard SHACL subclass matching
        // must satisfy the WorkIntent range without OWL materialization.
        KgNode::new(&intent, C::FeatureIntent, "intent", "fixture"),
        KgNode::new(&change, C::Change, "change", "fixture"),
        KgNode::new(&provenance, C::ProvenanceGraph, "provenance", "fixture"),
        KgNode::new(&attestation, C::Attestation, "attestation", "fixture"),
        KgNode::new(&signer, C::Identity, "signer", "fixture"),
    ];
    let edges = vec![
        KgEdge::new(&session, &turn, P::HasTurn),
        KgEdge::new(&turn, &intent, P::HasPlan),
        KgEdge::new(&turn, &change, P::GeneratedChange),
        KgEdge::new(&turn, &provenance, P::TurnExplainedBy),
        KgEdge::new(&change, &intent, P::Implements),
        KgEdge::new(&change, &provenance, P::ChangeExplainedBy),
        KgEdge::new(&change, &attestation, P::AttestedBy),
        KgEdge::new(&attestation, &change, P::CoversChange),
        KgEdge::new(&attestation, &session, P::AttestsSession),
        KgEdge::new(&attestation, &signer, P::SignedBy),
        KgEdge::new(
            &attestation,
            "https://atomic.dev/ns/workflow#Verified",
            P::VerificationResult,
        ),
    ];
    (nodes, edges, change, project.to_urn())
}

#[test]
fn complete_workflow_conforms_to_both_profiles_and_reports_real_coverage() {
    let (nodes, edges, _, _) = fixture();
    let data = snapshot_ntriples(&nodes, &edges).unwrap();
    for profile in [Profile::Projection, Profile::Publication] {
        let report = validate_ntriples(
            &data,
            "fixture",
            &ValidationRequest {
                profile,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(report.conforms, "{}", report.report_ntriples);
        assert!(report
            .target_coverage
            .iter()
            .any(|coverage| coverage.evaluated > 0));
        assert_eq!(report.snapshot_identity, "fixture");
    }
}

#[test]
fn each_missing_shortcut_needs_its_own_change_finding() {
    let (mut nodes, mut edges, change, _) = fixture();
    edges.retain(|edge| {
        !matches!(
            edge.kind,
            P::HasPlan
                | P::TurnExplainedBy
                | P::CoversChange
                | P::Implements
                | P::ChangeExplainedBy
                | P::AttestedBy
        )
    });
    let request = ValidationRequest::default();
    let validate = |nodes: &[KgNode], edges: &[KgEdge]| {
        validate_ntriples(
            &snapshot_ntriples(nodes, edges).unwrap(),
            "fixture",
            &request,
        )
        .unwrap()
    };
    assert!(!validate(&nodes, &edges).conforms);
    let project = ProjectId::generate();
    for code in ["MissingPlanId", "MissingProvenance", "MissingAttestation"] {
        let finding = iri::mint_finding(&project, code, &change);
        nodes.push(
            KgNode::new(&finding, C::Finding, code, "fixture")
                .with_metadata(serde_json::json!({"finding_code": code})),
        );
        edges.push(KgEdge::new(&finding, &change, P::AboutResource));
    }
    assert!(validate(&nodes, &edges).conforms);
    nodes.last_mut().unwrap().metadata = Some(serde_json::json!({"finding_code": "WrongCode"}));
    assert!(!validate(&nodes, &edges).conforms);
}

#[test]
fn shortcuts_cannot_disagree_with_intermediate_paths() {
    let (nodes, mut edges, change, _) = fixture();
    let alternate = nodes
        .iter()
        .find(|node| node.has_type(C::Session))
        .unwrap()
        .id
        .clone();
    edges.push(KgEdge::new(change, alternate, P::Implements));
    let report = validate_ntriples(
        &snapshot_ntriples(&nodes, &edges).unwrap(),
        "fixture",
        &Default::default(),
    )
    .unwrap();
    assert!(!report.conforms);
    assert!(report
        .results
        .iter()
        .any(|result| result["source_constraint_component"]
            .as_str()
            .unwrap()
            .contains("SPARQL")));
}

#[test]
fn publication_rejects_unverified_and_mac_signatures() {
    let (nodes, mut edges, _, _) = fixture();
    let request = ValidationRequest {
        profile: Profile::Publication,
        ..Default::default()
    };
    for status in ["Failed", "Unverifiable"] {
        edges
            .iter_mut()
            .find(|edge| edge.kind == P::VerificationResult)
            .unwrap()
            .to_id = format!("https://atomic.dev/ns/workflow#{status}");
        assert!(
            !validate_ntriples(
                &snapshot_ntriples(&nodes, &edges).unwrap(),
                "fixture",
                &request
            )
            .unwrap()
            .conforms
        );
    }
    edges
        .iter_mut()
        .find(|edge| edge.kind == P::VerificationResult)
        .unwrap()
        .to_id = "https://atomic.dev/ns/workflow#Verified".into();
    edges
        .iter_mut()
        .find(|edge| edge.kind == P::SignedBy)
        .unwrap()
        .to_id = "urn:atomic:identity:session-mac".into();
    assert!(
        !validate_ntriples(
            &snapshot_ntriples(&nodes, &edges).unwrap(),
            "fixture",
            &request
        )
        .unwrap()
        .conforms
    );
}

fn custom(turtle: &str) -> ValidationRequest {
    ValidationRequest { custom: vec![ShapeBundle { iri: "urn:enterprise:shapes".into(), turtle: format!("@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix atomic: <https://atomic.dev/ns/workflow#> . {turtle}") }], ..Default::default() }
}

#[test]
fn errors_are_not_vacuous_conformance() {
    for turtle in [
        "<urn:bad> a sh:NodeShape ; sh:targetClass atomic:Change ; sh:property [ sh:path atomic:implements ; sh:minCount \"one\" ] .",
        "<urn:bad> a sh:NodeShape ; sh:js [ sh:jsFunctionName \"validate\" ] .",
        "<urn:bad> a sh:NodeShape ; sh:sparql [ sh:select \"SELECT $this WHERE {{\" ] .",
        "<urn:enterprise:shapes> <http://www.w3.org/2002/07/owl#imports> <https://example.invalid/remote.ttl> .",
    ] { assert!(validate_ntriples("", "empty", &custom(turtle)).is_err(), "accepted {turtle}"); }
    let report = validate_ntriples("", "empty", &Default::default()).unwrap();
    assert!(report.conforms);
    assert!(report
        .target_coverage
        .iter()
        .all(|coverage| coverage.evaluated == 0));
    assert!(report
        .diagnostics
        .iter()
        .any(|message| message.contains("vacuous")));
}

#[test]
fn custom_shapes_preserve_nested_paths_and_standard_report_fields() {
    let (nodes, edges, change, _) = fixture();
    let request = custom("<urn:custom> a sh:NodeShape ; sh:targetClass atomic:Change ; sh:property [ sh:path ( [ sh:inversePath atomic:generatedChange ] atomic:hasTodo ) ; sh:minCount 1 ; sh:message \"Need a todo\"@en ] .");
    let mut request = request;
    request.shape = Some("urn:custom".into());
    request.node = Some(change);
    let report = validate_ntriples(
        &snapshot_ntriples(&nodes, &edges).unwrap(),
        "fixture",
        &request,
    )
    .unwrap();
    assert!(!report.conforms);
    assert!(report.results[0]["path_structure"]["sequence"].is_array());
    assert!(report.results[0]["result_messages"][0]["rdf_literal"]
        .as_str()
        .unwrap()
        .ends_with("@en"));
    for field in [
        "focusNode",
        "resultPath",
        "sourceShape",
        "sourceConstraintComponent",
        "resultSeverity",
        "resultMessage",
        "inversePath",
    ] {
        assert!(report.report_ntriples.contains(field), "missing {field}");
    }
}
