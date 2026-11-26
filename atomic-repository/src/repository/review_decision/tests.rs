use super::*;
use crate::{
    record::RecordOptions, tracking::TrackingOptions, CrossViewInsertOptions, InsertOptions,
};
use atomic_canonical::{
    review_decision::{self, ReviewEvaluation},
    review_graph::ReviewPass,
};
use atomic_core::{change::ChangeHeader, pristine::OperationTxnT};
use atomic_identity::{Identity, KeyPair};
use serde_json::json;

struct Fixture {
    _dir: tempfile::TempDir,
    repo: Repository,
    signer: Identity,
    pair: KeyPair,
    base: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let base = repo.current_view().to_owned();
        std::fs::write(dir.path().join("lib.rs"), "pub fn original() {}\n").unwrap();
        repo.add(
            repo.require_working_copy_id().unwrap(),
            "lib.rs",
            TrackingOptions::default(),
        )
        .unwrap();
        record(&repo);
        repo.create_draft_view("draft", &base).unwrap();
        repo.switch_view(repo.require_working_copy_id().unwrap(), "draft")
            .unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn changed() { todo!(); }\n").unwrap();
        record(&repo);
        let pair = KeyPair::generate();
        let signer = Identity::new("accountable-reviewer", &pair);
        let mut value = Self {
            _dir: dir,
            repo,
            signer,
            pair,
            base,
        };
        value.trust(false);
        value
    }
    fn trust(&mut self, revoked: bool) {
        let path = self.repo.dot_dir().join("config.toml");
        let mut config = atomic_config::RepoConfig::load(&path).unwrap();
        let did = atomic_canonical::did::did_for_public_key(&self.signer.public_key);
        config.git.trust.signers = vec![did.clone()];
        config.git.trust.revoked = if revoked { vec![did] } else { vec![] };
        config.save(&path).unwrap();
    }
    fn report(&self) -> Value {
        let source = self.repo.review_decision_view_pin("draft").unwrap();
        let target = self.repo.review_decision_view_pin(&self.base).unwrap();
        let scope = intent_bridge::view_intent_sources(&self.repo, "draft")
            .unwrap()
            .hash;
        let pin = |value: &ViewPin| json!({"view":value.view,"state":value.state,"membership_hash":value.membership_hash});
        json!({"schema_version":5,"selection":{"kind":"view_comparison"},"view_comparison":{"from":pin(&source),"to":pin(&target),"before":pin(&target),"after":pin(&source),"conformance":{"scope":{"view":"draft","current_inventory_hash":scope},"criteria":[],"diagnostics":[]}},"graph":{"scope":{"project":self.repo.project_id().unwrap().to_urn(),"view":"draft","view_state":source.state},"findings":[{"number":1,"id":"finding:placeholder","passes":["anti_slop"],"finding":{"summary":"Unfinished placeholder remains"},"comparison":{"status":"new"}}],"criteria":[],"nodes":[],"passes":ReviewPass::ALL.map(|pass|json!({"pass":pass,"status":"partial","detail":"Static inspection has explicit limitations"})),"risk":{"assessment":{"rating":"medium","coverage":"partial","rationale":"User must evaluate the changed implementation","limitations":["No runtime tests executed by review"]}}},"publication_gate":{"allowed":true,"blocks":[]}})
    }
    fn decision(&self, not_ready: bool, supersedes: Vec<String>) -> ReviewDecision {
        let report = self.report();
        let scope = self
            .repo
            .review_decision_scope("draft", &self.base, &report)
            .unwrap();
        let issues = review_decision::issues(&report).unwrap();
        let mut evaluation = ReviewEvaluation::template(&scope, &report, &issues);
        for pass in &mut evaluation.passes {
            pass.evaluated = true;
            pass.rationale =
                "Responsible user inspected this complete pass and its source/coverage".into();
            for issue in &mut pass.accepted_degradations {
                issue.rationale="Explicitly accepted technical debt for this exact reviewed scope; tracked follow-up".into();
            }
        }
        if not_ready {
            evaluation.passes[0].disposition = Disposition::NotReady;
            evaluation.passes[0].accepted_degradations.clear();
        }
        let decision = ReviewDecision {
            schema: review_decision::SCHEMA.into(),
            reviewed_at: "2026-11-25T00:00:00Z".into(),
            scope,
            report_hash: review_decision::report_hash(&report),
            report,
            issues,
            readiness: Disposition::NotReady,
            evaluation,
            independent_approval: false,
            attributed_to: atomic_canonical::did::did_for_public_key(&self.signer.public_key),
            public_key: self.signer.public_key.to_base32(),
            supersedes,
            content_hash: None,
            proof: None,
        };
        serde_json::from_value(atomic_canonical::proof::attest_value(
            decision.value(),
            &self.signer,
            &self.pair,
        ))
        .unwrap()
    }
    fn insert(&self) -> Result<crate::CrossViewInsertOutcome, RepositoryError> {
        self.repo
            .insert_from_view(CrossViewInsertOptions::new("draft", &self.base))
    }
}
fn record(repo: &Repository) -> Hash {
    *repo
        .record(
            repo.require_working_copy_id().unwrap(),
            ChangeHeader::new("native review fixture"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
                .enrich_kg(false),
        )
        .unwrap()
        .hash()
}

#[test]
fn accepted_degradation_inserts_and_links_the_attested_unready_report_to_operations() {
    let f = Fixture::new();
    let decision = f.decision(false, vec![]);
    let evidence = f.repo.record_review_decision(&decision).unwrap();
    assert_eq!(evidence.readiness, Disposition::NotReady);
    assert_eq!(evidence.disposition, Disposition::AcceptedDegradation);
    let outcome = f.insert().unwrap();
    assert!(outcome.changes_applied > 0);
    assert_eq!(outcome.review_decision.unwrap().decision, evidence.decision);
    assert!(String::from_utf8(
        f.repo
            .get_file_content_on_view("lib.rs", &f.base)
            .unwrap()
            .unwrap()
    )
    .unwrap()
    .contains("todo!()"));
    let hash = Hash::from_base32(evidence.decision.as_bytes()).unwrap();
    let operations = f
        .repo
        .pristine()
        .read_txn()
        .unwrap()
        .list_operations()
        .unwrap();
    assert!(operations.iter().any(|operation| operation.payload().kind
        == atomic_core::operation::OperationKind::Insert
        && operation.payload().evidence.contains(&hash)));
    let retained = f.repo.native_review_decisions().unwrap();
    assert_eq!(retained[0], decision);
    assert!(!retained[0].independent_approval);
    assert!(f.repo.inspect_review_decision(&retained[0]).is_err(),"Historical report stays retained but its old target-before state no longer authorizes a new insertion");
}

#[test]
fn explicitly_not_ready_refuses_view_and_direct_insertion_before_mutation() {
    let f = Fixture::new();
    let decision = f.decision(true, vec![]);
    f.repo.record_review_decision(&decision).unwrap();
    let state = f.repo.get_view_info(&f.base).unwrap().state;
    assert!(f.insert().unwrap_err().to_string().contains("not_ready"));
    let hash = f
        .repo
        .get_view_changes(Some("draft"))
        .unwrap()
        .last()
        .unwrap()
        .1;
    assert!(f
        .repo
        .insert_change_rec(&hash, InsertOptions::default().view(&f.base))
        .is_err());
    assert!(f.repo.cherry_pick(&[hash], "", Some(&f.base)).is_err());
    assert!(
        f.repo
            .insert_from_view(CrossViewInsertOptions::new("", &f.base).only_changes(vec![hash]))
            .is_err(),
        "An omitted source label must not bypass the native decision"
    );
    assert_eq!(f.repo.get_view_info(&f.base).unwrap().state, state);
}

#[test]
fn hash_set_insertion_reuses_the_exact_registered_evaluation() {
    let f = Fixture::new();
    let decision = f.decision(false, vec![]);
    f.repo.record_review_decision(&decision).unwrap();
    let hash = f
        .repo
        .get_view_changes(Some("draft"))
        .unwrap()
        .last()
        .unwrap()
        .1;
    let outcome = f.repo.cherry_pick(&[hash], "", Some(&f.base)).unwrap();
    assert_eq!(
        outcome.review_decision.unwrap().decision,
        decision.artifact_hash()
    );
}

#[test]
fn moving_either_endpoint_or_current_intent_context_requires_re_evaluation() {
    for change in ["source", "target", "intent"] {
        let mut f = Fixture::new();
        let decision = f.decision(false, vec![]);
        f.repo.record_review_decision(&decision).unwrap();
        match change {
            "source" => {
                std::fs::write(f.repo.root().join("lib.rs"), "pub fn later_source() {}\n").unwrap();
                record(&f.repo);
            }
            "target" => {
                let wc = f.repo.require_working_copy_id().unwrap();
                f.repo.switch_view(wc, &f.base).unwrap();
                std::fs::write(f.repo.root().join("lib.rs"), "pub fn later_target() {}\n").unwrap();
                record(&f.repo);
            }
            _ => {
                f.repo
                    .vault_intent_create(crate::IntentCreateOptions {
                        title: "New context after review".into(),
                        priority: None,
                        assignee: None,
                        labels: vec![],
                        session_id: None,
                        turn_id: None,
                        kind: None,
                    })
                    .unwrap();
            }
        }
        let state = f.repo.get_view_info(&f.base).unwrap().state;
        assert!(f.insert().is_err(), "{change}");
        assert_eq!(f.repo.get_view_info(&f.base).unwrap().state, state);
    }
}

#[test]
fn signatures_trust_and_full_evaluation_are_not_waivable() {
    let mut f = Fixture::new();
    let good = f.decision(false, vec![]);
    let mut tampered = good.clone();
    tampered.evaluation.passes[0].rationale = "tampered after signing".into();
    assert!(f.repo.record_review_decision(&tampered).is_err());
    let mut omitted = good.clone();
    omitted.evaluation.passes[0].accepted_degradations.clear();
    assert!(f.repo.record_review_decision(&omitted).is_err());
    let mut fake = good.clone();
    fake.attributed_to = "did:atomic:someone-else".into();
    assert!(f.repo.record_review_decision(&fake).is_err());
    f.repo.record_review_decision(&good).unwrap();
    f.trust(true);
    assert!(f.insert().is_err());
}

#[test]
fn decisions_are_immutable_and_replacing_a_current_head_is_explicit() {
    let f = Fixture::new();
    let denied = f.decision(true, vec![]);
    let first = f.repo.record_review_decision(&denied).unwrap();
    let implicit = f.decision(false, vec![]);
    assert!(f.repo.record_review_decision(&implicit).is_err());
    let approved = f.decision(false, vec![first.decision.clone()]);
    f.repo.record_review_decision(&approved).unwrap();
    assert!(f.repo.inspect_review_decision(&denied).is_err());
    assert!(f.insert().is_ok());
    let path = format!("{PREFIX}{}/review.md", first.decision);
    assert!(f
        .repo
        .vault_store(
            &path,
            atomic_core::pristine::VaultEntryType::Attestation,
            b"replacement".to_vec(),
            "{}".into()
        )
        .is_err());
    assert_eq!(f.repo.native_review_decisions().unwrap().len(), 2);
}

#[test]
fn unreviewed_existing_paths_keep_their_prior_policy_and_foreign_scope_is_refused() {
    let f = Fixture::new();
    assert!(f.insert().is_ok());
    let f = Fixture::new();
    let mut decision = f.decision(false, vec![]);
    decision.scope.project = "urn:atomic:project:foreign".into();
    assert!(f.repo.record_review_decision(&decision).is_err());
}
