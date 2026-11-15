//! Read canonical evidence, not best-effort KG/file-overlap attribution.
use super::*;
use atomic_canonical::{
    node::CanonicalNode,
    publication_review::{ChangePin, IntentPin, ReviewSubject},
};
use atomic_core::pristine::{node_type, GraphTxnT};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn intent(
    repo: &Repository,
    id: &str,
    keys: DidKeyProvider<'_>,
) -> Result<(CanonicalNode, IntentPin), RepositoryError> {
    read_intent(repo, id, keys, false)
}

pub(super) fn read_intent(
    repo: &Repository,
    id: &str,
    keys: DidKeyProvider<'_>,
    preview: bool,
) -> Result<(CanonicalNode, IntentPin), RepositoryError> {
    let id = id.strip_prefix("urn:atomic:intent:").unwrap_or(id);
    let inputs = crate::intent_bridge::read_intent(repo, id)?;
    let node = match crate::intent_bridge::load_attestation(repo, id, &inputs)? {
        crate::intent_bridge::Attestation::Fresh(node) => *node,
        crate::intent_bridge::Attestation::Stale(node) if preview => *node,
        _ => {
            return Err(refused(
                "review and work intents require fresh signed revisions",
            ))
        }
    };
    let did = node
        .attributed_to
        .as_deref()
        .ok_or_else(|| refused("intent has no signing identity"))?;
    let bytes = keys(did).ok_or_else(|| refused("intent signing key unavailable"))?;
    let key = atomic_identity::keypair::PublicKey::from_bytes(&bytes).map_err(refused)?;
    if !atomic_canonical::did::did_matches_public_key(did, &key) {
        return Err(refused("intent author is not bound to its signing key"));
    }
    atomic_canonical::verify(&node, &key).map_err(refused)?;
    // A mutable sourceContentHash in a sidecar is not an authenticated freshness
    // anchor. Compare the actual current lift with the signed document as well.
    let expected = atomic_canonical::proof::prepare_attestation(
        crate::intent_bridge::lift(&inputs)?.to_value(),
        &key,
    )
    .value;
    if node.id != crate::intent_bridge::lift(&inputs)?.id {
        return Err(refused(
            "signed intent identity does not match source identity",
        ));
    }
    if !preview && atomic_canonical::proof::signing_view(&node.to_value()) != expected {
        return Err(refused(
            "signed intent does not match current source inputs",
        ));
    }
    let pin = IntentPin {
        id: node.id.clone(),
        source_hash: crate::intent_bridge::source_content_hash(&inputs),
        signed_hash: atomic_canonical::hash::content_hash(&node.to_value()),
        public_key: key.to_base32(),
    };
    Ok((node, pin))
}

/// Scan both registrations and available sidecars. A registered missing/corrupt
/// original or any unreadable unindexed sidecar is uncertainty, never absence.
fn provenance(
    repo: &Repository,
    changes: &[Hash],
) -> Result<BTreeMap<Hash, atomic_core::change::ProvenanceGraph>, RepositoryError> {
    let mut hashes = BTreeSet::new();
    let txn = repo.pristine().read_txn()?;
    for hash in changes {
        if let Some(id) = txn.get_internal(hash)? {
            for dependent in txn.get_rev_deps(id)? {
                if txn.get_node_type(dependent)? == Some(node_type::PROVENANCE) {
                    hashes
                        .insert(txn.get_external(dependent)?.ok_or_else(|| {
                            refused("registered provenance has no external hash")
                        })?);
                }
            }
        }
    }
    drop(txn);
    for hash in repo.change_store().iter_provenance_graphs() {
        hashes.insert(hash.map_err(refused)?);
        if hashes.len() > 16384 {
            return Err(refused("provenance inventory exceeds review budget"));
        }
    }
    hashes
        .into_iter()
        .map(|hash| {
            let graph = repo.load_provenance_graph(&hash)?;
            Ok((hash, graph))
        })
        .collect()
}

pub(super) fn build(
    repo: &Repository,
    review_id: &str,
    roots: &[Hash],
    view: &str,
    keys: DidKeyProvider<'_>,
) -> Result<ReviewSubject, RepositoryError> {
    let mut subject = build_with_mode(repo, review_id, roots, view, keys, false, None)?;
    subject.snapshot = Some(super::snapshot::capture(repo, &subject, keys)?);
    super::snapshot::verify(repo, &subject)?;
    Ok(subject)
}

pub(super) fn build_with_mode(
    repo: &Repository,
    review_id: &str,
    roots: &[Hash],
    view: &str,
    keys: DidKeyProvider<'_>,
    preview: bool,
    retained: Option<&ReviewSubject>,
) -> Result<ReviewSubject, RepositoryError> {
    if roots.is_empty() || roots.len() > 64 {
        return Err(refused("review requires 1 to 64 root changes"));
    }
    let (review, review_pin) = read_intent(repo, review_id, keys, preview)?;
    if review.kind != "review" {
        return Err(refused("review subject must name a review intent"));
    }
    let mut work = BTreeMap::new();
    let mut set_aside = BTreeMap::new();
    for reference in &review.depends_on {
        if reference.edge == "reviews" {
            let id = reference
                .to
                .strip_prefix("urn:atomic:intent:")
                .unwrap_or(&reference.to);
            let (node, pin) = read_intent(repo, id, keys, preview)?;
            if node.status == "icebox" {
                set_aside.insert(pin.id.clone(), pin);
            } else {
                work.insert(pin.id.clone(), pin);
            }
        }
    }
    if work.len() + set_aside.len() > 256 {
        return Err(refused("review exceeds 256 work intents"));
    }
    let mut view_chain = retained
        .map(|subject| subject.view_chain.clone())
        .unwrap_or_default();
    let mut cursor = if retained.is_some() {
        None
    } else {
        Some(view.to_string())
    };
    let mut seen_views = BTreeSet::new();
    while let Some(name) = cursor {
        if !seen_views.insert(name.clone()) || view_chain.len() >= 256 {
            return Err(refused("cyclic or excessive view ancestry"));
        }
        let info = repo.get_view_info(&name)?;
        view_chain.push(atomic_canonical::publication_review::ReviewViewPin {
            name,
            state: info.state_base32(),
            scope: info.kind_label().to_string(),
        });
        cursor = info.parent_name;
    }
    if retained.is_none() && !repo.view_contains_changes(view, roots)? {
        return Err(refused("selected view does not contain the review roots"));
    }
    let mut closure = BTreeSet::new();
    let mut pending = roots.to_vec();
    while let Some(hash) = pending.pop() {
        if !closure.insert(hash) {
            continue;
        }
        if closure.len() > 4096 {
            return Err(refused("review closure exceeds 4096 changes"));
        }
        let change = repo.load_change(&hash)?;
        if change.hash().map_err(refused)? != hash {
            return Err(refused("review change hash mismatch"));
        }
        pending.extend_from_slice(change.dependencies());
        if pending.len() > 65536 {
            return Err(refused("review dependency queue exceeds budget"));
        }
    }
    let closure: Vec<_> = closure.into_iter().collect();
    let graphs = provenance(repo, &closure)?;
    let mut changes = Vec::new();
    for hash in closure {
        let change = repo.load_change(&hash)?;
        if change.hash().map_err(refused)? != hash {
            return Err(refused("review change hash mismatch"));
        }
        let mut sessions: BTreeSet<_> = change
            .provenance()
            .iter()
            .filter_map(|p| p.session_id.clone())
            .collect();
        if atomic_core::change::envelope::SessionEnvelope::is_session_envelope(
            &change.hashed.metadata,
        ) {
            sessions.insert(
                atomic_core::change::envelope::SessionEnvelope::decode(&change.hashed.metadata)
                    .map_err(refused)?
                    .session_id,
            );
        }
        if sessions.len() > 1 {
            return Err(refused("conflicting change session identities"));
        }
        let session = sessions.into_iter().next();
        let mut evidence = Vec::new();
        let mut observations = Vec::new();
        for (graph_hash, graph) in &graphs {
            if graph.explains_change(&hash) {
                evidence.push(graph_hash.to_base32());
                observations.extend(super::observations::resolve(repo, graph_hash, graph, keys)?);
            }
        }
        for observation in &mut observations {
            if observation.intent.as_deref() == Some(review_pin.id.as_str()) {
                observation.role =
                    atomic_canonical::publication_review::ObservationRole::ReviewContext;
            }
        }
        changes.push(ChangePin {
            hash: hash.to_base32(),
            session,
            governing_intents: Vec::new(),
            provenance: evidence,
            observations,
        });
    }
    changes.sort_by(|a, b| a.hash.cmp(&b.hash));
    let roots: BTreeSet<_> = roots.iter().map(Base32::to_base32).collect();
    Ok(ReviewSubject {
        project: repo.project_id()?.to_urn(),
        roots: roots.into_iter().collect(),
        changes,
        review: review_pin,
        work: work.into_values().collect(),
        set_aside: set_aside.into_values().collect(),
        profile_hash: profile_hash(),
        view_chain,
        effective_set_id: match retained {
            Some(subject) => subject.effective_set_id.clone(),
            None => repo.view_set_id(view)?.to_base32(),
        },
        snapshot: retained.and_then(|subject| subject.snapshot.clone()),
    })
}
