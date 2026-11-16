use super::*;
use std::cell::Cell;

/// One more managed original from the fixture session, explained by the same
/// retained receipt as the fixture's own change.
fn managed_original(f: &Fixture, message: &str) -> Hash {
    let mut change = Change::empty(ChangeHeader::new(message));
    change.hashed.dependencies.push(f.change);
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
    let change = f.repo.save_change(&change).unwrap();
    let graph = ProvenanceGraphBuilder::new(SESSION, "fixture")
        .add_node(receipt_node(&f.work))
        .changes_explained(vec![change])
        .build();
    f.repo.save_provenance_graph(&graph).unwrap();
    f.repo
        .insert_change_rec(
            &change,
            crate::InsertOptions::default().view("review-source"),
        )
        .unwrap();
    change
}

/// Three managed originals of one session, all covered by one recorded review.
fn three_changes_under_one_review(f: &Fixture) -> (Vec<Hash>, String) {
    let changes = vec![
        f.change,
        managed_original(f, "second managed original"),
        managed_original(f, "third managed original"),
    ];
    // The review intent has to observe the view once all three are in it.
    let review = signed_intent(
        &f.repo,
        &f.reviewer,
        &f.reviewer_key,
        "review",
        Some(&f.work),
    );
    let prepared = f
        .repo
        .prepare_publication_review(&review, &changes, &|did| f.keys(did))
        .unwrap();
    let signed = f.sign(prepared);
    f.repo
        .record_publication_review(&signed, &f.config(), &|did| f.keys(did))
        .unwrap();
    (changes, signed.artifact_hash())
}

fn gate(
    f: &Fixture,
    changes: &[Hash],
    config: &PublicationGateConfig,
) -> super::super::super::provenance_gate::GateVerdict {
    f.repo
        .evaluate_publication_gate(changes, config, None, Some(&|did| f.keys(did)))
        .unwrap()
}

/// Nothing the gate learns about a stored review depends on which of its
/// changes asks, so a batch must not authenticate and validate it per change.
/// Key lookups are the work a caller can see: every signature the review
/// resolver checks goes through the injected provider.
#[test]
fn gate_resolves_a_stored_review_once_for_every_change_it_covers() {
    let f = Fixture::new();
    let (changes, review) = three_changes_under_one_review(&f);
    let lookups = Cell::new(0usize);
    let counted = |did: &str| {
        lookups.set(lookups.get() + 1);
        f.keys(did)
    };
    let run = |changes: &[Hash]| {
        lookups.set(0);
        let verdict = f
            .repo
            .evaluate_publication_gate(changes, &f.config(), None, Some(&counted))
            .unwrap();
        (verdict, lookups.get())
    };

    let (one, lookups_for_one) = run(&changes[..1]);
    assert!(one.allowed(), "{}", one.refusal_report());
    assert!(lookups_for_one > 0, "the review must have been resolved");

    let (all, lookups_for_all) = run(&changes);
    assert!(all.allowed(), "{}", all.refusal_report());
    assert_eq!(all.retrospective_reviews.len(), 3);
    assert!(all
        .retrospective_reviews
        .iter()
        .all(|approval| approval.review_hash == review));
    assert_eq!(
        lookups_for_all, lookups_for_one,
        "three changes under one review must cost what one does"
    );
}

/// Reusing an outcome must not turn one refusal into one diagnosed change and
/// two silent ones: each covered change is blocked and says why.
#[test]
fn a_stored_review_that_stopped_conforming_blocks_every_change_it_covers() {
    let f = Fixture::new();
    let (changes, _) = three_changes_under_one_review(&f);
    let mut revoked = f.config();
    revoked.trust.revoked = revoked.trust.signers.clone();

    let verdict = gate(&f, &changes, &revoked);
    assert!(!verdict.allowed());
    assert!(verdict.retrospective_reviews.is_empty());
    assert_eq!(verdict.blocks.len(), 3, "{}", verdict.refusal_report());
    assert_eq!(
        verdict
            .limitations
            .iter()
            .filter(|limitation| limitation.contains("stored review fails SHACL"))
            .count(),
        3
    );
}

/// The same holds when the inventory itself cannot be read.
#[test]
fn an_unreadable_review_inventory_blocks_every_managed_change() {
    let f = Fixture::new();
    let (changes, _) = three_changes_under_one_review(&f);
    f.repo
        .vault_store(
            &format!("{PREFIX}not-a-review/review.md"),
            VaultEntryType::Attestation,
            b"{}".to_vec(),
            "{}".into(),
        )
        .unwrap();
    let unreadable = f.repo.publication_reviews().unwrap_err().to_string();

    let verdict = gate(&f, &changes, &f.config());
    assert!(!verdict.allowed());
    assert_eq!(verdict.blocks.len(), 3, "{}", verdict.refusal_report());
    assert_eq!(
        verdict
            .limitations
            .iter()
            .filter(|limitation| **limitation == unreadable)
            .count(),
        3
    );
}
