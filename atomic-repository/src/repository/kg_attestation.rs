//! Cryptographic verification is offline; network lookup only fills the
//! DID-checked key cache through the repository boundary.

use super::*;
use atomic_core::pristine::{KgMutTxnT, KgTxnT};
use atomic_core::{
    kg::{
        iri,
        ontology::{Class as C, Predicate as P},
        project_id::MintReason,
    },
    KgEdge, KgNode,
};
use atomic_identity::keypair::PublicKey;

impl Repository {
    pub fn cache_signer_key(&self, did: &str, key: &PublicKey) -> Result<(), RepositoryError> {
        if !did.starts_with("did:atomic:")
            || !atomic_canonical::did::did_matches_public_key(did, key)
        {
            return Err(RepositoryError::InvalidOperation {
                message: "signer DID does not match the supplied public key".into(),
            });
        }
        self.ensure_project_identity(MintReason::Upgrade)?;
        let mut txn = self.pristine.write_txn()?;
        txn.put_signer_key(did, &key.to_base32())?;
        if txn.kg_writes_enabled()? {
            self.project_attestations_kg(&mut txn)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn unresolved_attestation_signers(&self) -> Result<Vec<String>, RepositoryError> {
        let txn = self.pristine.read_txn()?;
        let mut result = std::collections::BTreeSet::new();
        for hash in self.change_store.iter_attestations() {
            let attestation = self
                .load_attestation(&hash.map_err(|e| RepositoryError::Database(e.to_string()))?)?;
            if let Some(did) = attestation
                .signer
                .as_deref()
                .filter(|did| did.starts_with("did:atomic:"))
            {
                if txn.signer_key(did)?.is_none() {
                    result.insert(did.to_owned());
                }
            }
        }
        Ok(result.into_iter().collect())
    }

    pub(super) fn project_attestations_kg(
        &self,
        txn: &mut atomic_core::WriteTxn<'_>,
    ) -> Result<(), RepositoryError> {
        let project = txn.kg_project()?;
        let local = atomic_identity::IdentityStore::open_default()
            .ok()
            .and_then(|store| store.list().ok())
            .unwrap_or_default();
        let sessions = txn.kg_session_rows()?;
        for hash in self.change_store.iter_attestations() {
            let hash = hash.map_err(|e| RepositoryError::Database(e.to_string()))?;
            let attestation = self.load_attestation(&hash)?;
            let id = iri::mint_attestation(&hash);
            txn.upsert_kg_node(&KgNode::new(
                &id,
                C::Attestation,
                &hash.to_base32()[..12],
                "attestation",
            ))?;
            for edge in txn.get_kg_edges_from(&id)? {
                txn.del_kg_edge(&edge.from_id, &edge.to_id, &edge.kind)?;
            }
            let session = iri::mint_session(&project, &attestation.session_id);
            if txn.get_kg_node(&session)?.is_none() {
                txn.upsert_kg_node(&KgNode::new(
                    &session,
                    C::Session,
                    &attestation.session_id,
                    "attestation",
                ))?;
            }
            txn.upsert_kg_edge(&KgEdge::new(&id, session, P::AttestsSession))?;
            for hash in &attestation.changes_covered {
                let change = iri::mint_change(hash);
                if txn.get_kg_node(&change)?.is_none() {
                    txn.upsert_kg_node(&KgNode::new(
                        &change,
                        C::Change,
                        &hash.to_base32()[..12],
                        "attestation",
                    ))?;
                }
                txn.upsert_kg_edge(&KgEdge::new(&id, &change, P::CoversChange))?;
                txn.upsert_kg_edge(&KgEdge::new(&change, &id, P::AttestedBy))?;
                txn.kg_clear_finding("MissingAttestation", &change)?;
            }
            let mut verification = "Unverifiable";
            if let Some(signer) = attestation.signer.as_deref() {
                let signer_id = if signer.starts_with("did:atomic:") {
                    signer.to_owned()
                } else {
                    iri::mint_named_identity(&project, signer)
                };
                txn.upsert_kg_node(&KgNode::new(&signer_id, C::Identity, signer, "attestation"))?;
                txn.upsert_kg_edge(&KgEdge::new(&id, signer_id, P::SignedBy))?;
                if signer.starts_with("did:atomic:") {
                    let key = local
                        .iter()
                        .find(|identity| {
                            atomic_canonical::did::did_matches_public_key(
                                signer,
                                &identity.public_key,
                            )
                        })
                        .map(|identity| identity.public_key.clone())
                        .or_else(|| {
                            txn.signer_key(signer)
                                .ok()
                                .flatten()
                                .and_then(|key| PublicKey::from_base32(&key).ok())
                        });
                    if let Some(key) =
                        key.filter(|key| atomic_canonical::did::did_matches_public_key(signer, key))
                    {
                        verification = if attestation.verify_with_did(key.as_bytes()) {
                            "Verified"
                        } else {
                            "Failed"
                        };
                    }
                }
            }
            txn.upsert_kg_edge(&KgEdge::new(
                &id,
                format!("https://atomic.dev/ns/workflow#{verification}"),
                P::VerificationResult,
            ))?;
            txn.kg_clear_finding("UnresolvedReference", &id)?;
            if let Some(previous) = attestation.previous_attestation {
                if self.has_attestation(&previous) {
                    let previous = iri::mint_attestation(&previous);
                    if txn.get_kg_node(&previous)?.is_none() {
                        txn.upsert_kg_node(&KgNode::new(
                            &previous,
                            C::Attestation,
                            "previous attestation",
                            "attestation",
                        ))?;
                    }
                    txn.upsert_kg_edge(&KgEdge::new(&id, previous, P::PreviousAttestation))?;
                } else {
                    txn.kg_finding(
                        "UnresolvedReference",
                        &id,
                        "The previous attestation sidecar is absent",
                    )?;
                }
            }
            let matches: Vec<_> = sessions
                .iter()
                .filter(|(record, _)| record.session_id == attestation.session_id)
                .flat_map(|(_, turns)| turns)
                .filter(|turn| {
                    !turn.change_hashes.is_empty()
                        && turn
                            .change_hashes
                            .iter()
                            .all(|hash| attestation.changes_covered.contains(hash))
                })
                .collect();
            txn.kg_clear_finding("AmbiguousAttestationTurn", &id)?;
            if matches.len() == 1 {
                let turn = matches[0];
                let turn = iri::mint_turn(&project, &turn.session_id, turn.turn_number);
                if txn.get_kg_node(&turn)?.is_some() {
                    txn.upsert_kg_edge(&KgEdge::new(&id, turn, P::Attests))?;
                }
            } else {
                txn.kg_finding(
                    "AmbiguousAttestationTurn",
                    &id,
                    "Attestation coverage does not identify exactly one non-empty session turn",
                )?;
            }
        }
        Ok(())
    }
}
