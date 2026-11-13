use super::*;
use atomic_core::kg::ontology::{Class, Predicate};

#[test]
fn null_plan_fields_resolve_receipts_without_fabricating_governance() {
    let f = Fixture::new();
    let signed = f.signed();
    let pin = signed
        .subject
        .changes
        .iter()
        .find(|pin| pin.hash == f.change.to_base32())
        .unwrap();
    assert!(pin.governing_intents.is_empty());
    assert_eq!(pin.observations.len(), 1);
    assert_eq!(pin.observations[0].resolution, "resolved");
    assert_eq!(pin.observations[0].turns, vec![0]);
    assert_eq!(
        pin.observations[0].intent.as_deref(),
        Some(format!("urn:atomic:intent:{}", f.work).as_str())
    );
    assert!(f.conforms(&signed, &f.config()));
    let graph = f
        .repo
        .publication_review_snapshot(&signed, &f.config(), &|did| f.keys(did))
        .unwrap();
    assert!(graph
        .edges
        .iter()
        .any(|edge| edge.kind == Predicate::ObservedIntent));
    assert!(!graph.edges.iter().any(|edge| matches!(
        edge.kind,
        Predicate::HasPlan | Predicate::Implements | Predicate::AttestsSession
    )));
}

#[test]
fn conflicting_legacy_plan_fields_are_not_consulted() {
    let f = Fixture::new();
    let graph = ProvenanceGraphBuilder::new(SESSION, "fixture")
        .plan_id("not-an-intent-and-not-a-review-authority")
        .add_node(receipt_node(&f.work))
        .changes_explained(vec![f.change])
        .build();
    f.repo.save_provenance_graph(&graph).unwrap();
    let signed = f.signed();
    assert!(f.conforms(&signed, &f.config()));
    assert!(signed
        .subject
        .changes
        .iter()
        .all(|pin| pin.governing_intents.is_empty()));
}

#[test]
fn unsigned_preview_reports_blockers_and_cannot_be_recorded() {
    let f = Fixture::new();
    let unsigned = f
        .repo
        .prepare_publication_review(&f.review, &[f.change], &|did| f.keys(did))
        .unwrap();
    let (graph, report) = f
        .repo
        .preview_publication_review(&unsigned, &f.config(), &|did| f.keys(did))
        .unwrap();
    assert!(!report.conforms);
    assert!(graph
        .nodes
        .iter()
        .any(|node| node.types.contains(&Class::ReviewFinding.iri().into())));
    assert!(graph
        .nodes
        .iter()
        .any(|node| node.types.contains(&Class::IntentObservation.iri().into())));
    assert!(!graph
        .edges
        .iter()
        .any(|edge| edge.kind == Predicate::SignedBy));
    assert!(f
        .repo
        .record_publication_review(&unsigned, &f.config(), &|did| f.keys(did))
        .is_err());
    assert!(f.repo.publication_reviews().unwrap().is_empty());
    let mut forged = f.signed();
    forged.reviewed_at = "2000-01-01T00:00:00Z".into();
    assert!(f
        .repo
        .preview_publication_review(&forged, &f.config(), &|did| f.keys(did))
        .is_err());
}

#[test]
fn unresolved_and_tampered_receipts_remain_blocking() {
    let f = Fixture::new();
    let graph = ProvenanceGraphBuilder::new(SESSION, "fixture")
        .add_node(receipt_node("REAC::missing::999"))
        .changes_explained(vec![f.change])
        .build();
    let hash = f.repo.save_provenance_graph(&graph).unwrap();
    let signed = f.signed();
    assert!(!f.conforms(&signed, &f.config()));
    let pin = signed
        .subject
        .changes
        .iter()
        .find(|pin| pin.hash == f.change.to_base32())
        .unwrap();
    assert!(pin
        .observations
        .iter()
        .any(|o| o.resolution == "unresolved" && o.diagnostic.is_some()));
    let mut tampered = graph;
    tampered.nodes[0] = receipt_node(&f.work);
    std::fs::write(
        f.repo.change_store().provenance_path(&hash),
        tampered.serialize().unwrap(),
    )
    .unwrap();
    assert!(f
        .repo
        .preview_publication_review(&signed, &f.config(), &|did| f.keys(did))
        .is_err());
}

#[test]
fn historical_signed_revision_is_visible_but_not_treated_as_current() {
    let f = Fixture::new();
    let signed = f.signed();
    f.repo
        .vault_intent_update(
            &f.work,
            IntentUpdateOptions {
                title: Some("Changed after the historical signature".into()),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    let (graph, report) = f
        .repo
        .preview_publication_review(&signed, &f.config(), &|did| f.keys(did))
        .unwrap();
    assert!(!report.conforms);
    assert!(graph
        .nodes
        .iter()
        .filter_map(|node| node.metadata.as_ref())
        .any(
            |metadata| metadata.get("source_relation").and_then(|v| v.as_str())
                == Some("retained-not-current")
                && metadata.get("signature_verified").and_then(|v| v.as_bool()) == Some(true)
        ));
    assert!(!f.conforms(&signed, &f.config()));
}

#[test]
fn unindexed_provenance_is_not_silently_assigned_to_a_turn() {
    let f = Fixture::new();
    let graph = ProvenanceGraphBuilder::new("unindexed-session", "fixture")
        .add_node(receipt_node(&f.work))
        .changes_explained(vec![f.change])
        .build();
    f.repo.change_store().save_provenance_graph(&graph).unwrap();
    let signed = f.signed();
    let pin = signed
        .subject
        .changes
        .iter()
        .find(|pin| pin.hash == f.change.to_base32())
        .unwrap();
    assert!(pin
        .observations
        .iter()
        .any(|o| o.session == "unindexed-session"
            && o.turns.is_empty()
            && o.resolution == "unresolved"));
    assert!(!f.conforms(&signed, &f.config()));
}

#[test]
fn stored_review_nodes_extend_the_existing_intent_graph_without_authority_claims() {
    let f = Fixture::new();
    f.repo
        .record_publication_review(&f.signed(), &f.config(), &|did| f.keys(did))
        .unwrap();
    for id in [&f.work, &f.review] {
        let graph = f.repo.intent_graph(id, 1000).unwrap();
        assert!(graph
            .nodes
            .iter()
            .any(|node| node.has_type(Class::PublicationReview)));
        assert!(graph
            .nodes
            .iter()
            .any(|node| node.has_type(Class::ReviewSubject)));
        assert!(graph
            .nodes
            .iter()
            .any(|node| node.has_type(Class::IntentObservation)));
        assert!(graph
            .reviewed_changes
            .contains(&atomic_core::kg::iri::mint_change(&f.change)));
        assert!(graph.implementing_changes.is_empty());
        assert!(!graph
            .edges
            .iter()
            .any(|edge| matches!(edge.kind, Predicate::HasPlan | Predicate::Implements)));
        let artifact = graph
            .nodes
            .iter()
            .find(|node| node.has_type(Class::PublicationReview))
            .unwrap();
        assert_eq!(
            artifact.metadata.as_ref().unwrap()["validation"],
            "not-evaluated"
        );
        assert!(!graph
            .edges
            .iter()
            .any(|edge| edge.from_id == artifact.id && edge.kind == Predicate::VerificationResult));
    }
}

#[test]
fn preview_extends_intent_graph_but_preserves_current_intent_state() {
    let f = Fixture::new();
    let prepared = f
        .repo
        .prepare_publication_review(&f.review, &[f.change], &|did| f.keys(did))
        .unwrap();
    f.repo
        .vault_intent_update(
            &f.work,
            IntentUpdateOptions {
                title: Some(
                    "Current title must not be overwritten by a signed historical title".into(),
                ),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    let (snapshot, identity) = f
        .repo
        .workflow_snapshot_with_review(&serde_json::to_vec(&prepared).unwrap())
        .unwrap();
    assert_eq!(identity, prepared.artifact_hash());
    let baseline = f.repo.intent_graph(&f.work, 1000).unwrap();
    let graph =
        crate::intent_graph::IntentGraph::from_snapshot(&snapshot, &baseline.root, 1000).unwrap();
    let root = graph
        .nodes
        .iter()
        .find(|node| node.id == graph.root)
        .unwrap();
    assert!(root.label.starts_with("Current title must not"));
    assert!(root
        .metadata
        .as_ref()
        .unwrap()
        .get("review_contexts")
        .is_some());
    assert!(graph
        .nodes
        .iter()
        .any(|node| node.has_type(Class::ReviewEvaluation)));
    assert!(graph
        .nodes
        .iter()
        .any(|node| node.has_type(Class::ReviewFinding)));
    assert!(graph
        .nodes
        .iter()
        .any(|node| node.has_type(Class::CriterionJudgment)));
    assert!(graph
        .nodes
        .iter()
        .any(|node| node.has_type(Class::VerificationRecord)));
    assert!(graph
        .diagnostics
        .iter()
        .any(|d| d.contains("Review preview: blocked")));
    assert!(graph.implementing_changes.is_empty());
    assert!(f.repo.publication_reviews().unwrap().is_empty());
    assert!(!graph.truncated);
}

#[test]
fn feedback_updates_the_original_review_instead_of_creating_replacement_intents() {
    let f = Fixture::new();
    let other = signed_intent(&f.repo, &f.author, &f.author_key, "feature", None);
    let mut first = receipt_node(&other);
    first.id = "receipt-1".into();
    let mut second = receipt_node(&other);
    second.id = "receipt-2".into();
    let graph = ProvenanceGraphBuilder::new(SESSION, "fixture")
        .nodes(vec![first, second])
        .changes_explained(vec![f.change])
        .build();
    f.repo.save_provenance_graph(&graph).unwrap();
    let before = f.signed();
    let report = f
        .repo
        .validate_publication_review(&before, &f.config(), &|did| f.keys(did))
        .unwrap();
    assert!(!report.conforms);
    assert_eq!(
        report.results.len(),
        1,
        "Repeated receipts must produce one actionable work-scope finding: {:?}",
        report.results
    );
    assert!(report.results[0]["value"]
        .as_str()
        .unwrap()
        .ends_with(&other));
    let mut body = bridge::read_intent(&f.repo, &f.review).unwrap().body;
    body.push_str(&format!(
        "\n:::ref{{to=urn:atomic:intent:{other} edge=reviews}}\n:::\n"
    ));
    f.repo
        .vault_intent_update(
            &f.review,
            IntentUpdateOptions {
                content: Some(body),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.repo
        .vault_intent_update(
            &f.review,
            IntentUpdateOptions {
                status: Some("done".into()),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    sign_intent(&f.repo, &f.review, &f.reviewer, &f.reviewer_key);
    let after = f.signed();
    assert_eq!(before.subject.review.id, after.subject.review.id);
    let report = f
        .repo
        .validate_publication_review(&after, &f.config(), &|did| f.keys(did))
        .unwrap();
    assert!(report.conforms, "{:?}", report.results);
    let snapshot = f
        .repo
        .publication_review_snapshot(&after, &f.config(), &|did| f.keys(did))
        .unwrap();
    let other_id =
        atomic_core::kg::iri::mint_intent(&f.repo.project_id().unwrap(), &other).unwrap();
    assert!(snapshot
        .edges
        .iter()
        .any(|edge| edge.from_id == other_id && edge.kind == Predicate::HasAcceptanceCriterion));
}

#[test]
fn selected_review_lifecycle_is_context_not_work_requiring_self_review() {
    let f = Fixture::new();
    let before = f.signed();
    let mut change = Change::empty(ChangeHeader::new("review metadata only"));
    change.hashed.dependencies.push(f.change);
    let mut provenance = Provenance::new(
        AIVendor::default(),
        "fixture",
        AITool::Cli("fixture".into()),
    );
    provenance.session_id = Some(SESSION.into());
    change.hashed.provenance.push(provenance);
    change.hashed.metadata =
        atomic_core::change::envelope::SessionEnvelope::builder(SESSION, "fixture")
            .build()
            .encode()
            .unwrap();
    let hash = f.repo.save_change(&change).unwrap();
    f.repo
        .save_provenance_graph(
            &ProvenanceGraphBuilder::new(SESSION, "fixture")
                .add_node(receipt_node(&f.review))
                .changes_explained(vec![hash])
                .build(),
        )
        .unwrap();
    f.repo
        .insert_change_rec(&hash, crate::InsertOptions::default().view("review-source"))
        .unwrap();
    let state = f
        .repo
        .get_view_info("review-source")
        .unwrap()
        .state_base32();
    let set = f.repo.view_set_id("review-source").unwrap().to_base32();
    let body = bridge::read_intent(&f.repo, &f.review)
        .unwrap()
        .body
        .replace(&before.subject.view_chain[0].state, &state)
        .replace(&before.subject.effective_set_id, &set);
    f.repo
        .vault_intent_update(
            &f.review,
            IntentUpdateOptions {
                content: Some(body),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    sign_intent(&f.repo, &f.review, &f.reviewer, &f.reviewer_key);
    let prepared = f
        .repo
        .prepare_publication_review(&f.review, &[hash], &|did| f.keys(did))
        .unwrap();
    let signed = f.sign(prepared);
    let pin = signed
        .subject
        .changes
        .iter()
        .find(|pin| pin.hash == hash.to_base32())
        .unwrap();
    assert_eq!(
        pin.observations[0].role,
        atomic_canonical::publication_review::ObservationRole::ReviewContext
    );
    assert!(!signed
        .subject
        .work
        .iter()
        .any(|work| work.id == signed.subject.review.id));
    assert!(f.conforms(&signed, &f.config()));
    let mut forged = signed;
    forged
        .subject
        .changes
        .iter_mut()
        .find(|pin| pin.hash == f.change.to_base32())
        .unwrap()
        .observations[0]
        .role = atomic_canonical::publication_review::ObservationRole::ReviewContext;
    assert!(
        !f.conforms(&f.sign(forged), &f.config()),
        "Work cannot be relabeled as review context"
    );
}

#[test]
fn additive_observation_roles_do_not_readdress_retained_work_documents() {
    let f = Fixture::new();
    let signed = f.signed();
    let value = signed.value();
    for pin in value["subject"]["changes"].as_array().unwrap() {
        if let Some(observations) = pin.get("observations").and_then(|v| v.as_array()) {
            for observation in observations {
                assert!(observation.get("role").is_none());
            }
        }
    }
    let retained = PublicationReview::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(retained.artifact_hash(), signed.artifact_hash());
    assert!(f.conforms(&retained, &f.config()));
}

fn retire_fixture_work(f: &Fixture, id: &str, signer: &Identity, pair: &KeyPair) {
    let body = bridge::read_intent(&f.repo, id)
        .unwrap()
        .body
        .replace("status=met", "status=unmet");
    f.repo
        .vault_intent_update(
            id,
            IntentUpdateOptions {
                status: Some("icebox".into()),
                content: Some(body),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    sign_intent(&f.repo, id, signer, pair);
}

fn extend_fixture_review(f: &Fixture, id: &str) {
    let mut body = bridge::read_intent(&f.repo, &f.review).unwrap().body;
    body.push_str(&format!(
        "\n:::ref{{to=urn:atomic:intent:{id} edge=reviews}}\n:::\n"
    ));
    f.repo
        .vault_intent_update(
            &f.review,
            IntentUpdateOptions {
                content: Some(body),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.repo
        .vault_intent_update(
            &f.review,
            IntentUpdateOptions {
                status: Some("done".into()),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    sign_intent(&f.repo, &f.review, &f.reviewer, &f.reviewer_key);
}

#[test]
fn terminal_disposition_does_not_approve_historical_implementation_goals() {
    let f = Fixture::new();
    let retired = signed_intent(&f.repo, &f.author, &f.author_key, "feature", None);
    retire_fixture_work(&f, &retired, &f.author, &f.author_key);
    extend_fixture_review(&f, &retired);
    let signed = f.signed();
    assert_eq!(signed.subject.set_aside.len(), 1);
    assert_eq!(signed.subject.work.len(), 1);
    assert!(f.conforms(&signed,&f.config()),"An independently reviewed terminal disposition need not pretend unmet historical goals are met");
    let snapshot = f
        .repo
        .publication_review_snapshot(&signed, &f.config(), &|did| f.keys(did))
        .unwrap();
    assert!(snapshot
        .edges
        .iter()
        .any(|edge| edge.kind == Predicate::IncludesSetAside));
    let mut forged = signed;
    let pin = forged.subject.set_aside.pop().unwrap();
    forged.subject.work.push(pin);
    assert!(!f.conforms(&f.sign(forged), &f.config()));
}

#[test]
fn reviewed_set_aside_intent_observed_by_a_change_does_not_block() {
    let (f, retired) = Fixture::with_second_observed_intent();
    retire_fixture_work(&f, &retired, &f.author, &f.author_key);
    extend_fixture_review(&f, &retired);
    let signed = f.signed();
    assert_eq!(signed.subject.work.len(), 1);
    assert_eq!(signed.subject.set_aside.len(), 1);
    let observed: Vec<_> = signed
        .subject
        .changes
        .iter()
        .flat_map(|pin| pin.observations.iter())
        .filter_map(|observation| observation.intent.clone())
        .collect();
    assert!(
        observed.contains(&format!("urn:atomic:intent:{retired}")),
        "the retained receipt observes the set-aside intent"
    );
    let report = f
        .repo
        .validate_publication_review(&signed, &f.config(), &|did| f.keys(did))
        .unwrap();
    assert!(
        report.conforms,
        "an explicitly reviewed set-aside intent is complete review scope: {:#?}",
        report.results
    );
    // Dropping the set-aside intent from the review scope is still incomplete.
    let mut unreviewed = signed;
    unreviewed.subject.set_aside.clear();
    assert!(!f.conforms(&f.sign(unreviewed), &f.config()));
}

#[test]
fn terminal_disposition_still_requires_an_independent_key() {
    let f = Fixture::new();
    let retired = signed_intent(&f.repo, &f.reviewer, &f.reviewer_key, "feature", None);
    retire_fixture_work(&f, &retired, &f.reviewer, &f.reviewer_key);
    extend_fixture_review(&f, &retired);
    assert!(!f.conforms(&f.signed(), &f.config()));
}

#[test]
fn retired_intent_cannot_supply_managed_change_coverage() {
    let f = Fixture::new();
    let active = signed_intent(&f.repo, &f.author, &f.author_key, "feature", None);
    retire_fixture_work(&f, &f.work, &f.author, &f.author_key);
    extend_fixture_review(&f, &active);
    let signed = f.signed();
    assert_eq!(signed.subject.set_aside.len(), 1);
    assert_eq!(signed.subject.work.len(), 1);
    let report = f
        .repo
        .validate_publication_review(&signed, &f.config(), &|did| f.keys(did))
        .unwrap();
    assert!(!report.conforms);
    assert!(report.results.iter().any(
        |r| r["value"].as_str() == Some(atomic_core::kg::iri::mint_change(&f.change).as_str())
    ));
}
