use super::*;
use atomic_core::kg::project_id::{MintReason, ProjectId, ProjectIdentityRecord};
use atomic_identity::{keypair::PublicKey, KeyPair, Signer};

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
