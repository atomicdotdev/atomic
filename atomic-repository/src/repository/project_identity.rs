use super::*;
use atomic_core::kg::project_id::{MintReason, ProjectId, ProjectIdentityRecord};
use atomic_identity::{keypair::PublicKey, KeyPair, Signer};

/// The outcome of [`Repository::adopt_remote_project_identity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityAdoption {
    /// No local identity existed; the remote one is now in force.
    Adopted,
    /// The transport-replaceable `Upgrade` mint was replaced by the remote
    /// identity. The derived KG must be rebuilt (project IRIs changed).
    ReplacedUpgrade,
    /// The remote identity matches the local one; nothing changed.
    AlreadyCurrent,
    /// The remote identity differs from a locally-established (`Created` or
    /// `Adopted`) one and was NOT overwritten. Either the two repositories
    /// were never in the same lineage, or one side rebound.
    Diverged,
}

fn invalid(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: error.to_string(),
    }
}

/// Hash authority excludes proof and includes the inline namespace definitions.
pub fn project_identity_hash(object: &serde_json::Value) -> Hash {
    let value = atomic_canonical::proof::signing_view(object);
    let mut bytes = b"atomic.project-identity.v1\0".to_vec();
    bytes.extend_from_slice(atomic_canonical::jcs::canonicalize(&value).as_bytes());
    Hash::of(&bytes)
}

pub fn build_project_identity(
    reason: MintReason,
    keypair: Option<&KeyPair>,
) -> ProjectIdentityRecord {
    let id = ProjectId::generate();
    let created_at = chrono::Utc::now().to_rfc3339();
    let mut object = serde_json::json!({
        "@context": {"atomic": "https://atomic.dev/ns/workflow#", "dcterms": "http://purl.org/dc/terms/"},
        "@type": "atomic:ProjectIdentity", "atomic:projectId": id.to_base32(),
        "atomic:schemaVersion": 1, "dcterms:created": created_at,
    });
    if let Some(keypair) = keypair.filter(|_| reason == MintReason::Created) {
        object["atomic:issuer"] =
            serde_json::json!({"@id": atomic_canonical::did::did_for_public_key(&keypair.public)});
        let bytes = atomic_canonical::jcs::canonicalize(&object);
        let signature = Signer::new(keypair).sign(bytes.as_bytes());
        object = atomic_canonical::proof::attach_proof(object, &keypair.public, &signature);
    }
    ProjectIdentityRecord {
        hash: project_identity_hash(&object),
        object: atomic_canonical::jcs::canonicalize(&object).into_bytes(),
        mint_reason: reason,
        created_at,
        previous: None,
    }
}

/// Verify the prefixed content hash and, when present, the existing Data Integrity suite.
pub fn verify_project_identity(
    record: &ProjectIdentityRecord,
    public_key: Option<&PublicKey>,
) -> Result<(), RepositoryError> {
    record.project_id().map_err(invalid)?;
    let value: serde_json::Value = serde_json::from_slice(&record.object).map_err(invalid)?;
    if project_identity_hash(&value) != record.hash {
        return Err(invalid("project identity hash mismatch"));
    }
    if value.get("proof").is_some() {
        let key = public_key.ok_or_else(|| invalid("project identity signer key unavailable"))?;
        let proof: atomic_canonical::node::Proof =
            serde_json::from_value(value["proof"].clone()).map_err(invalid)?;
        let did = value["atomic:issuer"]["@id"]
            .as_str()
            .ok_or_else(|| invalid("project identity has no issuer"))?;
        if !atomic_canonical::did::did_matches_public_key(did, key)
            || proof.type_ != atomic_canonical::proof::PROOF_TYPE
            || proof.cryptosuite != atomic_canonical::proof::CRYPTOSUITE
            || proof.proof_purpose != atomic_canonical::proof::PROOF_PURPOSE
            || proof.verification_method != atomic_canonical::did::verification_method(did)
        {
            return Err(invalid("project identity proof metadata mismatch"));
        }
        let signature = atomic_canonical::proof::proof_signature(&value).map_err(invalid)?;
        let unsigned = atomic_canonical::proof::signing_view(&value);
        signature
            .verify(
                atomic_canonical::jcs::canonicalize(&unsigned).as_bytes(),
                key,
            )
            .map_err(invalid)?;
    }
    Ok(())
}

impl Repository {
    pub fn project_identity(&self) -> Result<Option<ProjectIdentityRecord>, RepositoryError> {
        Ok(self.pristine.read_txn()?.project_identity()?)
    }

    /// Mint once, before projecting project-qualified resources. Upgrade opens
    /// never load a private key. Repository creation may sign with the default identity.
    pub fn ensure_project_identity(
        &self,
        reason: MintReason,
    ) -> Result<ProjectIdentityRecord, RepositoryError> {
        if let Some(record) = self.project_identity()? {
            return Ok(record);
        }
        let keypair = if reason == MintReason::Created {
            if let Ok(store) = atomic_identity::IdentityStore::open_default() {
                match store.get_default().map_err(invalid)? {
                    Some(identity) => {
                        Some(store.load_keypair(&identity.id, None).map_err(invalid)?)
                    }
                    None => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        let record = build_project_identity(reason, keypair.as_ref());
        let mut txn = self.pristine.write_txn()?;
        let record = txn.put_project_identity_if_absent(&record)?;
        txn.commit()?;
        Ok(record)
    }

    pub fn project_id(&self) -> Result<ProjectId, RepositoryError> {
        self.project_identity()?
            .ok_or_else(|| invalid("MissingProjectIdentity"))?
            .project_id()
            .map_err(invalid)
    }

    /// Reconcile a remotely fetched identity with the local one after a clone
    /// or pull. Never invents history: an identity that differs from a
    /// locally-established (`Created`/`Adopted`) one is reported, not
    /// overwritten.
    ///
    /// - no local identity, or a transport-replaceable `Upgrade` mint → the
    ///   remote identity is verified and stored (`Adopted`/`ReplacedUpgrade`).
    ///   Replacing invalidates the derived KG (project IRIs change), so run
    ///   [`Self::rebuild_kg`] afterwards.
    /// - same content hash as local → [`IdentityAdoption::AlreadyCurrent`].
    /// - different from a `Created`/`Adopted` identity →
    ///   [`IdentityAdoption::Diverged`]; the caller should warn loudly.
    ///
    /// The canonical proof-free hash is always re-verified. DID proof
    /// verification is best-effort: it runs when the signer's key resolves
    /// locally (cached signer keys from attestation discovery, or the local
    /// default identity), and is deferred otherwise — mirroring the
    /// attestation model where an unresolvable signer stays `Unverifiable`
    /// until discovery fills the cache.
    pub fn adopt_remote_project_identity(
        &self,
        remote: &ProjectIdentityRecord,
    ) -> Result<IdentityAdoption, RepositoryError> {
        let object: serde_json::Value = serde_json::from_slice(&remote.object)
            .map_err(|e| invalid(format!("remote identity is not valid JSON: {e}")))?;
        if project_identity_hash(&object) != remote.hash {
            return Err(invalid("remote project identity hash mismatch"));
        }
        match self.remote_identity_signer_key(&object)? {
            Some(key) => verify_project_identity(remote, Some(&key))?,
            None if object.get("proof").is_none() => verify_project_identity(remote, None)?,
            // Proofed identity, signer unresolved: hash verified above; the
            // signature check is deferred to publication-time verification.
            None => {}
        }

        let adoption = {
            let mut txn = self.pristine.write_txn()?;
            let current = txn.project_identity()?;
            let already = current
                .as_ref()
                .map(|c| c.hash == remote.hash)
                .unwrap_or(false);
            if already {
                txn.commit()?;
                IdentityAdoption::AlreadyCurrent
            } else {
                match current.as_ref().map(|c| c.mint_reason) {
                    Some(MintReason::Created) | Some(MintReason::Adopted) => {
                        txn.commit()?;
                        IdentityAdoption::Diverged
                    }
                    _ => {
                        txn.adopt_project_identity(remote)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        txn.commit()?;
                        if current.is_none() {
                            IdentityAdoption::Adopted
                        } else {
                            IdentityAdoption::ReplacedUpgrade
                        }
                    }
                }
            }
        };
        Ok(adoption)
    }

    /// Resolve the signer key for a remote identity's DID proof, if the proof
    /// is present and the key is locally known: first the cached signer keys
    /// (filled by attestation signer discovery), then the local default
    /// identity when it is the issuer.
    fn remote_identity_signer_key(
        &self,
        object: &serde_json::Value,
    ) -> Result<Option<atomic_identity::PublicKey>, RepositoryError> {
        let Some(did) = object
            .get("atomic:issuer")
            .and_then(|i| i.get("@id"))
            .and_then(|v| v.as_str())
        else {
            return Ok(None); // unsigned: nothing to resolve
        };
        // Cached signer key first (survives offline after one discovery).
        if let Ok(txn) = self.pristine.read_txn() {
            if let Ok(Some(key)) = txn.signer_key(did) {
                if let Ok(key) = atomic_identity::PublicKey::from_base32(&key) {
                    if atomic_canonical::did::did_matches_public_key(did, &key) {
                        return Ok(Some(key));
                    }
                }
            }
        }
        // The local default identity may be the issuer (re-clone by the same
        // user is the common adoption case).
        if let Ok(store) = atomic_identity::IdentityStore::open_default() {
            if let Ok(Some(identity)) = store.get_default() {
                if let Ok(keypair) = store.load_keypair(&identity.id, None) {
                    let key = keypair.public;
                    if atomic_canonical::did::did_matches_public_key(did, &key) {
                        return Ok(Some(key));
                    }
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signed_identity_hash_and_proof_are_independently_verified() {
        let pair = KeyPair::generate();
        let record = build_project_identity(MintReason::Created, Some(&pair));
        verify_project_identity(&record, Some(&pair.public)).unwrap();
        let mut forged = record.clone();
        let mut value: serde_json::Value = serde_json::from_slice(&record.object).unwrap();
        value["atomic:schemaVersion"] = serde_json::json!(2);
        forged.hash = project_identity_hash(&value);
        forged.object = atomic_canonical::jcs::canonicalize(&value).into_bytes();
        assert!(verify_project_identity(&forged, Some(&pair.public)).is_err());
        let upgrade = build_project_identity(MintReason::Upgrade, Some(&pair));
        verify_project_identity(&upgrade, None).unwrap();
        assert!(!String::from_utf8(upgrade.object).unwrap().contains("proof"));
    }
}
