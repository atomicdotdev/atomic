//! Purpose-bound local authority for a disposable publication reviewer.
//!
//! These certificates are not identity grants and never modify signer trust.
//! Only an issuer trusted by the current raw signer policy can delegate, and
//! only for one exact immutable subject. Configured repository identity trust
//! is accepted; broad identity grants and transitive trust are not consulted.
//! SHACL remains the publication policy evaluator.
use super::provenance_gate::{DidKeyProvider, PublicationGateConfig};
use super::{Repository, RepositoryError};
use atomic_canonical::{
    node::Proof,
    publication_review::{PublicationReview, ReviewSubject},
};
use atomic_config::SignerTrust;
use atomic_core::{
    pristine::{MutTxnT, VaultEntry, VaultEntryType, VaultMutTxnT, VaultTxnT},
    types::{Base32, Hash, Merkle},
};
use atomic_identity::{Identity, KeyPair, PublicKey};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const PREFIX: &str = "review-authorizations/";
const SCHEMA: &str = "atomic.review-authorization.v1";
const REVOCATION_SCHEMA: &str = "atomic.review-authorization-revocation.v1";
const PURPOSE: &str = "review-publication";
const MAX_BYTES: usize = 4 * 1024 * 1024;

/// An issuer-signed exact-subject capability, including only public key material.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAuthorization {
    pub schema: String,
    pub id: String,
    pub purpose: String,
    pub project: String,
    pub delegate: String,
    pub delegate_public_key: String,
    /// Full signed scope: no wildcards, subset matching, or inferred closure.
    pub subject: ReviewSubject,
    pub created_at: String,
    pub expires_at: String,
    #[serde(rename = "attributedTo")]
    pub issuer: String,
    pub issuer_public_key: String,
    #[serde(
        rename = "contentHash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<Proof>,
}

/// Explicit, issuer-signed invalidation. It deliberately denies even historical
/// authority, while retained public keys remain available for inspection.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAuthorizationRevocation {
    pub schema: String,
    pub purpose: String,
    pub authorization: String,
    pub project: String,
    pub revoked_at: String,
    #[serde(rename = "attributedTo")]
    pub issuer: String,
    #[serde(
        rename = "contentHash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<Proof>,
}

fn refused(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!("review authorization: {error}"),
    }
}

fn value<T: Serialize>(document: &T) -> Result<serde_json::Value, RepositoryError> {
    serde_json::to_value(document).map_err(refused)
}

fn time(text: &str) -> Result<DateTime<Utc>, RepositoryError> {
    DateTime::parse_from_rfc3339(text)
        .map(|date| date.with_timezone(&Utc))
        .map_err(refused)
}

fn public_key(text: &str) -> Result<PublicKey, RepositoryError> {
    let bytes: [u8; 32] = data_encoding::BASE32_NOPAD
        .decode(text.as_bytes())
        .map_err(refused)?
        .try_into()
        .map_err(|_| refused("invalid public key length"))?;
    let key = PublicKey::from_bytes(&bytes).map_err(refused)?;
    if key.to_base32() != text {
        return Err(refused("noncanonical public key"));
    }
    Ok(key)
}

fn issuer_is_trusted(config: &PublicationGateConfig, issuer: &str) -> bool {
    // Use only the configured raw trust roots, never broad identity grants or
    // recursive delegation. Explicit revocation wins for either root form.
    config
        .trust
        .evaluate(issuer, config.repository_identity.as_deref())
        == SignerTrust::Trusted
}

fn path(id: &str) -> String {
    format!("{PREFIX}{id}.json")
}
fn revocation_path(id: &str) -> String {
    format!("{PREFIX}revocations/{id}.json")
}

fn identifier(authorization: &ReviewAuthorization) -> Result<String, RepositoryError> {
    let mut document = value(authorization)?;
    let object = document
        .as_object_mut()
        .ok_or_else(|| refused("expected object"))?;
    for field in ["id", "proof", "contentHash"] {
        object.remove(field);
    }
    let mut hash = blake3::Hasher::new();
    hash.update(b"atomic.review-authorization.id.v1\0");
    hash.update(atomic_canonical::jcs::canonicalize(&document).as_bytes());
    Ok(hash.finalize().to_hex().to_string())
}

fn validate_subject(repo: &Repository, subject: &ReviewSubject) -> Result<(), RepositoryError> {
    if subject.project != repo.project_id()?.to_urn() {
        return Err(refused("project mismatch"));
    }
    // Canonical roots and closure are an unambiguous exact list, not a pattern.
    for hashes in [
        &subject.roots,
        &subject
            .changes
            .iter()
            .map(|pin| pin.hash.clone())
            .collect::<Vec<_>>(),
    ] {
        if hashes.is_empty() || hashes.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(refused("scope hashes must be nonempty, sorted and unique"));
        }
        for text in hashes {
            if !Hash::from_base32(text.as_bytes()).is_some_and(|hash| hash.to_base32() == *text) {
                return Err(refused("scope requires full canonical change hashes"));
            }
        }
    }
    if subject.view_chain.is_empty() || subject.effective_set_id.is_empty() {
        return Err(refused("missing exact source perspective"));
    }
    Ok(())
}

impl ReviewAuthorization {
    fn verify(
        &self,
        repo: &Repository,
        config: &PublicationGateConfig,
    ) -> Result<PublicKey, RepositoryError> {
        if self.schema != SCHEMA
            || self.purpose != PURPOSE
            || self.id != identifier(self)?
            || self.project != self.subject.project
            || !issuer_is_trusted(config, &self.issuer)
        {
            return Err(refused("unsupported, changed, or untrusted authorization"));
        }
        validate_subject(repo, &self.subject)?;
        let issuer = public_key(&self.issuer_public_key)?;
        let delegate = public_key(&self.delegate_public_key)?;
        if !atomic_canonical::did::did_matches_public_key(&self.issuer, &issuer)
            || !atomic_canonical::did::did_matches_public_key(&self.delegate, &delegate)
        {
            return Err(refused("identity/key mismatch"));
        }
        atomic_canonical::proof::verify_value(&value(self)?, &issuer).map_err(refused)?;
        let created = time(&self.created_at)?;
        if created > Utc::now() || time(&self.expires_at)? <= created {
            return Err(refused("invalid authorization validity window"));
        }
        Ok(delegate)
    }

    fn covers(&self, review: &PublicationReview) -> bool {
        self.delegate == review.attributed_to && self.subject == review.subject
    }
}

/// Mint and durably retain a capability using an explicitly supplied issuer.
/// No private key is serialized. The issuer must be trusted by the raw configured
/// signer policy: `trust.signers` or `repository_identity`, with revocation winning.
#[allow(clippy::too_many_arguments)]
pub fn mint_and_store(
    repo: &Repository,
    subject: &ReviewSubject,
    delegate: &PublicKey,
    issuer: &Identity,
    issuer_keypair: &KeyPair,
    expires_at: DateTime<Utc>,
    config: &PublicationGateConfig,
) -> Result<ReviewAuthorization, RepositoryError> {
    if issuer.public_key != issuer_keypair.secret.public_key() {
        return Err(refused("issuer keypair mismatch"));
    }
    let now = Utc::now();
    if expires_at <= now {
        return Err(refused("authorization already expired"));
    }
    let mut authorization = ReviewAuthorization {
        schema: SCHEMA.into(),
        id: String::new(),
        purpose: PURPOSE.into(),
        project: repo.project_id()?.to_urn(),
        delegate: atomic_canonical::did::did_for_public_key(delegate),
        delegate_public_key: delegate.to_base32(),
        subject: subject.clone(),
        created_at: now.to_rfc3339(),
        expires_at: expires_at.to_rfc3339(),
        issuer: atomic_canonical::did::did_for_public_key(&issuer.public_key),
        issuer_public_key: issuer.public_key.to_base32(),
        content_hash: None,
        proof: None,
    };
    authorization.id = identifier(&authorization)?;
    authorization = serde_json::from_value(atomic_canonical::proof::attest_value(
        value(&authorization)?,
        issuer,
        issuer_keypair,
    ))
    .map_err(refused)?;
    authorization.verify(repo, config)?;
    store_immutable(repo, &path(&authorization.id), &authorization)?;
    Ok(authorization)
}

/// Store immutable canonical bytes and update the vault manifest in the same
/// transaction. Content-address verification also detects replacement through
/// generic vault APIs. No indexing or inference confers authority on this entry.
fn store_immutable<T: Serialize>(
    repo: &Repository,
    path: &str,
    document: &T,
) -> Result<(), RepositoryError> {
    let _boundary = repo.review_evidence_lock()?;
    let bytes = atomic_canonical::jcs::canonicalize(&value(document)?).into_bytes();
    if bytes.len() > MAX_BYTES {
        return Err(refused("authorization exceeds size limit"));
    }
    let now = Utc::now().to_rfc3339();
    let entry = VaultEntry::new(VaultEntryType::Attestation, bytes, "{}".into(), now.clone());
    let mut txn = repo.pristine.write_txn().map_err(refused)?;
    if let Some(old) = txn.get_vault_entry(path).map_err(refused)? {
        if old.content_bytes != entry.content_bytes || old.entry_type != entry.entry_type {
            return Err(refused("authorization entries are immutable"));
        }
        return Ok(());
    }
    txn.put_vault_entry(path, &entry).map_err(refused)?;
    let mut manifest = txn.get_vault_manifest().map_err(refused)?;
    manifest.file_count = manifest.file_count.saturating_add(1);
    manifest.total_bytes = manifest
        .total_bytes
        .saturating_add(entry.content_size() as u64);
    let previous = Merkle::from_base32(manifest.merkle.as_bytes()).unwrap_or(Merkle::ZERO);
    manifest.merkle = previous
        .next(&Hash::from_bytes(entry.content_hash))
        .to_base32();
    manifest.updated_at = now;
    txn.put_vault_manifest(&manifest).map_err(refused)?;
    txn.commit().map_err(refused)
}

fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, RepositoryError> {
    if bytes.len() > MAX_BYTES {
        return Err(refused("authorization exceeds size limit"));
    }
    serde_json::from_value(atomic_canonical::jcs::admit_document(bytes).map_err(refused)?)
        .map_err(refused)
}

fn authorizations(repo: &Repository) -> Result<Vec<ReviewAuthorization>, RepositoryError> {
    let mut result = Vec::new();
    for meta in repo.vault_list(PREFIX, None)? {
        if meta.path.starts_with(&format!("{PREFIX}revocations/")) {
            continue;
        }
        let entry = repo
            .vault_retrieve(&meta.path)?
            .ok_or_else(|| refused("authorization disappeared"))?;
        let authorization: ReviewAuthorization = parse(&entry.content_bytes)?;
        if meta.path != path(&authorization.id) {
            return Err(refused("authorization path/ID mismatch"));
        }
        result.push(authorization);
    }
    Ok(result)
}

fn is_revoked(
    repo: &Repository,
    authorization: &ReviewAuthorization,
) -> Result<bool, RepositoryError> {
    let Some(entry) = repo.vault_retrieve(&revocation_path(&authorization.id))? else {
        return Ok(false);
    };
    let revocation: ReviewAuthorizationRevocation = parse(&entry.content_bytes)?;
    if revocation.schema != REVOCATION_SCHEMA
        || revocation.purpose != PURPOSE
        || revocation.authorization != authorization.id
        || revocation.project != authorization.project
        || revocation.issuer != authorization.issuer
        || time(&revocation.revoked_at)? > Utc::now()
        || time(&revocation.revoked_at)? < time(&authorization.created_at)?
    {
        return Err(refused("invalid revocation"));
    }
    atomic_canonical::proof::verify_value(
        &value(&revocation)?,
        &public_key(&authorization.issuer_public_key)?,
    )
    .map_err(refused)?;
    Ok(true)
}

/// Explicitly invalidate a capability, without deleting retained verification
/// keys. Only its original, still trusted issuer may revoke it here. This is a
/// deliberate approval withdrawal, NOT credential retirement: successful
/// publication must retain its certificate when retiring the ephemeral secret
/// or revoking a separate recording delegation.
pub fn revoke_and_store(
    repo: &Repository,
    authorization: &ReviewAuthorization,
    issuer: &Identity,
    issuer_keypair: &KeyPair,
    config: &PublicationGateConfig,
) -> Result<(), RepositoryError> {
    authorization.verify(repo, config)?;
    if issuer.public_key != issuer_keypair.secret.public_key()
        || atomic_canonical::did::did_for_public_key(&issuer.public_key) != authorization.issuer
    {
        return Err(refused("revocation issuer mismatch"));
    }
    if is_revoked(repo, authorization)? {
        return Ok(());
    }
    let revocation = ReviewAuthorizationRevocation {
        schema: REVOCATION_SCHEMA.into(),
        purpose: PURPOSE.into(),
        authorization: authorization.id.clone(),
        project: authorization.project.clone(),
        revoked_at: Utc::now().to_rfc3339(),
        issuer: authorization.issuer.clone(),
        content_hash: None,
        proof: None,
    };
    let signed = atomic_canonical::proof::attest_value(value(&revocation)?, issuer, issuer_keypair);
    store_immutable(repo, &revocation_path(&authorization.id), &signed)
}

/// Resolve only a public key authenticated by a currently raw-policy-trusted
/// issuer and matching this exact subject/delegate. This is key resolution, not
/// authority: expiry/revocation are checked separately, so deleting an ephemeral
/// secret never destroys historical signature verification. Compose with an
/// identity-store provider; never infer trust from the returned key alone.
pub fn retained_public_key(
    repo: &Repository,
    review: &PublicationReview,
    config: &PublicationGateConfig,
) -> Result<Option<[u8; 32]>, RepositoryError> {
    for authorization in authorizations(repo)? {
        if authorization.covers(review) {
            if let Ok(key) = authorization.verify(repo, config) {
                return Ok(Some(*key.as_bytes()));
            }
        }
    }
    Ok(None)
}

/// Check authority for a retained signed review at its signed `reviewed_at`.
/// Unsigned previews are checked *now* as well. Explicit signer revocation has
/// unconditional deny precedence, including over direct trust and certificates.
pub fn reviewer_is_authorized(
    repo: &Repository,
    review: &PublicationReview,
    config: &PublicationGateConfig,
    keys: DidKeyProvider<'_>,
) -> Result<bool, RepositoryError> {
    match config
        .trust
        .evaluate(&review.attributed_to, config.repository_identity.as_deref())
    {
        SignerTrust::Revoked => return Ok(false),
        SignerTrust::Trusted => return Ok(true),
        SignerTrust::Unknown => {}
    }
    delegated_authority(repo, review, config, keys, review.proof.is_none())
}

/// Mandatory pre-signing check: unlike historical verification, expiry is
/// evaluated at wall-clock time even if a caller supplies a backdated review.
pub fn reviewer_may_sign_now(
    repo: &Repository,
    review: &PublicationReview,
    config: &PublicationGateConfig,
    keys: DidKeyProvider<'_>,
) -> Result<bool, RepositoryError> {
    match config
        .trust
        .evaluate(&review.attributed_to, config.repository_identity.as_deref())
    {
        SignerTrust::Revoked => return Ok(false),
        SignerTrust::Trusted => return Ok(true),
        SignerTrust::Unknown => {}
    }
    delegated_authority(repo, review, config, keys, true)
}

fn delegated_authority(
    repo: &Repository,
    review: &PublicationReview,
    config: &PublicationGateConfig,
    keys: DidKeyProvider<'_>,
    signing_now: bool,
) -> Result<bool, RepositoryError> {
    let reviewed = time(&review.reviewed_at)?;
    let now = Utc::now();
    if reviewed > now {
        return Ok(false);
    }
    for authorization in authorizations(repo)? {
        if !authorization.covers(review) {
            continue;
        }
        let Ok(key) = authorization.verify(repo, config) else {
            continue;
        };
        let created = time(&authorization.created_at)?;
        let expires = time(&authorization.expires_at)?;
        if reviewed < created
            || reviewed >= expires
            || (signing_now && (now < created || now >= expires))
            || is_revoked(repo, &authorization)?
        {
            continue;
        }
        if keys(&review.attributed_to).is_some_and(|bytes| bytes != *key.as_bytes()) {
            return Ok(false);
        }
        if review.proof.is_some() {
            if atomic_canonical::proof::verify_value(&review.value(), &key).is_err() {
                return Ok(false);
            }
        } else if !signing_now {
            return Ok(false);
        }
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::publication_review::{ChangePin, IntentPin, ReviewViewPin};
    use chrono::Duration;

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: Repository,
        issuer: Identity,
        issuer_key: KeyPair,
        delegate: Identity,
        delegate_key: KeyPair,
        config: PublicationGateConfig,
        review: PublicationReview,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init(dir.path()).unwrap();
            repo.ensure_project_identity(atomic_core::kg::project_id::MintReason::Upgrade)
                .unwrap();
            let issuer_key = KeyPair::generate();
            let issuer = Identity::new("test-issuer", &issuer_key);
            let delegate_key = KeyPair::generate();
            let delegate = Identity::new("temporary-reviewer", &delegate_key);
            let issuer_did = atomic_canonical::did::did_for_public_key(&issuer.public_key);
            let root = Hash::of(b"exact-root").to_base32();
            let pin = IntentPin {
                id: "urn:atomic:intent:test-only".into(),
                source_hash: "source".into(),
                signed_hash: "signed".into(),
                public_key: delegate.public_key.to_base32(),
            };
            let review = PublicationReview {
                schema: atomic_canonical::publication_review::LEGACY_SCHEMA.into(),
                reviewed_at: Utc::now().to_rfc3339(),
                subject: ReviewSubject {
                    project: repo.project_id().unwrap().to_urn(),
                    roots: vec![root.clone()],
                    changes: vec![ChangePin {
                        hash: root,
                        session: None,
                        governing_intents: vec![],
                        provenance: vec![],
                        observations: vec![],
                    }],
                    review: pin,
                    work: vec![],
                    set_aside: vec![],
                    profile_hash: "policy-digest".into(),
                    view_chain: vec![ReviewViewPin {
                        name: "historical-source".into(),
                        state: "state-digest".into(),
                        scope: "draft".into(),
                    }],
                    effective_set_id: "membership-digest".into(),
                    snapshot: None,
                },
                supersedes: vec![],
                attributed_to: atomic_canonical::did::did_for_public_key(&delegate.public_key),
                content_hash: None,
                proof: None,
            };
            Self {
                _dir: dir,
                repo,
                issuer,
                issuer_key,
                delegate,
                delegate_key,
                config: PublicationGateConfig {
                    trust: atomic_config::GitTrustConfig {
                        signers: vec![issuer_did],
                        revoked: vec![],
                    },
                    repository_identity: None,
                },
                review,
            }
        }

        fn mint(&mut self) -> ReviewAuthorization {
            let authorization = mint_and_store(
                &self.repo,
                &self.review.subject,
                &self.delegate.public_key,
                &self.issuer,
                &self.issuer_key,
                Utc::now() + Duration::minutes(10),
                &self.config,
            )
            .unwrap();
            self.review.reviewed_at = Utc::now().to_rfc3339();
            authorization
        }

        fn sign_review(&mut self) {
            self.review = serde_json::from_value(atomic_canonical::proof::attest_value(
                self.review.value(),
                &self.delegate,
                &self.delegate_key,
            ))
            .unwrap();
        }

        fn authorized(&self) -> bool {
            reviewer_is_authorized(&self.repo, &self.review, &self.config, &|_| None).unwrap()
        }

        fn store_reissued(&self, mut authorization: ReviewAuthorization) -> ReviewAuthorization {
            authorization.proof = None;
            authorization.content_hash = None;
            authorization.id = identifier(&authorization).unwrap();
            let authorization: ReviewAuthorization =
                serde_json::from_value(atomic_canonical::proof::attest_value(
                    value(&authorization).unwrap(),
                    &self.issuer,
                    &self.issuer_key,
                ))
                .unwrap();
            store_immutable(&self.repo, &path(&authorization.id), &authorization).unwrap();
            authorization
        }
    }

    #[test]
    fn exact_authorization_and_retained_public_key_work_without_identity_store() {
        let mut f = Fixture::new();
        assert!(!f.authorized());
        let auth = f.mint();
        assert!(f.authorized());
        assert!(reviewer_may_sign_now(&f.repo, &f.review, &f.config, &|_| None).unwrap());
        f.sign_review();
        assert!(f.authorized());
        assert_eq!(
            retained_public_key(&f.repo, &f.review, &f.config).unwrap(),
            Some(*f.delegate.public_key.as_bytes())
        );
        let stored = f.repo.vault_retrieve(&path(&auth.id)).unwrap().unwrap();
        assert_eq!(
            stored.content_bytes,
            atomic_canonical::jcs::canonicalize(&value(&auth).unwrap()).as_bytes()
        );
        assert!(stored.frontmatter_json == "{}");
        // Key resolution is authenticated by issuer bytes, not arbitrary local keys.
        let other = KeyPair::generate();
        assert!(
            !reviewer_is_authorized(&f.repo, &f.review, &f.config, &|_| Some(
                *other.public.as_bytes()
            ))
            .unwrap()
        );
    }

    #[test]
    fn scope_never_widens_roots_closure_views_intent_pins_or_project() {
        let mut f = Fixture::new();
        f.mint();
        let original = f.review.clone();
        let mut changed = vec![];
        let mut r = original.clone();
        r.subject.roots = vec![Hash::of(b"another-root").to_base32()];
        changed.push(r);
        let mut r = original.clone();
        r.subject.changes[0].hash = Hash::of(b"another-dependency").to_base32();
        changed.push(r);
        let mut r = original.clone();
        r.subject.view_chain[0].name = "other-view".into();
        changed.push(r);
        let mut r = original.clone();
        r.subject.view_chain[0].state = "different-state".into();
        changed.push(r);
        let mut r = original.clone();
        r.subject.effective_set_id = "changed-membership".into();
        changed.push(r);
        let mut r = original.clone();
        r.subject.review.signed_hash = "changed-revision".into();
        changed.push(r);
        let mut r = original.clone();
        r.subject.profile_hash = "changed-policy".into();
        changed.push(r);
        let mut r = original.clone();
        r.subject.project = "urn:atomic:project:other".into();
        changed.push(r);
        for r in changed {
            assert!(!reviewer_is_authorized(&f.repo, &r, &f.config, &|_| None).unwrap());
            assert!(retained_public_key(&f.repo, &r, &f.config)
                .unwrap()
                .is_none());
        }
        let other = Fixture::new();
        let auth = f.repo.vault_list(PREFIX, None).unwrap().pop().unwrap();
        let entry = f.repo.vault_retrieve(&auth.path).unwrap().unwrap();
        let certificate: ReviewAuthorization = parse(&entry.content_bytes).unwrap();
        store_immutable(&other.repo, &auth.path, &certificate).unwrap();
        assert!(!reviewer_is_authorized(&other.repo, &original, &f.config, &|_| None).unwrap());
    }

    #[test]
    fn issuer_requires_current_configured_trust_not_arbitrary_local_keys() {
        let mut f = Fixture::new();
        let mut untrusted = f.config.clone();
        untrusted.trust.signers.clear();
        assert!(mint_and_store(
            &f.repo,
            &f.review.subject,
            &f.delegate.public_key,
            &f.issuer,
            &f.issuer_key,
            Utc::now() + Duration::minutes(10),
            &untrusted
        )
        .is_err());
        f.mint();
        assert!(!reviewer_is_authorized(&f.repo, &f.review, &untrusted, &|_| None).unwrap());
        f.config
            .trust
            .revoked
            .push(f.config.trust.signers[0].clone());
        assert!(!f.authorized());
    }

    #[test]
    fn configured_repository_identity_can_issue_but_revocation_still_wins() {
        let mut f = Fixture::new();
        f.config.repository_identity = f.config.trust.signers.pop();
        let auth = f.mint();
        f.sign_review();
        assert!(f.authorized());
        assert!(reviewer_may_sign_now(&f.repo, &f.review, &f.config, &|_| None).unwrap());
        assert!(retained_public_key(&f.repo, &f.review, &f.config)
            .unwrap()
            .is_some());
        f.config.trust.revoked.push(auth.issuer);
        assert!(!f.authorized());
        assert!(!reviewer_may_sign_now(&f.repo, &f.review, &f.config, &|_| None).unwrap());
    }

    #[test]
    fn revoked_delegate_overrides_both_certificate_and_direct_trust() {
        let mut f = Fixture::new();
        f.mint();
        f.sign_review();
        f.config.trust.revoked.push(f.review.attributed_to.clone());
        assert!(!f.authorized());
        f.config.trust.signers.push(f.review.attributed_to.clone());
        f.config.repository_identity = Some(f.review.attributed_to.clone());
        assert!(!f.authorized());
        assert!(!reviewer_may_sign_now(&f.repo, &f.review, &f.config, &|_| None).unwrap());
    }

    #[test]
    fn direct_signer_trust_is_preserved_without_any_certificate() {
        let mut f = Fixture::new();
        f.config.repository_identity = Some(f.review.attributed_to.clone());
        assert!(f.authorized());
    }

    #[test]
    fn expiry_blocks_new_signing_not_authenticated_historical_reviews() {
        let mut f = Fixture::new();
        let mut auth = f.mint();
        // Remove the still-live certificate using a fresh repository rather than
        // deleting authority. Only the intentionally expired fixture is retained.
        let other = Fixture::new();
        auth.project = other.repo.project_id().unwrap().to_urn();
        auth.subject.project = auth.project.clone();
        auth.created_at = (Utc::now() - Duration::hours(2)).to_rfc3339();
        auth.expires_at = (Utc::now() - Duration::hours(1)).to_rfc3339();
        auth.content_hash = None;
        auth.proof = None;
        auth.id = identifier(&auth).unwrap();
        let signed: ReviewAuthorization = serde_json::from_value(
            atomic_canonical::proof::attest_value(value(&auth).unwrap(), &f.issuer, &f.issuer_key),
        )
        .unwrap();
        store_immutable(&other.repo, &path(&signed.id), &signed).unwrap();
        f.review.subject = auth.subject;
        f.review.reviewed_at = (Utc::now() - Duration::minutes(90)).to_rfc3339();
        f.sign_review();
        assert!(reviewer_is_authorized(&other.repo, &f.review, &f.config, &|_| None).unwrap());
        assert!(!reviewer_may_sign_now(&other.repo, &f.review, &f.config, &|_| None).unwrap());
        assert!(retained_public_key(&other.repo, &f.review, &f.config)
            .unwrap()
            .is_some());
        f.review.proof = None;
        assert!(!reviewer_is_authorized(&other.repo, &f.review, &f.config, &|_| None).unwrap());
    }

    #[test]
    fn backdating_future_dates_and_tampered_reviews_are_denied() {
        let mut f = Fixture::new();
        let auth = f.mint();
        f.review.reviewed_at =
            (time(&auth.created_at).unwrap() - Duration::seconds(1)).to_rfc3339();
        f.sign_review();
        assert!(!f.authorized());
        f.review.reviewed_at = (Utc::now() + Duration::minutes(1)).to_rfc3339();
        f.sign_review();
        assert!(!f.authorized());
        f.review.reviewed_at = Utc::now().to_rfc3339();
        f.sign_review();
        assert!(f.authorized());
        f.review.reviewed_at = (Utc::now() - Duration::milliseconds(1)).to_rfc3339();
        assert!(!f.authorized());
    }

    #[test]
    fn malformed_scope_wrong_purpose_and_mismatched_issuer_keys_fail_closed() {
        let mut f = Fixture::new();
        let auth = f.mint();
        let mut bad = auth.clone();
        bad.purpose = "read-record".into();
        let bad = f.store_reissued(bad);
        assert!(bad.verify(&f.repo, &f.config).is_err());
        let mut bad = auth.clone();
        bad.delegate_public_key = f.issuer.public_key.to_base32();
        let bad = f.store_reissued(bad);
        assert!(bad.verify(&f.repo, &f.config).is_err());
        let mut bad = auth.clone();
        bad.issuer_public_key = f.delegate.public_key.to_base32();
        let bad = f.store_reissued(bad);
        assert!(bad.verify(&f.repo, &f.config).is_err());
        let mut bad = auth.clone();
        bad.subject.roots.push(bad.subject.roots[0].clone());
        let bad = f.store_reissued(bad);
        assert!(bad.verify(&f.repo, &f.config).is_err());
        let mut bad = auth.clone();
        bad.subject.roots[0] = "*".into();
        let bad = f.store_reissued(bad);
        assert!(bad.verify(&f.repo, &f.config).is_err());
        let mut bad = auth.clone();
        bad.expires_at = (Utc::now() + Duration::hours(3)).to_rfc3339();
        // Recomputing an ID is not a replacement for an issuer signature.
        bad.id = identifier(&bad).unwrap();
        assert!(bad.verify(&f.repo, &f.config).is_err());
        assert!(mint_and_store(
            &f.repo,
            &f.review.subject,
            &f.delegate.public_key,
            &f.issuer,
            &f.delegate_key,
            Utc::now() + Duration::minutes(10),
            &f.config
        )
        .is_err());
    }

    #[test]
    fn signed_revocation_blocks_authority_but_preserves_public_verification_key() {
        let mut f = Fixture::new();
        let auth = f.mint();
        f.sign_review();
        assert!(f.authorized());
        assert!(revoke_and_store(&f.repo, &auth, &f.delegate, &f.delegate_key, &f.config).is_err());
        revoke_and_store(&f.repo, &auth, &f.issuer, &f.issuer_key, &f.config).unwrap();
        revoke_and_store(&f.repo, &auth, &f.issuer, &f.issuer_key, &f.config).unwrap();
        assert!(!f.authorized());
        assert!(!reviewer_may_sign_now(&f.repo, &f.review, &f.config, &|_| None).unwrap());
        assert_eq!(
            retained_public_key(&f.repo, &f.review, &f.config).unwrap(),
            Some(*f.delegate.public_key.as_bytes())
        );
    }

    #[test]
    fn ordinary_secret_retirement_keeps_signed_approval_authority() {
        let mut f = Fixture::new();
        let auth = f.mint();
        f.sign_review();
        // Retire the actual signing secret, retaining only the public identity
        // and signed certificate. No approval-withdrawal operation is implied.
        drop(f.delegate_key);
        assert!(reviewer_is_authorized(&f.repo, &f.review, &f.config, &|_| None).unwrap());
        assert_eq!(
            retained_public_key(&f.repo, &f.review, &f.config).unwrap(),
            Some(*f.delegate.public_key.as_bytes())
        );
        assert!(f
            .repo
            .vault_retrieve(&revocation_path(&auth.id))
            .unwrap()
            .is_none());
    }

    #[test]
    fn immutable_storage_and_untrusted_revocation_assertions() {
        let mut f = Fixture::new();
        let auth = f.mint();
        store_immutable(&f.repo, &path(&auth.id), &auth).unwrap();
        let mut changed = auth.clone();
        changed.purpose = "*".into();
        assert!(store_immutable(&f.repo, &path(&auth.id), &changed).is_err());
        store_immutable(
            &f.repo,
            &revocation_path(&auth.id),
            &serde_json::json!({"revoked":true}),
        )
        .unwrap();
        assert!(reviewer_is_authorized(&f.repo, &f.review, &f.config, &|_| None).is_err());
        assert!(parse::<ReviewAuthorization>(br#"{"id":"a","id":"b"}"#).is_err());
    }

    #[test]
    fn generic_vault_mutations_cannot_erase_or_rewrite_approval_withdrawal() {
        let mut f = Fixture::new();
        let auth = f.mint();
        f.sign_review();
        revoke_and_store(&f.repo, &auth, &f.issuer, &f.issuer_key, &f.config).unwrap();
        assert!(!f.authorized());
        let manifest_before = serde_json::to_value(f.repo.vault_manifest().unwrap()).unwrap();
        for retained_path in [path(&auth.id), revocation_path(&auth.id)] {
            let original = f.repo.vault_retrieve(&retained_path).unwrap().unwrap();
            // Content, metadata, and entry-type replacement are all forbidden
            // through both generic store APIs, not just the mint/revoke helpers.
            for (content, frontmatter, entry_type) in [
                (
                    b"{\"revoked\":false}".to_vec(),
                    original.frontmatter_json.clone(),
                    original.entry_type,
                ),
                (
                    original.content_bytes.clone(),
                    "{\"revoked\":false}".into(),
                    original.entry_type,
                ),
                (
                    original.content_bytes.clone(),
                    original.frontmatter_json.clone(),
                    VaultEntryType::Scratch,
                ),
            ] {
                assert!(f
                    .repo
                    .vault_store(
                        &retained_path,
                        entry_type,
                        content.clone(),
                        frontmatter.clone()
                    )
                    .is_err());
                let replacement =
                    VaultEntry::new(entry_type, content, frontmatter, Utc::now().to_rfc3339());
                assert!(f
                    .repo
                    .vault_store_entry(&retained_path, &replacement)
                    .is_err());
                assert!(!f.authorized());
            }
            assert!(f.repo.vault_delete(&retained_path).is_err());
            // Explicit-file deflation routes edits/deletes through the same
            // protections and must not be an alternative mutation channel.
            assert!(f
                .repo
                .vault_record_files(&[(retained_path.clone(), Some("tampered".into()))])
                .is_err());
            assert!(f
                .repo
                .vault_record_files(&[(retained_path.clone(), None)])
                .is_err());
            let after = f.repo.vault_retrieve(&retained_path).unwrap().unwrap();
            assert_eq!(after.content_bytes, original.content_bytes);
            assert_eq!(after.frontmatter_json, original.frontmatter_json);
            assert_eq!(after.entry_type, original.entry_type);
            assert!(!f.authorized());
        }
        assert_eq!(
            serde_json::to_value(f.repo.vault_manifest().unwrap()).unwrap(),
            manifest_before
        );

        // The ordinary on-disk deflation path cannot delete a withdrawal either.
        f.repo.vault_materialize_all().unwrap();
        std::fs::remove_file(f.repo.vault_dir().join(revocation_path(&auth.id))).unwrap();
        assert!(f.repo.vault_record_working_copy().is_err());
        assert!(is_revoked(&f.repo, &auth).unwrap());
        assert!(!f.authorized());
        assert!(retained_public_key(&f.repo, &f.review, &f.config)
            .unwrap()
            .is_some());
    }
}
