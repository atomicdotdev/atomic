//! Exact-scope retrospective publication reviews. Canonical objects are the
//! authority; verifier-owned graph observations and SHACL are rebuilt on use.
#[cfg(feature = "review-shacl")]
mod artifact;
#[cfg(feature = "review-shacl")]
mod graph;
mod observations;
pub(crate) mod references;
mod snapshot;
mod subject;
#[cfg(all(test, feature = "review-shacl"))]
mod tests;

use super::provenance_gate::{DidKeyProvider, PublicationGateConfig};
use super::{Repository, RepositoryError};
use atomic_canonical::publication_review::{PublicationReview, SCHEMA};
use atomic_core::types::{Base32, Hash};

pub const PREFIX: &str = "publication-reviews/";

pub(super) fn refused(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!("publication review: {error}"),
    }
}

/// Pin the actual executable shapes and ontology, not a caller-chosen version label.
pub fn profile_hash() -> String {
    let mut hash = blake3::Hasher::new();
    for bytes in [
        b"atomic.review-policy.v3\0".as_slice(),
        atomic_core::kg::ontology::TTL.as_bytes(),
        include_bytes!("../../../../atomic-core/ontology/shapes/review-1.0.0.ttl").as_slice(),
    ] {
        hash.update(&(bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    #[cfg(feature = "review-shacl")]
    hash.update(atomic_shacl::ENGINE_VERSION.as_bytes());
    #[cfg(not(feature = "review-shacl"))]
    hash.update(b"unavailable-validator");
    hash.finalize().to_hex().to_string()
}

impl Repository {
    /// Writers affecting review evidence participate in the common operation
    /// boundary once retrospective artifacts exist. No feature means no bypass.
    pub(super) fn review_evidence_lock(
        &self,
    ) -> Result<Option<super::locks::RepositoryCommonLockGuard>, RepositoryError> {
        if self.vault_list(PREFIX, None)?.is_empty() {
            return Ok(None);
        }
        self.review_evidence_common_lock().map(Some)
    }
    /// Prepare a present-day review from fresh signed intents and exact original
    /// changes. This does not grant trust, sign, store, or publish anything.
    pub fn prepare_publication_review(
        &self,
        review_id: &str,
        roots: &[Hash],
        keys: DidKeyProvider<'_>,
    ) -> Result<PublicationReview, RepositoryError> {
        self.prepare_publication_review_on_view(review_id, roots, self.current_view(), keys)
    }

    pub fn prepare_publication_review_on_view(
        &self,
        review_id: &str,
        roots: &[Hash],
        view: &str,
        keys: DidKeyProvider<'_>,
    ) -> Result<PublicationReview, RepositoryError> {
        let subject = subject::build(self, review_id, roots, view, keys)?;
        let (node, _) = subject::intent(self, review_id, keys)?;
        let review = PublicationReview {
            schema: SCHEMA.into(),
            reviewed_at: chrono::Utc::now().to_rfc3339(),
            subject,
            supersedes: Vec::new(),
            attributed_to: node
                .attributed_to
                .ok_or_else(|| refused("missing reviewer"))?,
            content_hash: None,
            proof: None,
        };
        PublicationReview::parse(&serde_json::to_vec(&review).map_err(refused)?)
            .map_err(refused)?;
        Ok(review)
    }

    /// Rebuild authenticated observations and evaluate the complete pinned review
    /// dataset. No supplied RDF or `Verified` assertion is an authorization input.
    #[cfg(feature = "review-shacl")]
    pub fn validate_publication_review(
        &self,
        review: &PublicationReview,
        config: &PublicationGateConfig,
        keys: DidKeyProvider<'_>,
    ) -> Result<atomic_shacl::ValidationOutput, RepositoryError> {
        let snapshot = self.publication_review_snapshot(review, config, keys)?;
        let data =
            atomic_shacl::snapshot_ntriples(&snapshot.nodes, &snapshot.edges).map_err(refused)?;
        atomic_shacl::validate_ntriples(
            &data,
            &review.artifact_hash(),
            &atomic_shacl::ValidationRequest {
                profile: atomic_shacl::Profile::Review,
                ..Default::default()
            },
        )
        .map_err(refused)
    }

    /// A rebuildable, review-subject-scoped KG projection over canonical signed
    /// originals. It is not a new project-wide knowledge base or an authority
    /// store. Signature, source and policy observations are recomputed each time.
    #[cfg(feature = "review-shacl")]
    pub fn publication_review_snapshot(
        &self,
        review: &PublicationReview,
        config: &PublicationGateConfig,
        keys: DidKeyProvider<'_>,
    ) -> Result<super::WorkflowSnapshot, RepositoryError> {
        self.review_snapshot(review, config, keys, false)
    }

    /// Inspect unsigned evidence with the same resolver and executable policy.
    /// Never used by the publication gate; absent proof stays a failing check.
    #[cfg(feature = "review-shacl")]
    pub fn preview_publication_review(
        &self,
        review: &PublicationReview,
        config: &PublicationGateConfig,
        keys: DidKeyProvider<'_>,
    ) -> Result<(super::WorkflowSnapshot, atomic_shacl::ValidationOutput), RepositoryError> {
        let mut snapshot = self.review_snapshot(review, config, keys, true)?;
        let data =
            atomic_shacl::snapshot_ntriples(&snapshot.nodes, &snapshot.edges).map_err(refused)?;
        let validation = atomic_shacl::validate_ntriples(
            &data,
            &review.artifact_hash(),
            &atomic_shacl::ValidationRequest {
                profile: atomic_shacl::Profile::Review,
                ..Default::default()
            },
        )
        .map_err(refused)?;
        // Put executable-policy diagnostics into the exported graph as well, so
        // DOT/HTML and JSON show the same blockers. This never feeds validation.
        for (i, result) in validation.results.iter().enumerate() {
            use atomic_core::kg::ontology::{Class, Predicate};
            let id = format!("urn:atomic:review:preview-finding:{i}");
            let message = result
                .get("result_messages")
                .and_then(|messages| messages.as_array())
                .and_then(|messages| messages.first())
                .and_then(|message| message.get("value"))
                .and_then(|value| value.as_str())
                .or_else(|| {
                    result
                        .get("source_constraint_component")
                        .and_then(|value| value.as_str())
                })
                .unwrap_or("Review policy violation");
            let mut node = atomic_core::KgNode::new(
                &id,
                Class::ReviewFinding,
                format!("SHACL: {message}"),
                "publication-review-preview",
            );
            node.metadata = Some(result.clone());
            snapshot.nodes.push(node);
            snapshot.edges.push(atomic_core::KgEdge::new(
                &id,
                "urn:atomic:review:evaluation",
                Predicate::AboutResource,
            ));
        }
        snapshot.identity =
            Hash::of(&serde_json::to_vec(&(&snapshot.nodes, &snapshot.edges)).map_err(refused)?)
                .to_base32();
        Ok((snapshot, validation))
    }

    /// Add an authenticated, bounded review preview to the existing workflow
    /// snapshot for intent-graph selection. Never a publication authorization.
    /// Unsupported consumers fail closed instead of displaying an unvalidated
    /// caller-supplied graph as verifier evidence.
    pub fn workflow_snapshot_with_review(
        &self,
        bytes: &[u8],
    ) -> Result<(super::WorkflowSnapshot, String), RepositoryError> {
        #[cfg(not(feature = "review-shacl"))]
        {
            let _ = bytes;
            Err(refused(
                "review-shacl feature is required for review preview",
            ))
        }
        #[cfg(feature = "review-shacl")]
        {
            let review = PublicationReview::parse(bytes).map_err(refused)?;
            let config = PublicationGateConfig::from_repo(self)?;
            let keys = super::provenance_gate::local_did_key_provider(self);
            let (mut preview, validation) =
                self.preview_publication_review(&review, &config, &keys)?;
            let context = review.artifact_hash();
            // Evaluation and report identities are request-local in validation,
            // but must be review-scoped when connected to the intent graph.
            let scoped = |id: &str| {
                if id == "urn:atomic:review:evaluation"
                    || id.starts_with("urn:atomic:review:preview-finding:")
                {
                    format!("{id}:{context}")
                } else {
                    id.to_owned()
                }
            };
            for node in &mut preview.nodes {
                node.id = scoped(&node.id);
                if node.has_type(atomic_core::kg::ontology::Class::ReviewEvaluation) {
                    node.label = format!(
                        "Review preview: {} ({} SHACL findings); not publication approval",
                        if validation.conforms {
                            "conforms"
                        } else {
                            "blocked"
                        },
                        validation.results.len()
                    );
                    node.metadata = Some(serde_json::json!({"preview":true,
                        "authorizes_publication":false,"validation":validation}));
                }
            }
            for edge in &mut preview.edges {
                edge.from_id = scoped(&edge.from_id);
                edge.to_id = scoped(&edge.to_id);
            }
            let mut snapshot = self.workflow_snapshot()?;
            references::extend(
                &mut snapshot.nodes,
                &mut snapshot.edges,
                (preview.nodes, preview.edges),
                &context,
            );
            snapshot.nodes.sort_by(|a, b| a.id.cmp(&b.id));
            snapshot.identity = Hash::of(
                &serde_json::to_vec(&(
                    snapshot.generation,
                    snapshot.ontology_version,
                    &snapshot.nodes,
                    &snapshot.edges,
                ))
                .map_err(refused)?,
            )
            .to_base32();
            Ok((snapshot, context))
        }
    }

    #[cfg(feature = "review-shacl")]
    fn review_snapshot(
        &self,
        review: &PublicationReview,
        config: &PublicationGateConfig,
        keys: DidKeyProvider<'_>,
        preview: bool,
    ) -> Result<super::WorkflowSnapshot, RepositoryError> {
        let (nodes, edges) = graph::build(self, review, config, keys, preview)?;
        let bytes = serde_json::to_vec(&(&nodes, &edges)).map_err(refused)?;
        Ok(super::WorkflowSnapshot {
            generation: atomic_core::kg::KG_GENERATION,
            ontology_version: atomic_core::kg::ontology::REVIEW_VERSION_IRI,
            identity: Hash::of(&bytes).to_base32(),
            nodes,
            edges,
        })
    }

    /// Persist a verified immutable artifact in the tracked vault. Existing
    /// entries at a content address must have identical bytes. No trust changes.
    #[cfg(feature = "review-shacl")]
    pub fn record_publication_review(
        &self,
        review: &PublicationReview,
        config: &PublicationGateConfig,
        keys: DidKeyProvider<'_>,
    ) -> Result<String, RepositoryError> {
        let _lock = self.review_evidence_common_lock()?;
        let report = self.validate_publication_review(review, config, keys)?;
        if !report.conforms {
            return Err(refused(format!(
                "SHACL refused review: {} violation(s): {}",
                report.results.len(),
                serde_json::to_string(&report.results.iter().take(4).collect::<Vec<_>>())
                    .map_err(refused)?
            )));
        }
        let existing = self.publication_reviews()?;
        self.review_supersession(review, &existing, keys)?;
        let path = format!("{PREFIX}{}/review.md", review.artifact_hash());
        let disk_path = artifact::destination(self, &review.artifact_hash())?;
        let bytes = format!(
            "{}\n",
            serde_json::to_string_pretty(review).map_err(refused)?
        )
        .into_bytes();
        if let Some(existing) = self.vault_retrieve(&path)? {
            if existing.content_bytes != bytes {
                return Err(refused("content-address collision"));
            }
        } else {
            if std::fs::symlink_metadata(&disk_path).is_ok() {
                return Err(refused(
                    "an unregistered working file occupies this review address",
                ));
            }
            self.vault_store(
                &path,
                atomic_core::pristine::VaultEntryType::Attestation,
                bytes,
                "{}".into(),
            )?;
        }
        artifact::write(self, &path, &disk_path)?;
        Ok(path)
    }

    /// Strictly inspect stored artifacts. Corruption cannot disappear behind a
    /// best-effort filter. Stale approvals are retained and refused, not erased.
    pub fn publication_reviews(&self) -> Result<Vec<PublicationReview>, RepositoryError> {
        let mut reviews = Vec::new();
        for entry in self.vault_list(PREFIX, None)? {
            if reviews.len() >= 4096 {
                return Err(refused("review inventory exceeds 4096 artifacts"));
            }
            let stored = self
                .vault_retrieve(&entry.path)?
                .ok_or_else(|| refused("registered review unavailable"))?;
            let review = PublicationReview::parse(&stored.content_bytes).map_err(refused)?;
            if entry.path != format!("{PREFIX}{}/review.md", review.artifact_hash()) {
                return Err(refused(
                    "stored artifact address does not match signed bytes",
                ));
            }
            reviews.push(review);
        }
        Ok(reviews)
    }

    pub(super) fn retrospective_review_for(
        &self,
        change: &str,
        session: &str,
        config: &PublicationGateConfig,
        keys: Option<DidKeyProvider<'_>>,
    ) -> Result<Option<String>, RepositoryError> {
        #[allow(unused_mut)]
        let mut accepted = None;
        let reviews = self.publication_reviews()?;
        let relevant: Vec<_> = reviews
            .iter()
            .filter(|review| {
                review
                    .subject
                    .changes
                    .iter()
                    .any(|pin| pin.hash == change && pin.session.as_deref() == Some(session))
            })
            .collect();
        #[cfg(feature = "review-shacl")]
        let mut superseded = std::collections::BTreeSet::new();
        #[cfg(feature = "review-shacl")]
        for review in &relevant {
            let keys = keys.ok_or_else(|| refused("review key resolver unavailable"))?;
            self.verify_review_signature(review, keys)?;
            // A renewal cannot suppress signature corruption, even in old rows.
            let ancestors = self.review_supersession(review, &reviews, keys)?;
            if !review.supersedes.is_empty() {
                let report = self.validate_publication_review(review, config, keys)?;
                if report.conforms {
                    superseded.extend(ancestors);
                }
            }
        }
        for review in relevant {
            if !review
                .subject
                .changes
                .iter()
                .any(|pin| pin.hash == change && pin.session.as_deref() == Some(session))
            {
                continue;
            }
            #[cfg(not(feature = "review-shacl"))]
            {
                let _ = (config, keys);
                return Err(refused(
                    "review-shacl feature is required to evaluate retrospective evidence",
                ));
            }
            #[cfg(feature = "review-shacl")]
            {
                if superseded.contains(&review.artifact_hash()) {
                    continue;
                }
                let report = self.validate_publication_review(
                    review,
                    config,
                    keys.ok_or_else(|| refused("review key resolver unavailable"))?,
                )?;
                if !report.conforms {
                    return Err(refused(format!(
                        "stored review fails SHACL ({} violations)",
                        report.results.len()
                    )));
                }
                accepted = Some(review.artifact_hash());
            }
        }
        Ok(accepted)
    }

    #[cfg(feature = "review-shacl")]
    fn verify_review_signature(
        &self,
        review: &PublicationReview,
        keys: DidKeyProvider<'_>,
    ) -> Result<(), RepositoryError> {
        let public =
            key(keys(&review.attributed_to)
                .ok_or_else(|| refused("review signing key unavailable"))?)?;
        if !atomic_canonical::did::did_matches_public_key(&review.attributed_to, &public) {
            return Err(refused("review DID is not key-bound"));
        }
        atomic_canonical::proof::verify_value(&review.value(), &public).map_err(refused)
    }

    #[cfg(feature = "review-shacl")]
    fn review_supersession(
        &self,
        review: &PublicationReview,
        all: &[PublicationReview],
        keys: DidKeyProvider<'_>,
    ) -> Result<std::collections::BTreeSet<String>, RepositoryError> {
        let mut visited = std::collections::BTreeSet::new();
        let mut pending = review.supersedes.clone();
        while let Some(hash) = pending.pop() {
            if hash == review.artifact_hash() {
                return Err(refused("cyclic review supersession"));
            }
            if !visited.insert(hash.clone()) {
                continue;
            }
            if visited.len() > 256 {
                return Err(refused("review supersession exceeds budget"));
            }
            let old = all
                .iter()
                .find(|old| old.artifact_hash() == hash)
                .ok_or_else(|| refused("superseded review is unavailable"))?;
            self.verify_review_signature(old, keys)?;
            let scope = |r: &PublicationReview| {
                (
                    r.subject.project.clone(),
                    r.subject.roots.clone(),
                    r.subject
                        .changes
                        .iter()
                        .map(|p| (p.hash.clone(), p.session.clone()))
                        .collect::<Vec<_>>(),
                )
            };
            if scope(old) != scope(review) || old.artifact_hash() == review.artifact_hash() {
                return Err(refused(
                    "supersession must retain the exact original project/change/session scope",
                ));
            }
            pending.extend(old.supersedes.iter().cloned());
        }
        Ok(visited)
    }

    /// Unlike the legacy best-effort discovery, this includes unindexed originals
    /// and refuses registered missing or corrupt originals before any exception.
    pub(super) fn review_original_attestations(
        &self,
        change: &Hash,
    ) -> Result<Vec<(Hash, atomic_core::change::Attestation)>, RepositoryError> {
        use atomic_core::pristine::{node_type, GraphTxnT};
        let mut hashes = std::collections::BTreeSet::new();
        let txn = self.pristine().read_txn()?;
        if let Some(id) = txn.get_internal(change)? {
            for dependent in txn.get_rev_deps(id)? {
                if txn.get_node_type(dependent)? == Some(node_type::ATTESTATION) {
                    hashes.insert(
                        txn.get_external(dependent)?
                            .ok_or_else(|| refused("attestation registration has no hash"))?,
                    );
                }
            }
        }
        drop(txn);
        for hash in self.change_store().iter_attestations() {
            hashes.insert(hash.map_err(refused)?);
            if hashes.len() > 16384 {
                return Err(refused("attestation inventory exceeds review budget"));
            }
        }
        let mut result = Vec::new();
        for hash in hashes {
            let attestation = self.load_attestation(&hash)?;
            if attestation.changes_covered.contains(change) {
                result.push((hash, attestation));
            }
        }
        Ok(result)
    }
}

#[cfg(feature = "review-shacl")]
fn key(bytes: [u8; 32]) -> Result<atomic_identity::keypair::PublicKey, RepositoryError> {
    atomic_identity::keypair::PublicKey::from_bytes(&bytes).map_err(refused)
}

#[cfg(all(test, not(feature = "review-shacl")))]
mod unsupported {
    use super::*;
    #[test]
    fn publication_review_requires_the_full_policy_engine() {
        use atomic_canonical::publication_review::{ChangePin, IntentPin, ReviewSubject};
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let change = Hash::of(b"unsupported-engine").to_base32();
        let pin = IntentPin {
            id: "untrusted-input".into(),
            source_hash: String::new(),
            signed_hash: String::new(),
            public_key: String::new(),
        };
        let review = PublicationReview {
            schema: atomic_canonical::publication_review::LEGACY_SCHEMA.into(),
            reviewed_at: chrono::Utc::now().to_rfc3339(),
            subject: ReviewSubject {
                project: repo.project_id().unwrap().to_urn(),
                roots: vec![change.clone()],
                changes: vec![ChangePin {
                    hash: change.clone(),
                    session: Some("legacy".into()),
                    governing_intents: vec![],
                    provenance: vec![],
                    observations: vec![],
                }],
                review: pin,
                work: vec![],
                set_aside: vec![],
                profile_hash: profile_hash(),
                view_chain: vec![],
                effective_set_id: String::new(),
                snapshot: None,
            },
            supersedes: vec![],
            attributed_to: "untrusted-input".into(),
            content_hash: None,
            proof: None,
        };
        repo.vault_store(
            &format!("{PREFIX}{}/review.md", review.artifact_hash()),
            atomic_core::pristine::VaultEntryType::Attestation,
            serde_json::to_vec(&review).unwrap(),
            "{}".into(),
        )
        .unwrap();
        let config = PublicationGateConfig {
            trust: Default::default(),
            repository_identity: None,
        };
        let result = repo.retrospective_review_for(&change, "legacy", &config, None);
        assert!(repo
            .workflow_snapshot_with_review(&serde_json::to_vec(&review).unwrap())
            .unwrap_err()
            .to_string()
            .contains("review-shacl feature is required"));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("review-shacl feature is required"));
    }
}
