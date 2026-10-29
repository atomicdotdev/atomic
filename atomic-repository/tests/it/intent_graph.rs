use atomic_core::{
    kg::{
        iri,
        ontology::{Class as C, Predicate as P, VERSION_IRI},
        project_id::ProjectId,
    },
    types::Hash,
    KgEdge, KgNode,
};
use atomic_repository::{intent_graph::IntentGraph, repository::WorkflowSnapshot};

fn fixture() -> (WorkflowSnapshot, String, String, String, String) {
    let project = ProjectId::from_base32(&"A".repeat(52)).unwrap();
    let root = iri::mint_intent(&project, "01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let other = iri::mint_intent(&project, "01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap();
    let change = iri::mint_change(&Hash::of(b"implementation"));
    let dependency = iri::mint_change(&Hash::of(b"dependency"));
    let unrelated = iri::mint_change(&Hash::of(b"unrelated"));
    let mut nodes = vec![];
    for (id, class) in [
        (root.as_str(), C::FeatureIntent),
        (other.as_str(), C::WorkIntent),
        (change.as_str(), C::Change),
        (dependency.as_str(), C::Change),
        (unrelated.as_str(), C::Change),
        ("urn:turn:one", C::SessionTurn),
        ("urn:turn:two", C::SessionTurn),
        ("urn:session:shared", C::Session),
        ("urn:file:shared", C::File),
        ("urn:author:shared", C::Identity),
        ("urn:attestation:shared", C::Attestation),
        ("urn:provenance:one", C::ProvenanceGraph),
        ("urn:view:child", C::View),
        ("urn:view:parent", C::View),
        ("urn:task:one", C::Task),
        ("urn:ac:one", C::AcceptanceCriterion),
        ("urn:finding:gap", C::Finding),
    ] {
        nodes.push(KgNode::new(id, class, id, "fixture"));
    }
    let mut edges = vec![];
    for (from, to, p) in [
        (change.as_str(), root.as_str(), P::Implements),
        (
            change.as_str(),
            dependency.as_str(),
            P::DirectlyDependsOnChange,
        ),
        (
            dependency.as_str(),
            change.as_str(),
            P::DirectlyDependsOnChange,
        ), // cycle safety
        ("urn:turn:one", root.as_str(), P::HasPlan),
        ("urn:turn:one", change.as_str(), P::GeneratedChange),
        ("urn:turn:two", other.as_str(), P::HasPlan),
        ("urn:turn:two", unrelated.as_str(), P::GeneratedChange),
        ("urn:session:shared", "urn:turn:one", P::HasTurn),
        ("urn:session:shared", "urn:turn:two", P::HasTurn),
        (change.as_str(), "urn:file:shared", P::ModifiesFile),
        (unrelated.as_str(), "urn:file:shared", P::ModifiesFile),
        (change.as_str(), "urn:author:shared", P::AuthoredBy),
        (unrelated.as_str(), "urn:author:shared", P::AuthoredBy),
        ("urn:attestation:shared", change.as_str(), P::CoversChange),
        (
            "urn:attestation:shared",
            unrelated.as_str(),
            P::CoversChange,
        ),
        (
            "urn:attestation:shared",
            "https://atomic.dev/ns/workflow#Verified",
            P::VerificationResult,
        ),
        ("urn:turn:one", "urn:provenance:one", P::TurnExplainedBy),
        (change.as_str(), "urn:view:child", P::DirectlyOnView),
        ("urn:view:child", "urn:view:parent", P::ChildViewOf),
        (root.as_str(), "urn:task:one", P::HasTask),
        (root.as_str(), "urn:ac:one", P::HasAcceptanceCriterion),
        ("urn:task:one", "urn:ac:one", P::Satisfies),
        ("urn:finding:gap", dependency.as_str(), P::AboutResource),
    ] {
        edges.push(KgEdge::new(from, to, p));
    }
    (
        WorkflowSnapshot {
            generation: atomic_core::kg::KG_GENERATION,
            ontology_version: VERSION_IRI,
            identity: "consistent-snapshot".into(),
            nodes,
            edges,
        },
        root,
        change,
        dependency,
        unrelated,
    )
}

#[test]
fn selects_semantic_lineage_without_acquiring_shared_resource_neighbors() {
    let (snapshot, root, change, dependency, unrelated) = fixture();
    let graph = IntentGraph::from_snapshot(&snapshot, &root, 100).unwrap();
    assert_eq!(graph.implementing_changes, vec![change]);
    assert_eq!(graph.dependency_changes, vec![dependency]);
    for id in [
        "urn:session:shared",
        "urn:provenance:one",
        "urn:attestation:shared",
        "urn:finding:gap",
        "urn:ac:one",
        "urn:view:parent",
        "https://atomic.dev/ns/workflow#Verified",
    ] {
        assert!(graph.nodes.iter().any(|n| n.id == id), "missing {id}");
    }
    assert!(!graph
        .nodes
        .iter()
        .any(|n| n.id == unrelated || n.id == "urn:turn:two"));
    assert!(!graph.truncated);
    assert!(graph.missing_resources.is_empty());
    assert!(graph.edges.iter().any(|e| e.kind == P::Satisfies));
    assert_eq!(graph.snapshot_identity, "consistent-snapshot");
    assert!(graph
        .diagnostics
        .iter()
        .any(|d| d.contains("object unknown")));
}

#[test]
fn selection_and_limits_are_deterministic_and_explicit() {
    let (mut snapshot, root, _, _, _) = fixture();
    let graph = IntentGraph::from_snapshot(&snapshot, &root, 8).unwrap();
    assert!(graph.truncated);
    assert_eq!(graph.nodes.len(), 8);
    assert!(graph.nodes.iter().any(|n| n.id == root));
    snapshot.nodes.reverse();
    snapshot.edges.reverse();
    let reversed = IntentGraph::from_snapshot(&snapshot, &root, 8).unwrap();
    assert_eq!(
        serde_json::to_value(graph).unwrap(),
        serde_json::to_value(reversed).unwrap()
    );
    for limit in [0, 10001] {
        assert!(IntentGraph::from_snapshot(&snapshot, &root, limit).is_err());
    }
    assert!(IntentGraph::from_snapshot(&snapshot, "intent:abbreviated", 100).is_err());
    snapshot.generation = 0;
    assert!(IntentGraph::from_snapshot(&snapshot, &root, 100).is_err());
}

#[test]
fn missing_resources_and_empty_implementation_are_not_silent_success_claims() {
    let (mut snapshot, root, _, _, _) = fixture();
    snapshot.edges.retain(|e| e.from_id == root);
    snapshot
        .edges
        .push(KgEdge::new(&root, "urn:missing:task", P::HasTask));
    let graph = IntentGraph::from_snapshot(&snapshot, &root, 100).unwrap();
    assert!(graph.implementing_changes.is_empty());
    assert_eq!(graph.missing_resources, vec!["urn:missing:task"]);
    assert!(graph.diagnostics.iter().any(|d| d.contains("not proof")));
    snapshot.nodes.retain(|n| n.id != root);
    assert!(IntentGraph::from_snapshot(&snapshot, &root, 100).is_err());
}

#[test]
fn authoritative_turn_path_seeds_changes_without_requiring_shortcuts() {
    let (mut snapshot, root, change, dependency, _) = fixture();
    snapshot.edges.retain(|e| e.kind != P::Implements);
    let graph = IntentGraph::from_snapshot(&snapshot, &root, 100).unwrap();
    assert_eq!(graph.implementing_changes, vec![change]);
    assert_eq!(graph.dependency_changes, vec![dependency]);
}

fn recording_fixture() -> (WorkflowSnapshot, String, String, String, String, String) {
    let (mut snapshot, root, change, dependency, unrelated) = fixture();
    snapshot
        .edges
        .retain(|e| !matches!(e.kind, P::Implements | P::HasPlan | P::GovernsTurn));
    let (project, uid) = iri::parse_intent(&root).unwrap();
    let path = format!("intents/{uid}/intent.md");
    snapshot
        .nodes
        .iter_mut()
        .find(|n| n.id == root)
        .unwrap()
        .metadata = Some(serde_json::json!({"vault_path": path}));
    let artifact = iri::mint_file(&project, &format!(".vault/{path}"));
    let other_artifact = iri::mint_file(
        &project,
        ".vault/intents/01ARZ3NDEKTSV4RRFFQ69G5FAW/intent.md",
    );
    for file in [&artifact, &other_artifact] {
        snapshot.nodes.push(KgNode::new(file, C::File, file, "vcs"));
    }
    snapshot
        .edges
        .push(KgEdge::new(&change, &artifact, P::ModifiesFile));
    snapshot
        .edges
        .push(KgEdge::new(&unrelated, &other_artifact, P::ModifiesFile));
    (snapshot, root, change, dependency, unrelated, artifact)
}

#[test]
fn exposes_recorded_intent_evidence_without_inventing_implementation() {
    let (snapshot, root, change, dependency, unrelated, artifact) = recording_fixture();
    let graph = IntentGraph::from_snapshot(&snapshot, &root, 100).unwrap();
    assert_eq!(graph.recording_changes, vec![change.clone()]);
    assert_eq!(graph.intent_artifact.as_ref(), Some(&artifact));
    assert_eq!(graph.dependency_changes, vec![dependency]);
    assert!(graph.implementing_changes.is_empty());
    assert!(graph.captured_changes.is_empty());
    assert!(graph.declared_changes.is_empty());
    for id in [
        &change,
        &artifact,
        "urn:turn:one",
        "urn:provenance:one",
        "urn:file:shared",
    ] {
        assert!(graph.nodes.iter().any(|n| n.id == id), "missing {id}");
    }
    assert!(!graph
        .nodes
        .iter()
        .any(|n| n.id == unrelated || n.id == "urn:turn:two"));
    assert!(
        graph.edges.iter().all(|edge| snapshot.edges.contains(edge)),
        "the read must not synthesize semantic edges"
    );
    assert!(graph
        .diagnostics
        .iter()
        .any(|d| d.contains("recording evidence")));
    assert!(!graph
        .diagnostics
        .iter()
        .any(|d| d.starts_with("No implementing changes")));
}

#[test]
fn recording_evidence_never_fans_out_from_shared_files_or_other_projects() {
    let (mut snapshot, root, change, _, _, _) = recording_fixture();
    snapshot.edges.retain(|edge| {
        !(edge.from_id == change && edge.kind == P::ModifiesFile && edge.to_id.contains(".vault/"))
    });
    let (project, uid) = iri::parse_intent(&root).unwrap();
    let foreign = ProjectId::from_base32(&format!("B{}", "A".repeat(51))).unwrap();
    assert_ne!(project, foreign);
    let foreign_artifact = iri::mint_file(&foreign, &format!(".vault/intents/{uid}/intent.md"));
    snapshot.nodes.push(KgNode::new(
        &foreign_artifact,
        C::File,
        "same path, other project",
        "vcs",
    ));
    snapshot
        .edges
        .push(KgEdge::new(&change, &foreign_artifact, P::ModifiesFile));
    let graph = IntentGraph::from_snapshot(&snapshot, &root, 100).unwrap();
    assert!(graph.recording_changes.is_empty());
    assert!(!graph
        .nodes
        .iter()
        .any(|node| node.id == change || node.id == foreign_artifact));
}

#[test]
fn recording_evidence_limits_are_deterministic_and_primary_changes_are_not_dependency_only() {
    let (mut snapshot, root, change, dependency, _, artifact) = recording_fixture();
    snapshot
        .edges
        .push(KgEdge::new(&dependency, &artifact, P::ModifiesFile));
    let graph = IntentGraph::from_snapshot(&snapshot, &root, 100).unwrap();
    assert_eq!(graph.recording_changes.len(), 2);
    assert!(graph.recording_changes.contains(&change));
    assert!(graph.recording_changes.contains(&dependency));
    assert!(graph.dependency_changes.is_empty());
    for limit in [1, 2, 5, 10] {
        let capped = IntentGraph::from_snapshot(&snapshot, &root, limit).unwrap();
        assert!(capped.truncated);
        assert_eq!(capped.nodes.len(), limit);
        let mut reversed = snapshot.clone();
        reversed.nodes.reverse();
        reversed.edges.reverse();
        assert_eq!(
            serde_json::to_value(capped).unwrap(),
            serde_json::to_value(IntentGraph::from_snapshot(&reversed, &root, limit).unwrap())
                .unwrap()
        );
    }
}
