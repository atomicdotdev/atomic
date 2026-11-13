//! Immutable source evidence. View names are provenance labels, never resources
//! that must be kept alive to revalidate a signed approval.
use super::*;
use atomic_canonical::publication_review::{RetainedIntentRevision, ReviewSnapshot, ReviewSubject};
use atomic_core::pristine::{EffectiveProjectionClosure, GraphTxnT, ViewMembershipSet, ViewTxnT};
use atomic_core::types::SetId;
use std::collections::BTreeSet;

const MAX_MEMBERS: usize = 16384;
const MAX_LOG_REFS: usize = 65536;

fn hash(value: &str) -> Result<Hash, RepositoryError> {
    Hash::from_base32(value.as_bytes())
        .filter(|h| h.to_base32() == value)
        .ok_or_else(|| refused("snapshot requires full canonical change hashes"))
}

pub(super) fn capture(
    repo: &Repository,
    subject: &ReviewSubject,
    keys: DidKeyProvider<'_>,
) -> Result<ReviewSnapshot, RepositoryError> {
    let txn = repo.pristine().read_txn()?;
    let mut view_logs = Vec::new();
    let mut refs = 0;
    for pin in &subject.view_chain {
        let view = txn
            .get_view(&pin.name)?
            .ok_or_else(|| refused("source view disappeared during capture"))?;
        if view.state.to_base32() != pin.state {
            return Err(refused("source view changed during capture"));
        }
        let mut log = Vec::new();
        for row in txn.iter_changes(&view, 0)? {
            let (_, id, _) = row?;
            log.push(
                txn.get_external(id)?
                    .ok_or_else(|| refused("source member has no external hash"))?
                    .to_base32(),
            );
            refs += 1;
            if refs > MAX_LOG_REFS {
                return Err(refused("source log snapshot exceeds budget"));
            }
        }
        view_logs.push(log);
    }
    let selected = subject
        .view_chain
        .first()
        .ok_or_else(|| refused("source context missing"))?;
    let view = txn
        .get_view(&selected.name)?
        .ok_or_else(|| refused("source view disappeared"))?;
    let closure = super::super::effective_projection_closure(&txn, &view)?;
    if closure.len() > MAX_MEMBERS {
        return Err(refused("source membership snapshot exceeds budget"));
    }
    let mut membership = BTreeSet::new();
    for id in closure.iter_dependency_first() {
        membership.insert(
            txn.get_external(*id)?
                .ok_or_else(|| refused("snapshot member has no hash"))?
                .to_base32(),
        );
    }
    drop(txn);
    let mut revisions = Vec::new();
    for pin in std::iter::once(&subject.review)
        .chain(subject.work.iter())
        .chain(subject.set_aside.iter())
    {
        let bare = pin.id.strip_prefix("urn:atomic:intent:").unwrap_or(&pin.id);
        let inputs = crate::intent_bridge::read_intent(repo, bare)?;
        let (node, current) = subject::intent(repo, bare, keys)?;
        if current != *pin {
            return Err(refused("intent changed during source capture"));
        }
        revisions.push(RetainedIntentRevision {
            id: pin.id.clone(),
            frontmatter: inputs.frontmatter,
            body: inputs.body,
            document: node.to_value(),
        });
    }
    revisions.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(ReviewSnapshot {
        version: 1,
        view_logs,
        membership: membership.into_iter().collect(),
        intent_revisions: revisions,
    })
}

pub(super) fn verify(repo: &Repository, subject: &ReviewSubject) -> Result<(), RepositoryError> {
    let snapshot = subject
        .snapshot
        .as_ref()
        .ok_or_else(|| refused("retained source snapshot missing"))?;
    if snapshot.version != 1
        || subject.view_chain.is_empty()
        || subject.view_chain.len() > 256
        || snapshot.view_logs.len() != subject.view_chain.len()
        || snapshot.membership.len() > MAX_MEMBERS
    {
        return Err(refused("invalid or unsupported retained source context"));
    }
    let mut names = BTreeSet::new();
    let mut total = 0;
    let mut direct = Vec::new();
    let txn = repo.pristine().read_txn()?;
    // Traverse root-to-leaf, exactly like canonical live view membership.
    for (pin, log) in subject.view_chain.iter().zip(&snapshot.view_logs).rev() {
        if !names.insert(&pin.name) || !matches!(pin.scope.as_str(), "draft" | "shared") {
            return Err(refused("invalid retained view labels"));
        }
        let mut state = Hash::ZERO;
        let mut seen = BTreeSet::new();
        for entry in log {
            total += 1;
            if total > MAX_LOG_REFS {
                return Err(refused("retained source logs exceed budget"));
            }
            let h = hash(entry)?;
            if !seen.insert(h) {
                return Err(refused("duplicate change in retained view log"));
            }
            state = state.next(&h);
            direct.push(
                txn.get_internal(&h)?
                    .ok_or_else(|| refused("retained change registration missing"))?,
            );
        }
        if state.to_base32() != pin.state {
            return Err(refused("retained view log does not prove its Merkle state"));
        }
    }
    let closure = EffectiveProjectionClosure::try_from_membership(
        &txn,
        &ViewMembershipSet::from_ordered(direct),
    )?;
    if closure.len() > MAX_MEMBERS {
        return Err(refused("retained dependency closure exceeds budget"));
    }
    let mut actual = BTreeSet::new();
    for id in closure.iter_dependency_first() {
        actual.insert(
            txn.get_external(*id)?
                .ok_or_else(|| refused("retained change hash missing"))?
                .to_base32(),
        );
    }
    let declared: BTreeSet<_> = snapshot.membership.iter().cloned().collect();
    if declared.len() != snapshot.membership.len()
        || snapshot.membership.iter().ne(declared.iter())
        || declared != actual
    {
        return Err(refused(
            "retained membership is not the exact effective source closure",
        ));
    }
    let mut set = SetId::ZERO;
    for entry in &snapshot.membership {
        let h = hash(entry)?;
        let change = repo.load_change(&h)?;
        if change.hash().map_err(refused)? != h {
            return Err(refused("retained source change hash mismatch"));
        }
        // An authenticated store object and its dependency index must agree.
        let id = txn
            .get_internal(&h)?
            .ok_or_else(|| refused("retained source change is unregistered"))?;
        let indexed: BTreeSet<_> = txn.get_indexed_change_deps(id)?.into_iter().collect();
        let stored: BTreeSet<_> = change.dependencies().iter().copied().collect();
        if indexed != stored {
            return Err(refused(
                "retained source dependency index disagrees with immutable change",
            ));
        }
        set = set.add(&h);
    }
    if set.to_base32() != subject.effective_set_id
        || subject.roots.iter().any(|root| !declared.contains(root))
    {
        return Err(refused(
            "retained source membership does not prove the reviewed SetId or roots",
        ));
    }
    drop(txn);
    let pins: Vec<_> = std::iter::once(&subject.review)
        .chain(subject.work.iter())
        .chain(subject.set_aside.iter())
        .collect();
    if pins.len() != snapshot.intent_revisions.len() || pins.len() > 257 {
        return Err(refused("retained intent revision inventory mismatch"));
    }
    let mut ids = BTreeSet::new();
    for revision in &snapshot.intent_revisions {
        if !ids.insert(&revision.id) {
            return Err(refused("duplicate retained intent revision"));
        }
        let pin = pins
            .iter()
            .find(|pin| pin.id == revision.id)
            .ok_or_else(|| refused("unscoped retained intent revision"))?;
        let inputs = crate::intent_bridge::LiftInputs {
            frontmatter: revision.frontmatter.clone(),
            body: revision.body.clone(),
        };
        let node: atomic_canonical::node::CanonicalNode =
            serde_json::from_value(revision.document.clone()).map_err(refused)?;
        let key =
            atomic_identity::keypair::PublicKey::from_base32(&pin.public_key).map_err(refused)?;
        let did = node
            .attributed_to
            .as_deref()
            .ok_or_else(|| refused("retained intent has no author"))?;
        if node.id != pin.id
            || !atomic_canonical::did::did_matches_public_key(did, &key)
            || crate::intent_bridge::source_content_hash(&inputs) != pin.source_hash
            || atomic_canonical::hash::content_hash(&node.to_value()) != pin.signed_hash
        {
            return Err(refused("retained intent source/signature pin mismatch"));
        }
        atomic_canonical::verify(&node, &key).map_err(refused)?;
        let expected = atomic_canonical::proof::prepare_attestation(
            crate::intent_bridge::lift(&inputs)?.to_value(),
            &key,
        )
        .value;
        if atomic_canonical::proof::signing_view(&node.to_value()) != expected {
            return Err(refused(
                "retained signed intent does not match its source inputs",
            ));
        }
    }
    Ok(())
}
