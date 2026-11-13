use super::*;
use crate::intent_bridge as bridge;
use crate::{IntentCreateOptions, IntentUpdateOptions};
use atomic_canonical::publication_review::PublicationReview;
use atomic_core::{
    change::{
        attestation::{AttestAgent, Attestation},
        provenance_graph::ProvenanceGraphBuilder,
        AITool, AIVendor, Change, ChangeHeader, Provenance,
    },
    pristine::VaultEntryType,
};
use atomic_identity::{Identity, KeyPair};
use serde_json::json;
mod boundaries;
mod observations;
mod snapshot;

const SESSION: &str = "retained-legacy-session";

struct Fixture {
    _dir: tempfile::TempDir,
    repo: Repository,
    change: Hash,
    author: Identity,
    author_key: KeyPair,
    reviewer: Identity,
    reviewer_key: KeyPair,
    work: String,
    review: String,
}

fn signed_intent(
    repo: &Repository,
    signer: &Identity,
    pair: &KeyPair,
    kind: &str,
    target: Option<&str>,
) -> String {
    let created = repo
        .vault_intent_create(IntentCreateOptions {
            title: format!("{kind} fixture"),
            priority: None,
            assignee: None,
            labels: vec![],
            session_id: None,
            turn_id: None,
            kind: Some(kind.into()),
        })
        .unwrap();
    let uid = &created.uid;
    let observed = repo
        .get_view_info(repo.current_view())
        .unwrap()
        .state_base32();
    let reference = target
        // Full source-context identity is independent of the review document,
        // avoiding a circular pin while observing inherited membership.
        .map(|target| format!(":::ref{{to=urn:atomic:intent:{target} edge=reviews}}\n:::\n"))
        .unwrap_or_default();
    let set_id = repo.view_set_id(repo.current_view()).unwrap().to_base32();
    let body = format!(":::why\nVerify exact retained work.\n:::\n\
        :::acceptance-criterion{{#{uid}-ac-1 status=met verifiedBy=fixture evidence=inspected requiredKinds=manual}}\n\
        Exact fixture bytes and provenance have been inspected.\n\
        ::verification{{kind=manual outcome=pass scope=ac observedAtMerkle={observed} ref=urn:atomic:view-set:{set_id} observation=checked}}\n:::\n\
        :::task{{#{uid}-1 status=done criteria={uid}-ac-1}}\nInspect retained originals.\n:::\n\
        {reference}:::scope-in\nFixture evidence.\n:::\n\
        :::scope-out\nLive repositories.\n:::\n:::constraint\nDo not invent original signatures.\n:::\n");
    repo.vault_intent_update(
        &created.id,
        IntentUpdateOptions {
            status: Some("done".into()),
            content: Some(body),
            force: true,
            ..Default::default()
        },
    )
    .unwrap();
    sign_intent(repo, &created.uid, signer, pair);
    created.uid
}

fn receipt_node(work: &str) -> atomic_core::change::provenance_graph::ProvenanceNode {
    receipt_node_for(&[work])
}

/// One retained receipt node attesting every listed intent in a single
/// multi-command invocation, exactly as a session transcript would retain it.
fn receipt_node_for(intents: &[&str]) -> atomic_core::change::provenance_graph::ProvenanceNode {
    let command = intents
        .iter()
        .map(|id| format!("atomic intent attest {id}"))
        .collect::<Vec<_>>()
        .join("; ");
    let output = intents
        .iter()
        .map(|id| format!("Attested intent: {id}\n"))
        .collect::<String>();
    serde_json::from_value(json!({
        "id":"fixture-receipt", "kind":"execution", "timestamp":0,
        "summary":"Retained lifecycle receipt", "classified":false,
        "detail":json!({"command":command,"output_summary":output}).to_string()
    }))
    .unwrap()
}

fn sign_intent(repo: &Repository, id: &str, signer: &Identity, pair: &KeyPair) {
    let inputs = bridge::read_intent(repo, id).unwrap();
    let node = atomic_canonical::proof::attest(bridge::lift(&inputs).unwrap(), signer, pair);
    let path = bridge::attestation_vault_path(repo, id).unwrap();
    repo.vault_store(
        &path,
        VaultEntryType::Attestation,
        format!("{}\n", serde_json::to_string_pretty(&node).unwrap()).into_bytes(),
        json!({"sourceContentHash":bridge::source_content_hash(&inputs)}).to_string(),
    )
    .unwrap();
}

impl Fixture {
    fn new() -> Self {
        Self::build(false).0
    }

    /// Like `new`, but the retained receipt also observes a second, independent
    /// work intent by the same author. Returns that intent's UID.
    fn with_second_observed_intent() -> (Self, String) {
        let (fixture, second) = Self::build(true);
        (fixture, second.expect("second observed intent"))
    }

    fn build(second_observed: bool) -> (Self, Option<String>) {
        let dir = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        repo.create_draft_view("review-source", "dev").unwrap();
        repo.switch_view(repo.require_working_copy_id().unwrap(), "review-source")
            .unwrap();
        let author_key = KeyPair::generate();
        let author = Identity::new("original-author", &author_key);
        let reviewer_key = KeyPair::generate();
        let reviewer = Identity::new("present-reviewer", &reviewer_key);
        let store =
            atomic_identity::IdentityStore::open(&repo.dot_dir().join("identities")).unwrap();
        store.save(&author).unwrap();
        store.save(&reviewer).unwrap();
        let work = signed_intent(&repo, &author, &author_key, "feature", None);
        let second =
            second_observed.then(|| signed_intent(&repo, &author, &author_key, "feature", None));
        let dependency = repo
            .save_change(&Change::empty(ChangeHeader::new("unmanaged dependency")))
            .unwrap();
        let mut change = Change::empty(ChangeHeader::new("legacy managed original"));
        change.hashed.dependencies.push(dependency);
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
        let change = repo.save_change(&change).unwrap();
        let graph = ProvenanceGraphBuilder::new(SESSION, "fixture")
            .add_node(receipt_node_for(
                &std::iter::once(work.as_str())
                    .chain(second.as_deref())
                    .collect::<Vec<_>>(),
            ))
            .changes_explained(vec![change])
            .build();
        repo.save_provenance_graph(&graph).unwrap();
        repo.insert_change_rec(
            &change,
            crate::InsertOptions::default().view("review-source"),
        )
        .unwrap();
        let review = signed_intent(&repo, &reviewer, &reviewer_key, "review", Some(&work));
        (
            Self {
                _dir: dir,
                repo,
                change,
                author,
                author_key,
                reviewer,
                reviewer_key,
                work,
                review,
            },
            second,
        )
    }
    fn keys(&self, did: &str) -> Option<[u8; 32]> {
        [&self.author, &self.reviewer]
            .into_iter()
            .find(|identity| {
                atomic_canonical::did::did_matches_public_key(did, &identity.public_key)
            })
            .map(|identity| *identity.public_key.as_bytes())
    }
    fn config(&self) -> PublicationGateConfig {
        PublicationGateConfig {
            trust: atomic_config::GitTrustConfig {
                signers: vec![atomic_canonical::did::did_for_public_key(
                    &self.reviewer.public_key,
                )],
                revoked: vec![],
            },
            repository_identity: None,
        }
    }
    fn signed(&self) -> PublicationReview {
        let prepared = self
            .repo
            .prepare_publication_review(&self.review, &[self.change], &|did| self.keys(did))
            .unwrap();
        self.sign(prepared)
    }
    fn sign(&self, mut review: PublicationReview) -> PublicationReview {
        review.proof = None;
        review.content_hash = None;
        serde_json::from_value(atomic_canonical::proof::attest_value(
            review.value(),
            &self.reviewer,
            &self.reviewer_key,
        ))
        .unwrap()
    }
    fn conforms(&self, review: &PublicationReview, config: &PublicationGateConfig) -> bool {
        self.repo
            .validate_publication_review(review, config, &|did| self.keys(did))
            .map(|r| r.conforms)
            .unwrap_or(false)
    }
    fn verdict(&self) -> super::super::provenance_gate::GateVerdict {
        self.repo
            .evaluate_publication_gate(
                &[self.change],
                &self.config(),
                None,
                Some(&|did| self.keys(did)),
            )
            .unwrap()
    }
}

#[test]
fn exact_signed_shacl_review_discharges_only_missing_attestation() {
    let fixture = Fixture::new();
    assert!(!fixture.verdict().allowed());
    let review = fixture.signed();
    let report = fixture
        .repo
        .validate_publication_review(&review, &fixture.config(), &|did| fixture.keys(did))
        .unwrap();
    assert!(report.conforms, "{:#?}", report.results);
    assert!(report
        .target_coverage
        .iter()
        .any(|c| c.evaluated > 0 && c.shape.ends_with("Evaluation")));
    assert_eq!(
        review.subject.changes.len(),
        2,
        "full dependency closure is pinned"
    );
    let mut original = Vec::new();
    fixture
        .repo
        .load_change(&fixture.change)
        .unwrap()
        .serialize(&mut original)
        .unwrap();
    let path = fixture
        .repo
        .record_publication_review(&review, &fixture.config(), &|did| fixture.keys(did))
        .unwrap();
    let verdict = fixture.verdict();
    assert!(verdict.allowed(), "{}", verdict.refusal_report());
    assert_eq!(verdict.retrospective_reviews.len(), 1);
    let mut after = Vec::new();
    fixture
        .repo
        .load_change(&fixture.change)
        .unwrap()
        .serialize(&mut after)
        .unwrap();
    assert_eq!(after, original);
    assert!(fixture
        .repo
        .find_attestations_for_change(&fixture.change)
        .unwrap()
        .is_empty());
    assert_eq!(
        fixture
            .repo
            .record_publication_review(&review, &fixture.config(), &|did| fixture.keys(did))
            .unwrap(),
        path
    );
}

#[test]
fn stale_wrong_scope_and_truncated_subjects_fail_even_when_resigned() {
    let f = Fixture::new();
    let original = f.signed();
    let mut variants = vec![];
    let mut r = original.clone();
    r.subject.project = "urn:atomic:project:another".into();
    variants.push(r);
    let mut r = original.clone();
    r.subject.changes[0].session = Some("other-session".into());
    variants.push(r);
    let mut r = original.clone();
    r.subject.changes.pop();
    variants.push(r);
    let mut r = original.clone();
    r.subject.work.clear();
    variants.push(r);
    let mut r = original.clone();
    r.subject.profile_hash = "old-profile".into();
    variants.push(r);
    for review in variants {
        assert!(!f.conforms(&f.sign(review), &f.config()));
    }
}

#[test]
fn signature_trust_and_revocation_are_not_self_asserted() {
    let f = Fixture::new();
    let signed = f.signed();
    let mut changed = signed.clone();
    changed.reviewed_at = "2026-01-01T00:00:00Z".into();
    assert!(!f.conforms(&changed, &f.config()));
    let mut config = f.config();
    config.trust.signers.clear();
    assert!(!f.conforms(&signed, &config));
    config = f.config();
    config.trust.revoked = config.trust.signers.clone();
    assert!(!f.conforms(&signed, &config));
    assert!(!f
        .repo
        .validate_publication_review(&signed, &f.config(), &|_| None)
        .is_ok());
}

#[test]
fn self_review_with_the_work_authors_key_is_rejected() {
    let mut f = Fixture::new();
    f.review = signed_intent(&f.repo, &f.author, &f.author_key, "review", Some(&f.work));
    let prepared = f
        .repo
        .prepare_publication_review(&f.review, &[f.change], &|did| f.keys(did))
        .unwrap();
    let signed: PublicationReview = serde_json::from_value(atomic_canonical::proof::attest_value(
        prepared.value(),
        &f.author,
        &f.author_key,
    ))
    .unwrap();
    let mut config = f.config();
    config
        .trust
        .signers
        .push(atomic_canonical::did::did_for_public_key(
            &f.author.public_key,
        ));
    assert!(!f.conforms(&signed, &config));
}

#[test]
fn later_intent_edit_and_forged_freshness_anchor_are_rejected() {
    let f = Fixture::new();
    let signed = f.signed();
    f.repo
        .vault_intent_update(
            &f.work,
            IntentUpdateOptions {
                title: Some("changed meaning".into()),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!f.conforms(&signed, &f.config()));
    // Forge the unprotected sidecar anchor while retaining the old valid signature.
    let inputs = bridge::read_intent(&f.repo, &f.work).unwrap();
    let path = bridge::attestation_vault_path(&f.repo, &f.work).unwrap();
    let entry = f.repo.vault_retrieve(&path).unwrap().unwrap();
    f.repo
        .vault_store(
            &path,
            VaultEntryType::Attestation,
            entry.content_bytes,
            json!({"sourceContentHash":bridge::source_content_hash(&inputs)}).to_string(),
        )
        .unwrap();
    assert!(!f.conforms(&signed, &f.config()));
}

#[test]
fn incomplete_review_and_missing_required_verification_are_rejected() {
    for replacement in ["status=unmet", "status=met"] {
        let f = Fixture::new();
        let inputs = bridge::read_intent(&f.repo, &f.review).unwrap();
        let body = if replacement == "status=unmet" {
            inputs.body.replace("status=met", replacement)
        } else {
            inputs
                .body
                .lines()
                .filter(|line| !line.starts_with("::verification"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // The normal lifecycle refuses this edit. Model a signed but invalid
        // artifact arriving through the low-level vault/transport boundary.
        let path = f
            .repo
            .vault_list("intents/", Some(VaultEntryType::Intent))
            .unwrap()
            .into_iter()
            .find(|meta| {
                let entry = f.repo.vault_retrieve(&meta.path).unwrap().unwrap();
                serde_json::from_str::<serde_json::Value>(&entry.frontmatter_json).unwrap()["uid"]
                    == f.review
            })
            .unwrap()
            .path;
        f.repo
            .vault_store(
                &path,
                VaultEntryType::Intent,
                body.into_bytes(),
                serde_json::to_string(&inputs.frontmatter).unwrap(),
            )
            .unwrap();
        sign_intent(&f.repo, &f.review, &f.reviewer, &f.reviewer_key);
        assert!(!f.conforms(&f.signed(), &f.config()));
    }
}

#[test]
fn bad_original_attestation_is_not_replaced_by_a_good_review() {
    let f = Fixture::new();
    let review = f.signed();
    f.repo
        .record_publication_review(&review, &f.config(), &|did| f.keys(did))
        .unwrap();
    let mut original =
        Attestation::builder(SESSION, AttestAgent::new("fixture", "fixture", "fixture"))
            .changes_covered(vec![f.change])
            .build();
    let did = atomic_canonical::did::did_for_public_key(&f.reviewer.public_key);
    original.sign_with_did(&did, f.reviewer_key.secret.as_bytes());
    original.changes_covered.push(Hash::of(b"tampered"));
    f.repo.save_attestation(&original).unwrap();
    assert!(!f.verdict().allowed());
    assert!(f.verdict().retrospective_reviews.is_empty());
}

#[test]
fn malformed_registered_reviews_fail_closed_and_duplicate_json_is_refused() {
    let f = Fixture::new();
    f.repo
        .vault_store(
            "publication-reviews/broken/review.md",
            VaultEntryType::Attestation,
            b"{}".to_vec(),
            "{}".into(),
        )
        .unwrap();
    assert!(!f.verdict().allowed());
    assert!(PublicationReview::parse(br#"{"schema":"x","schema":"y"}"#).is_err());
}
