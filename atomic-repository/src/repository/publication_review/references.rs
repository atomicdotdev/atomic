//! Reference topology for the existing workflow/intent graph. These are claims
//! in a retained document, not verifier observations or a publication verdict.
use super::*;
use atomic_core::{
    kg::{
        iri,
        ontology::{Class as C, Predicate as P},
        project_id::ProjectId,
    },
    KgEdge, KgNode,
};
use std::collections::BTreeMap;

pub(super) fn artifact_id(review: &PublicationReview) -> Result<String, RepositoryError> {
    let bytes = data_encoding::HEXLOWER
        .decode(review.artifact_hash().as_bytes())
        .map_err(refused)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| refused("invalid review hash"))?;
    Ok(iri::mint_attestation(&Hash::from_bytes(bytes)))
}

pub(super) fn subject_id(review: &PublicationReview) -> Result<String, RepositoryError> {
    Ok(format!(
        "urn:atomic:review-subject:{}",
        atomic_canonical::hash::content_hash(
            &serde_json::to_value(&review.subject).map_err(refused)?
        )
        .replace(':', "-")
    ))
}

pub(crate) fn project(
    project: &ProjectId,
    review: &PublicationReview,
) -> Result<(Vec<KgNode>, Vec<KgEdge>), RepositoryError> {
    if review.schema != SCHEMA
        && review.schema != atomic_canonical::publication_review::LEGACY_SCHEMA
    {
        return Err(refused("unsupported review reference schema"));
    }
    if review.subject.project != project.to_urn() {
        return Err(refused("review reference belongs to another project"));
    }
    let artifact = artifact_id(review)?;
    let subject = subject_id(review)?;
    let mut nodes = BTreeMap::new();
    let mut edges = Vec::new();
    let mut node = |id: &str, class: C, label: &str| {
        nodes.entry(id.to_owned()).or_insert_with(|| {
            let mut node = KgNode::new(id, class, label, "publication-review-reference");
            node.metadata = Some(serde_json::json!({"review_reference":true,"validation":"not-evaluated","authorizes_publication":false}));
            node
        });
    };
    let intent = |id: &str| {
        iri::mint_intent(project, id.strip_prefix("urn:atomic:intent:").unwrap_or(id))
            .map_err(refused)
    };
    let change = |id: &str| {
        Hash::from_base32(id.as_bytes())
            .filter(|h| h.to_base32() == id)
            .map(|h| iri::mint_change(&h))
            .ok_or_else(|| refused("invalid review change reference"))
    };
    node(
        &artifact,
        C::PublicationReview,
        "Retained review (not revalidated)",
    );
    node(&subject, C::ReviewSubject, "Exact retained review subject");
    let review_intent = intent(&review.subject.review.id)?;
    node(&review_intent, C::ReviewIntent, "Review intent reference");
    edges.push(KgEdge::new(&artifact, &subject, P::ReviewsClosure));
    edges.push(KgEdge::new(&artifact, &review_intent, P::HasReviewIntent));
    for pin in &review.subject.work {
        let work = intent(&pin.id)?;
        node(&work, C::WorkIntent, "Reviewed intent reference");
        edges.push(KgEdge::new(&subject, &work, P::IncludesIntent));
    }
    for pin in &review.subject.set_aside {
        let id = intent(&pin.id)?;
        node(
            &id,
            C::WorkIntent,
            "Reviewed set-aside disposition (not current implementation)",
        );
        edges.push(KgEdge::new(&subject, &id, P::IncludesSetAside));
    }
    for pin in &review.subject.changes {
        let id = change(&pin.hash)?;
        node(&id, C::Change, &pin.hash);
        edges.push(KgEdge::new(&subject, &id, P::ReviewsChange));
        for (i, observation) in pin.observations.iter().enumerate() {
            let evidence = format!("{artifact}:change:{}:observation:{i}", pin.hash);
            node(
                &evidence,
                C::IntentObservation,
                &format!("{}: {}", observation.kind, observation.reference),
            );
            edges.push(KgEdge::new(&id, &evidence, P::HasReviewEvidence));
            edges.push(KgEdge::new(&subject, &evidence, P::SubjectEvidence));
            if let Some(target) = &observation.intent {
                let target = intent(target)?;
                node(&target, C::WorkIntent, &observation.reference);
                edges.push(KgEdge::new(&evidence, &target, P::ObservedIntent));
            }
            let source = Hash::from_base32(observation.provenance.as_bytes())
                .filter(|h| h.to_base32() == observation.provenance)
                .ok_or_else(|| refused("invalid review provenance reference"))?;
            let source = iri::mint_provenance(&source);
            node(&source, C::ProvenanceGraph, "Retained provenance reference");
            edges.push(KgEdge::new(&evidence, &source, P::EvidenceSource));
            let session = iri::mint_session(project, &observation.session);
            node(&session, C::Session, &observation.session);
            for number in &observation.turns {
                let turn = iri::mint_turn(project, &observation.session, *number);
                node(&turn, C::SessionTurn, &format!("Retained turn {number}"));
                edges.push(KgEdge::new(&session, &turn, P::HasTurn));
                edges.push(KgEdge::new(&evidence, &turn, P::EvidenceTurn));
            }
        }
    }
    for pin in &review.subject.changes {
        for (i, observation) in pin.observations.iter().enumerate() {
            let id = format!("{artifact}:change:{}:observation:{i}", pin.hash);
            if let Some(node) = nodes.get_mut(&id) {
                node.metadata = Some(
                    serde_json::json!({"review_reference":true,"validation":"not-evaluated",
                    "authorizes_publication":false,"pinned_observation":observation}),
                );
            }
        }
    }
    if let Some(node) = nodes.get_mut(&artifact) {
        let mut display_subject = serde_json::to_value(&review.subject).map_err(refused)?;
        if let Some(snapshot) = &review.subject.snapshot {
            display_subject["snapshot"] = serde_json::json!({"version":snapshot.version,
                "membership":snapshot.membership,"view_logs":snapshot.view_logs,
                "intent_documents_omitted_from_display":true,
                "intent_revisions":snapshot.intent_revisions.iter().map(|revision|serde_json::json!({
                    "id":revision.id,"document_hash":atomic_canonical::hash::content_hash(&revision.document)})).collect::<Vec<_>>()});
        }
        node.metadata = Some(
            serde_json::json!({"review_reference":true,"validation":"not-evaluated",
            "authorizes_publication":false,"pinned_subject":display_subject,"claimed_signer":review.attributed_to,
            "has_proof":review.proof.is_some()}),
        );
    }
    Ok((nodes.into_values().collect(), edges))
}

/// Merge display context without replacing current intent metadata or treating
/// a retrospective source revision as the current intent. Validation happens
/// before this merge, on the bounded dataset, never on the combined display.
pub(crate) fn extend(
    nodes: &mut Vec<KgNode>,
    edges: &mut Vec<KgEdge>,
    additions: (Vec<KgNode>, Vec<KgEdge>),
    context: &str,
) {
    let mut positions: BTreeMap<_, _> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.clone(), i))
        .collect();
    for node in additions.0 {
        if let Some(index) = positions.get(&node.id) {
            let current = &mut nodes[*index];
            if current.source == "publication-review-reference" && node.source != current.source {
                *current = node;
                continue;
            }
            // Current canonical types win as well: a historical signed kind
            // must not reclassify the live intent during display composition.
            if let Some(metadata) = node.metadata {
                let current_metadata = current
                    .metadata
                    .get_or_insert_with(|| serde_json::json!({}));
                if let Some(current_metadata) = current_metadata.as_object_mut() {
                    let contexts = current_metadata
                        .entry("review_contexts")
                        .or_insert_with(|| serde_json::json!({}));
                    if let Some(contexts) = contexts.as_object_mut() {
                        contexts.insert(context.into(), metadata);
                    }
                }
            }
        } else {
            positions.insert(node.id.clone(), nodes.len());
            nodes.push(node);
        }
    }
    edges.extend(additions.1);
    edges.sort_by(|a, b| (&a.from_id, a.kind, &a.to_id).cmp(&(&b.from_id, b.kind, &b.to_id)));
    edges.dedup_by(|a, b| a.from_id == b.from_id && a.kind == b.kind && a.to_id == b.to_id);
}
