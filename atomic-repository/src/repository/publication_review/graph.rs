//! A verifier-owned, request-scoped dataset. Caller-supplied graph assertions
//! never enter this path. Policy lives in review-1.0.0.ttl.
use super::*;
use atomic_canonical::{node::CanonicalNode, publication_review::IntentPin};
use atomic_core::{
    kg::{
        iri,
        ontology::{Class as C, Predicate as P},
    },
    KgEdge, KgNode,
};
use serde_json::json;

struct Graph {
    nodes: Vec<KgNode>,
    edges: Vec<KgEdge>,
}
impl Graph {
    fn node(&mut self, id: &str, class: C) {
        if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
            if !node.types.iter().any(|kind| kind == class.iri()) {
                node.types.push(class.iri().to_owned());
            }
        } else {
            self.nodes.push(KgNode::new(
                id,
                class,
                format!(
                    "{} {}",
                    class.curie(),
                    id.rsplit(':')
                        .next()
                        .unwrap_or(id)
                        .chars()
                        .take(28)
                        .collect::<String>()
                ),
                "publication-review",
            ));
        }
    }
    fn edge(&mut self, from: &str, to: &str, predicate: P) {
        self.edges.push(KgEdge::new(from, to, predicate));
    }
    fn result(&mut self, id: &str, predicate: P, passed: bool) {
        let status = format!(
            "https://atomic.dev/ns/workflow#{}",
            if passed { "Verified" } else { "Failed" }
        );
        self.node(&status, C::VerificationStatus);
        self.edge(id, &status, predicate);
    }
    fn metadata(&mut self, id: &str, metadata: serde_json::Value) {
        if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
            node.metadata = Some(metadata);
        }
    }
}

fn intent_iri(repo: &Repository, id: &str) -> Result<String, RepositoryError> {
    iri::mint_intent(
        &repo.project_id()?,
        id.strip_prefix("urn:atomic:intent:").unwrap_or(id),
    )
    .map_err(refused)
}

fn add_intent(
    repo: &Repository,
    graph: &mut Graph,
    node: &CanonicalNode,
    pin: &IntentPin,
    keys: DidKeyProvider<'_>,
    preview: bool,
) -> Result<String, RepositoryError> {
    let id = intent_iri(repo, &node.id)?;
    graph.node(
        &id,
        if node.kind == "review" {
            C::ReviewIntent
        } else {
            C::WorkIntent
        },
    );
    graph.metadata(
        &id,
        json!({"status":node.status,"title":node.title,
        "source_hash":pin.source_hash,"signed_hash":pin.signed_hash,
        "public_key":pin.public_key}),
    );
    if let Some(projected) = graph.nodes.iter_mut().find(|projected| projected.id == id) {
        projected.label = node.title.clone();
    }
    graph.result(&id, P::VerificationResult, true); // subject::intent authenticated the signature
    graph.result(
        &id,
        P::FreshnessResult,
        subject::intent(repo, &node.id, keys).is_ok(),
    );
    graph.result(
        &id,
        P::CanonicalConformance,
        atomic_canonical::validate_intent(node).conforms,
    );
    let did = node
        .attributed_to
        .as_deref()
        .ok_or_else(|| refused("missing intent author"))?;
    graph.node(did, C::Identity);
    let key_id = format!("urn:atomic:signing-key:{}", pin.public_key);
    graph.node(&key_id, C::SigningKey);
    graph.edge(did, &key_id, P::SigningKey);
    graph.edge(&id, did, P::AuthoredBy);
    for (i, ac) in node.has_acceptance_criterion.iter().enumerate() {
        let criterion = format!("{id}:criterion:{i}");
        graph.node(&criterion, C::AcceptanceCriterion);
        graph.metadata(
            &criterion,
            json!({"status":ac.ac_status,"evidence":ac.evidence,
            "verified_by":ac.verified_by,"required_kinds":ac.required_kinds}),
        );
        graph.edge(&id, &criterion, P::HasAcceptanceCriterion);
        for (j, record) in ac.verifications.iter().enumerate() {
            let verification = format!("{criterion}:verification:{j}");
            graph.node(&verification, C::VerificationRecord);
            graph.metadata(
                &verification,
                json!({"status":record.outcome,
                "verification_kind":record.kind,"observed_state":record.observed_at_merkle,
                "verification_reference":record.reference}),
            );
            graph.edge(&criterion, &verification, P::HasVerification);
        }
    }
    for reference in &node.depends_on {
        if reference.edge == "reviews" {
            let (work, _) = subject::read_intent(repo, &reference.to, keys, preview)?;
            graph.edge(&id, &intent_iri(repo, &work.id)?, P::Reviews);
        }
        if reference.edge == "implementsChange"
            && reference
                .declaration_evidence
                .as_deref()
                .is_some_and(|e| !e.trim().is_empty())
        {
            let hash = iri::parse_change(&reference.to).map_err(refused)?;
            graph.edge(&id, &iri::mint_change(&hash), P::DeclaresImplementation);
        }
    }
    Ok(id)
}

pub(super) fn build(
    repo: &Repository,
    review: &PublicationReview,
    config: &PublicationGateConfig,
    keys: DidKeyProvider<'_>,
    preview: bool,
) -> Result<(Vec<KgNode>, Vec<KgEdge>), RepositoryError> {
    PublicationReview::parse(&serde_json::to_vec(review).map_err(refused)?).map_err(refused)?;
    if review.schema != SCHEMA
        && review.schema != atomic_canonical::publication_review::LEGACY_SCHEMA
    {
        return Err(refused("unsupported review schema"));
    }
    chrono::DateTime::parse_from_rfc3339(&review.reviewed_at).map_err(refused)?;
    let bytes =
        keys(&review.attributed_to).ok_or_else(|| refused("review signing key unavailable"))?;
    let key = key(bytes)?;
    if !atomic_canonical::did::did_matches_public_key(&review.attributed_to, &key) {
        return Err(refused("review identity does not match its key"));
    }
    let signed = review.proof.is_some();
    if signed || !preview {
        atomic_canonical::proof::verify_value(&review.value(), &key).map_err(refused)?;
    }
    let roots = review
        .subject
        .roots
        .iter()
        .map(|hash| {
            Hash::from_base32(hash.as_bytes())
                .filter(|parsed| parsed.to_base32() == *hash)
                .ok_or_else(|| refused("review roots must be full canonical hashes"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let selected = review
        .subject
        .view_chain
        .first()
        .ok_or_else(|| refused("review source perspective is missing"))?;
    let retained = if review.schema == SCHEMA {
        super::snapshot::verify(repo, &review.subject)?;
        Some(&review.subject)
    } else {
        None
    };
    let current = subject::build_with_mode(
        repo,
        review
            .subject
            .review
            .id
            .strip_prefix("urn:atomic:intent:")
            .unwrap_or(&review.subject.review.id),
        &roots,
        &selected.name,
        keys,
        preview,
        retained,
    )?;
    let fresh = current == review.subject;
    let mut graph = Graph {
        nodes: Vec::new(),
        edges: Vec::new(),
    };
    let artifact = super::references::artifact_id(review)?;
    let subject_id = super::references::subject_id(review)?;
    graph.node("urn:atomic:review:evaluation", C::ReviewEvaluation);
    graph.node(&artifact, C::PublicationReview);
    graph.metadata(
        &artifact,
        json!({"status":if signed {"signed"} else {"unsigned-preview"},
        "review_ontology":atomic_core::kg::ontology::REVIEW_VERSION_IRI}),
    );
    graph.node(&subject_id, C::ReviewSubject);
    graph.metadata(
        &subject_id,
        json!({"reviewed_state":current.view_chain[0].state,
            "reviewed_context":format!("urn:atomic:view-set:{}",current.effective_set_id),
            "pinned_source":review.subject.view_chain,
            "pinned_effective_set_id":review.subject.effective_set_id,
            "source_context":if retained.is_some() {"retained-immutable"} else {"legacy-live-view"},
            "current_intent_revisions_match":fresh,
            "current_source":current.view_chain,"fresh":fresh}),
    );
    for pin in &current.view_chain {
        let view = iri::mint_view(&repo.project_id()?, &pin.name);
        graph.node(&view, C::View);
        graph.metadata(
            &view,
            json!({"name":pin.name,"state":pin.state,
            "historical_reference":retained.is_some(),"requires_live_view":retained.is_none()}),
        );
        graph.edge(&subject_id, &view, P::References);
    }
    graph.edge(
        "urn:atomic:review:evaluation",
        &artifact,
        P::EvaluatesReview,
    );
    graph.edge(&artifact, &subject_id, P::ReviewsClosure);
    if let Some(snapshot) = &review.subject.snapshot {
        let evidence = format!("{subject_id}:retained-source");
        graph.node(&evidence, C::ReviewEvidence);
        graph.metadata(&evidence,json!({"kind":"retained-source-snapshot",
            "membership":snapshot.membership,"effective_set_id":review.subject.effective_set_id,
            "authenticated_intent_revisions":snapshot.intent_revisions.iter().map(|revision|&revision.id).collect::<Vec<_>>(),
            "requires_live_view":false}));
        graph.edge(&subject_id, &evidence, P::References);
    }
    graph.result(&artifact, P::VerificationResult, signed);
    graph.result(&artifact, P::FreshnessResult, fresh);
    graph.result(
        &artifact,
        P::AuthorityResult,
        config
            .trust
            .evaluate(&review.attributed_to, config.repository_identity.as_deref())
            == atomic_config::SignerTrust::Trusted,
    );
    graph.node(&review.attributed_to, C::Identity);
    if signed {
        graph.edge(&artifact, &review.attributed_to, P::SignedBy);
    } else {
        graph.edge(&artifact, &review.attributed_to, P::AuthoredBy);
    }
    let key_id = format!("urn:atomic:signing-key:{}", key.to_base32());
    graph.node(&key_id, C::SigningKey);
    graph.edge(&review.attributed_to, &key_id, P::SigningKey);
    let (review_node, review_pin) = subject::read_intent(repo, &current.review.id, keys, preview)?;
    let review_id = add_intent(repo, &mut graph, &review_node, &review_pin, keys, preview)?;
    graph.edge(&artifact, &review_id, P::HasReviewIntent);
    for (i, ac) in review_node.has_acceptance_criterion.iter().enumerate() {
        let criterion = format!("{review_id}:criterion:{i}");
        let judgment = format!("{artifact}:judgment:{i}");
        graph.node(&judgment, C::CriterionJudgment);
        graph.metadata(
            &judgment,
            json!({"status":ac.ac_status,"meaning":"reviewer assertion, not original execution"}),
        );
        graph.edge(&artifact, &judgment, P::HasJudgment);
        graph.edge(&judgment, &criterion, P::JudgesCriterion);
        for j in 0..ac.verifications.len() {
            graph.edge(
                &judgment,
                &format!("{criterion}:verification:{j}"),
                P::SupportedBy,
            );
        }
    }
    for work in &current.work {
        let (node, pin) = subject::read_intent(repo, &work.id, keys, preview)?;
        let work_id = add_intent(repo, &mut graph, &node, &pin, keys, preview)?;
        graph.edge(&subject_id, &work_id, P::IncludesIntent);
    }
    for disposition in &current.set_aside {
        let (node, pin) = subject::read_intent(repo, &disposition.id, keys, preview)?;
        let id = add_intent(repo, &mut graph, &node, &pin, keys, preview)?;
        graph.edge(&subject_id, &id, P::IncludesSetAside);
    }
    let mut projected_intents: std::collections::BTreeSet<_> = current
        .work
        .iter()
        .chain(current.set_aside.iter())
        .map(|pin| pin.id.clone())
        .chain(std::iter::once(current.review.id.clone()))
        .collect();
    for pin in &current.changes {
        let hash =
            Hash::from_base32(pin.hash.as_bytes()).ok_or_else(|| refused("bad change hash"))?;
        let id = iri::mint_change(&hash);
        graph.node(&id, C::Change);
        graph.metadata(
            &id,
            json!({"status":if pin.session.is_some() {"managed"} else {"unmanaged"},
            "review_role":if current.roots.contains(&pin.hash) {"selected-root"} else {"dependency"},
            "object_availability":"available","projection_completeness":"projected"}),
        );
        graph.edge(&subject_id, &id, P::ReviewsChange);
        graph.edge(&artifact, &id, P::CoversChange);
        graph.edge(&id, &artifact, P::AttestedBy);
        if let Some(session_id) = &pin.session {
            let project = repo.project_id()?;
            let session = iri::mint_session(&project, session_id);
            graph.node(&session, C::Session);
            let turns = repo
                .get_session_ledger(session_id)?
                .map(|(_, turns)| turns)
                .unwrap_or_default();
            let matching: Vec<_> = turns
                .iter()
                .filter(|turn| turn.change_hashes.contains(&hash))
                .collect();
            for row in matching {
                let turn = iri::mint_turn(&project, session_id, row.turn_number);
                graph.node(&turn, C::SessionTurn);
                graph.edge(&session, &turn, P::HasTurn);
                graph.edge(&turn, &id, P::GeneratedChange);
                for evidence in &pin.provenance {
                    let hash = Hash::from_base32(evidence.as_bytes())
                        .ok_or_else(|| refused("invalid provenance hash"))?;
                    let provenance = iri::mint_provenance(&hash);
                    graph.node(&provenance, C::ProvenanceGraph);
                    if row.provenance_hash == hash {
                        graph.edge(&turn, &provenance, P::TurnExplainedBy);
                    }
                    graph.edge(&id, &provenance, P::ChangeExplainedBy);
                }
            }
            for code in [
                "MissingIntentEvidence",
                "MissingProvenance",
                "MissingAttestation",
            ] {
                let missing = match code {
                    "MissingIntentEvidence" => pin.observations.is_empty(),
                    "MissingProvenance" => pin.provenance.is_empty(),
                    _ => repo.review_original_attestations(&hash)?.is_empty(),
                };
                if missing {
                    let finding = format!("{artifact}:change:{}:review-gap:{code}", pin.hash);
                    graph.node(&finding, C::ReviewFinding);
                    graph.metadata(&finding, json!({"finding_code":code}));
                    graph.edge(&finding, &id, P::AboutResource);
                }
            }
        }
        for (i, observation) in pin.observations.iter().enumerate() {
            let evidence = format!("{artifact}:change:{}:observation:{i}", pin.hash);
            graph.node(&evidence, C::IntentObservation);
            graph.node(&evidence, C::ReviewEvidence);
            if let Some(node) = graph.nodes.iter_mut().find(|node| node.id == evidence) {
                node.label = format!(
                    "{}: {} ({})",
                    observation.kind, observation.reference, observation.resolution
                );
            }
            graph.metadata(
                &evidence,
                json!({"status":observation.resolution,
                "observation_kind":observation.kind,"source_node":observation.node,
                "observation_role":observation.role,
                "source_hash":observation.provenance,"session":observation.session,
                "source_reference":observation.reference,
                "diagnostic":observation.diagnostic,
                "meaning":"retained receipt, not governing authority or execution verification"}),
            );
            graph.edge(&id, &evidence, P::HasReviewEvidence);
            graph.edge(&subject_id, &evidence, P::SubjectEvidence);
            if let Some(revision) = &observation.retained_revision {
                let revision_id = format!(
                    "urn:atomic:review-revision:{}",
                    revision.signed_hash.replace(':', "-")
                );
                graph.node(&revision_id, C::ReviewEvidence);
                graph.metadata(
                    &revision_id,
                    json!({"signed_hash":revision.signed_hash,
                    "public_key":revision.public_key,"source_relation":revision.source_relation,
                    "signature_verified":true,
                    "meaning":"retained signed document; does not authorize current work"}),
                );
                graph.edge(&evidence, &revision_id, P::References);
            }
            let hash = Hash::from_base32(observation.provenance.as_bytes())
                .ok_or_else(|| refused("invalid observation provenance hash"))?;
            let provenance = iri::mint_provenance(&hash);
            graph.node(&provenance, C::ProvenanceGraph);
            graph.edge(&evidence, &provenance, P::EvidenceSource);
            graph.edge(&id, &provenance, P::ChangeExplainedBy);
            let project = repo.project_id()?;
            let session = iri::mint_session(&project, &observation.session);
            graph.node(&session, C::Session);
            for number in &observation.turns {
                let turn = iri::mint_turn(&project, &observation.session, *number);
                graph.node(&turn, C::SessionTurn);
                graph.edge(&session, &turn, P::HasTurn);
                graph.edge(&turn, &provenance, P::TurnExplainedBy);
                graph.edge(&evidence, &turn, P::EvidenceTurn);
            }
            if let Some(target) = &observation.intent {
                let target_id = intent_iri(repo, target)?;
                if projected_intents.insert(target.clone()) {
                    match subject::read_intent(repo, target, keys, true) {
                        Ok((node, pin)) => {
                            add_intent(repo, &mut graph, &node, &pin, keys, true)?;
                        }
                        Err(error) => {
                            graph.node(&target_id, C::WorkIntent);
                            graph.metadata(&target_id,json!({"projection_error":error.to_string(),"meaning":"unresolved intent reference, not absent historical evidence"}));
                        }
                    }
                }
                graph.node(&target_id, C::WorkIntent);
                graph.edge(&evidence, &target_id, P::ObservedIntent);
            }
            if observation.resolution != "resolved"
                || pin
                    .session
                    .as_ref()
                    .is_some_and(|s| s != &observation.session)
            {
                graph.result(&evidence, P::FreshnessResult, false);
                let finding = format!("{evidence}:finding");
                graph.node(&finding, C::ReviewFinding);
                graph.metadata(&finding, json!({"finding_code":"UnresolvedIntentEvidence","status":observation.resolution}));
                graph.edge(&finding, &evidence, P::AboutResource);
            } else {
                graph.result(&evidence, P::FreshnessResult, true);
            }
        }
        for dep in repo.load_change(&hash)?.dependencies() {
            graph.edge(&id, &iri::mint_change(dep), P::DirectlyDependsOnChange);
        }
    }
    Ok((graph.nodes, graph.edges))
}
